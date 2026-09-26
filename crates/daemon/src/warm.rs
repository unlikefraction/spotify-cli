//! Keeps one headless `spotify_player` instance running so CLI calls take ~20 ms instead of ~1.5 s.
//!
//! spotify_player's CLI forwards each command over UDP (`client_port`, default 8080) to the one
//! running instance that holds that port, or starts a throwaway client when none answers. The
//! Homebrew build has no daemon mode, so the daemon runs the normal terminal app on a
//! pseudo-terminal it owns, answering the terminal's startup queries (cursor position, device
//! attributes, window size) the way a real terminal would; without those answers the TUI refuses
//! to start ("cursor position could not be read"). Streaming, media keys and desktop
//! notifications are switched off: the instance never becomes a playback device, it only answers
//! Web API commands.
//!
//! Exactly one copy belongs to the daemon, and it is only reported `running` once it holds the
//! client port (spotify_player 0.25 does not exit when the port is taken: it logs a warning and
//! idles, so the copy that holds the port answers every command):
//! - Before each start, copies left by earlier daemons are stopped: the pid recorded in
//!   `warm-player.json` (matched by start time) and any orphaned (parent is launchd) process of
//!   this user that carries the daemon's exact `-o` overrides. A spotify_player you run yourself
//!   (without exactly those overrides) is never touched.
//! - When another spotify_player (usually your own) holds the port, the daemon starts none and
//!   reports `deferred` with its pid; that copy answers spotify-cli's commands.
//! - After starting, the daemon waits until its copy holds the port (`lsof`), then reports
//!   `running`; a copy that never gets it is stopped and restarted.
//! - The copy dies with the daemon, even on SIGKILL: it leads its own session on the
//!   pseudo-terminal, whose only master descriptor is the daemon's (close-on-exec), so the
//!   daemon's exit hangs the terminal up and the kernel sends it SIGHUP. A normal shutdown also
//!   stops its process group ([`stop`]); anything that still survives is stopped by the next
//!   daemon's first start.
//! - Its playback state is refreshed every [`REFRESH_MS`] (see there).

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silicon_spotify_client::player::SpotifyPlayer;
use tokio::process::Command;

use crate::log;
use crate::service::Daemon;

/// spotify_player's default `client_port`.
pub const DEFAULT_CLIENT_PORT: u16 = 8080;

/// How often the warm copy re-reads playback from the Web API (`GET /v1/me/player`), passed as
/// `-o playback_refresh_duration_in_ms`.
///
/// spotify_player answers `get key playback`, `like`, relative seeks and play/pause toggles from
/// its in-memory state, which with its default (0) changes only on its own events, so it goes
/// stale as soon as someone uses Spotify.app. 3 s bounds that staleness at one small GET every
/// 3 s: 20 a minute, 10 per 30-second rolling window (the window Spotify rate-limits on).
/// Spotify does not publish its limits; that is a small, steady load (spotify_player's other
/// background requests, such as the queue, are throttled to one per 5 s), well under what gets
/// clients 429s even with a development-mode client ID, and leaves the quota to real commands.
/// A smaller positive value in the user's own `app.toml` is kept.
pub const REFRESH_MS: u64 = 3_000;

/// The overrides that mark a daemon's warm copy (every daemon version passed exactly these, in
/// this order, and nothing else passes them).
const SIGNATURE: [&str; 6] = [
    "-o",
    "enable_streaming=Never",
    "-o",
    "enable_media_control=false",
    "-o",
    "enable_notify=false",
];

/// spotify_player subcommands: a process running one is a short-lived CLI call (which holds the
/// client port for a second when no instance runs), not a long-running instance.
const SUBCOMMANDS: [&str; 10] = [
    "get",
    "playback",
    "connect",
    "like",
    "authenticate",
    "playlist",
    "generate",
    "search",
    "lyrics",
    "features",
];

/// A fresh copy must hold the port within this long (it binds right after signing in).
const CONFIRM_WITHIN: Duration = Duration::from_secs(30);
/// While running, re-check that the copy still holds the port this often.
const RECHECK_EVERY: Duration = Duration::from_secs(300);
/// While another instance holds the port, look again this often.
const DEFERRED_RECHECK: Duration = Duration::from_secs(30);

fn set(daemon: &Daemon, value: Value) {
    *daemon
        .warm
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

/// Opens a pseudo-terminal pair sized 40×120, both ends close-on-exec (only the child's stdio
/// dups of the slave cross `exec`, so the daemon holds the only master).
#[allow(unsafe_code)]
fn open_pty() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = libc::winsize {
        ws_row: 40,
        ws_col: 120,
        ws_xpixel: 1200,
        ws_ypixel: 800,
    };
    // SAFETY: openpty writes two valid descriptors on success; name and termios may be null.
    let status = unsafe {
        libc::openpty(
            &raw mut master,
            &raw mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &raw mut size,
        )
    };
    if status != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: both descriptors are fresh and owned exclusively here.
    let pair = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    close_on_exec(&pair.0)?;
    close_on_exec(&pair.1)?;
    Ok(pair)
}

#[allow(unsafe_code)]
fn close_on_exec(fd: &OwnedFd) -> std::io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
    // SAFETY: as above.
    if flags == -1
        || unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } == -1
    {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// Runs between fork and exec: a new session with the pseudo-terminal (fd 0) as its
/// controlling terminal, so hanging the terminal up (the daemon exiting) sends it SIGHUP, and
/// SIGHUP's default action (a daemon started from a `nohup` shell would pass on "ignore").
#[allow(unsafe_code)]
fn lead_session_on_stdin() -> std::io::Result<()> {
    // SAFETY: async-signal-safe syscalls only; an all-zero sigaction is a valid value.
    unsafe {
        let mut action: libc::sigaction = std::mem::zeroed();
        action.sa_sigaction = libc::SIG_DFL;
        if libc::sigaction(libc::SIGHUP, &raw const action, std::ptr::null_mut()) == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::setsid() == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::ioctl(0, libc::TIOCSCTTY.into(), 0) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Answers terminal queries found in the child's output.
fn responses(window: &[u8]) -> Vec<&'static [u8]> {
    let mut out = Vec::new();
    let contains = |needle: &[u8]| window.windows(needle.len()).any(|w| w == needle);
    if contains(b"\x1b[6n") {
        out.push(b"\x1b[1;1R".as_slice());
    }
    if contains(b"\x1b[5n") {
        out.push(b"\x1b[0n".as_slice());
    }
    if contains(b"\x1b[c") || contains(b"\x1b[0c") {
        out.push(b"\x1b[?62;22c".as_slice());
    }
    if contains(b"\x1b[16t") {
        out.push(b"\x1b[6;20;10t".as_slice());
    }
    if contains(b"\x1b[14t") {
        out.push(b"\x1b[4;800;1200t".as_slice());
    }
    if contains(b"\x1b[18t") {
        out.push(b"\x1b[8;40;120t".as_slice());
    }
    out
}

/// Reads the terminal side until the child exits, answering queries. Returning drops the only
/// master, which hangs the terminal up and so stops the child: return only once it is gone.
fn pump(master: OwnedFd) {
    let mut file = std::fs::File::from(master);
    let mut writer = match file.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut buffer = [0_u8; 8192];
    let mut tail: Vec<u8> = Vec::new();
    loop {
        match file.read(&mut buffer) {
            // A signal landed on this thread: not the end of the child.
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Ok(0) | Err(_) => return, // EIO once the child side closes.
            Ok(n) => {
                // Keep a short tail so a query split across reads is still seen once.
                let mut window = std::mem::take(&mut tail);
                window.extend_from_slice(&buffer[..n]);
                for answer in responses(&window) {
                    let _ = writer.write_all(answer);
                }
                let keep = window.len().min(8);
                // Drop any complete query from the tail so it is answered only once.
                tail = window[window.len() - keep..].to_vec();
                if tail.contains(&b'n')
                    || tail.contains(&b'c')
                    || tail.contains(&b't')
                    || tail.contains(&b'R')
                {
                    tail.clear();
                }
            }
        }
    }
}

/// The top-level `app.toml` settings the daemon needs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct AppConfig {
    client_port: Option<u16>,
    playback_refresh_ms: Option<u64>,
}

/// Reads `client_port` and `playback_refresh_duration_in_ms` from `app.toml` text (top-level
/// integer keys, before any `[table]`).
fn parse_app_toml(text: &str) -> AppConfig {
    let mut config = AppConfig::default();
    for line in text.lines() {
        let line = line.split('#').next().unwrap_or_default().trim();
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().replace('_', "");
        match key.trim() {
            "client_port" => config.client_port = value.parse().ok(),
            "playback_refresh_duration_in_ms" => config.playback_refresh_ms = value.parse().ok(),
            _ => {}
        }
    }
    config
}

fn app_config(player: &SpotifyPlayer) -> AppConfig {
    let dir = player.config_dir.clone().or_else(|| {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config/spotify-player"))
    });
    dir.and_then(|dir| std::fs::read_to_string(dir.join("app.toml")).ok())
        .map(|text| parse_app_toml(&text))
        .unwrap_or_default()
}

/// The refresh interval to run with: [`REFRESH_MS`], or the user's own smaller positive value.
fn refresh_ms(configured: Option<u64>) -> u64 {
    configured
        .filter(|&ms| ms > 0)
        .map_or(REFRESH_MS, |ms| ms.min(REFRESH_MS))
}

/// A process of this user, as far as the kernel tells us.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Proc {
    pid: i32,
    ppid: i32,
    pgid: i32,
    /// Start time (seconds, microseconds): tells a process from a later one with the same pid.
    start: (u64, u64),
    comm: String,
    args: Vec<String>,
}

/// What a spotify_player process is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Role {
    /// A daemon's warm copy (carries [`SIGNATURE`]).
    Warm,
    /// A short-lived `spotify_player <subcommand>` call.
    Cli,
    /// A spotify_player someone runs themselves.
    Other,
}

fn role(args: &[String]) -> Role {
    let rest = args.get(1..).unwrap_or_default();
    if rest.windows(SIGNATURE.len()).any(|w| w == SIGNATURE) {
        return Role::Warm;
    }
    if rest.iter().any(|a| SUBCOMMANDS.contains(&a.as_str())) {
        return Role::Cli;
    }
    Role::Other
}

impl Proc {
    fn is_spotify_player(&self) -> bool {
        self.comm == "spotify_player"
            && self.args.first().is_some_and(|a| {
                Path::new(a)
                    .file_name()
                    .is_some_and(|n| n == "spotify_player")
            })
    }

    fn role(&self) -> Role {
        role(&self.args)
    }

    /// JSON for status: who holds the port. Another program's arguments are left out (only
    /// its name is shown): they are none of spotify-cli's business and can carry secrets.
    fn describe(&self) -> Value {
        let kind = match self.role() {
            _ if !self.is_spotify_player() => "other_process",
            Role::Warm if self.ppid == 1 => "stale_warm_copy",
            Role::Warm => "another_daemons_warm_copy",
            Role::Cli => "spotify_player_command",
            Role::Other => "your_spotify_player",
        };
        let command = if self.is_spotify_player() {
            self.args.join(" ")
        } else {
            self.comm.clone()
        };
        json!({"pid": self.pid, "parent_pid": self.ppid, "kind": kind, "command": command})
    }
}

/// Splits `KERN_PROCARGS2` output (argc, exec path, NUL padding, argv, env) into argv.
fn parse_procargs(buffer: &[u8]) -> Option<Vec<String>> {
    let argc = i32::from_ne_bytes(buffer.get(..4)?.try_into().ok()?);
    let rest = buffer.get(4..)?;
    // The exec path, then NUL padding up to argv[0].
    let rest = &rest[rest.iter().position(|&b| b == 0)?..];
    let mut rest = &rest[rest.iter().position(|&b| b != 0)?..];
    let mut args = Vec::new();
    for _ in 0..argc {
        let end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
        args.push(String::from_utf8_lossy(&rest[..end]).into_owned());
        rest = rest.get(end + 1..).unwrap_or_default();
    }
    Some(args)
}

#[allow(unsafe_code)]
fn process_args(pid: i32) -> Option<Vec<String>> {
    let mut argmax: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: `argmax` has room for the c_int the kernel writes; `size` says so.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            (&raw mut argmax).cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    let mut buffer = vec![0_u8; usize::try_from(argmax).ok()?];
    let mut size = buffer.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: the kernel writes at most `size` bytes into `buffer` and updates `size`.
    let status = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buffer.as_mut_ptr().cast(),
            &raw mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if status != 0 {
        return None;
    }
    buffer.truncate(size);
    parse_procargs(&buffer)
}

/// The process `pid` with its arguments, when it is alive and belongs to this user.
fn process(pid: i32) -> Option<Proc> {
    let mut process = basic(pid)?;
    process.args = process_args(pid).unwrap_or_default();
    Some(process)
}

/// The process `pid` without its arguments, when it is alive (not a zombie) and belongs to this
/// user.
#[allow(unsafe_code)]
fn basic(pid: i32) -> Option<Proc> {
    // SAFETY: proc_bsdinfo is plain data; all-zero is a valid value.
    let mut info: libc::proc_bsdinfo = unsafe { std::mem::zeroed() };
    let size = libc::c_int::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
    // SAFETY: the buffer is exactly `size` bytes.
    let written =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDTBSDINFO, 0, (&raw mut info).cast(), size) };
    // SAFETY: getuid has no preconditions.
    if written != size
        || info.pbi_status == libc::SZOMB
        || info.pbi_uid != unsafe { libc::getuid() }
    {
        return None;
    }
    let comm: Vec<u8> = info
        .pbi_comm
        .iter()
        .take_while(|&&c| c != 0)
        .map(|&c| c.to_ne_bytes()[0])
        .collect();
    Some(Proc {
        pid,
        ppid: i32::try_from(info.pbi_ppid).unwrap_or(0),
        pgid: i32::try_from(info.pbi_pgid).unwrap_or(0),
        start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
        comm: String::from_utf8_lossy(&comm).into_owned(),
        args: Vec::new(),
    })
}

/// This user's spotify_player processes.
#[allow(unsafe_code)]
fn spotify_players() -> Vec<Proc> {
    // SAFETY: a null buffer asks only for the count.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    let Ok(count) = usize::try_from(count) else {
        return Vec::new();
    };
    let mut pids: Vec<libc::c_int> = vec![0; count + 64];
    let bytes = libc::c_int::try_from(pids.len() * std::mem::size_of::<libc::c_int>())
        .unwrap_or(libc::c_int::MAX);
    // SAFETY: the buffer holds `bytes` bytes; the call returns how many pids it wrote.
    let written = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
    pids.truncate(usize::try_from(written).unwrap_or(0));
    pids.into_iter()
        .filter(|&pid| pid > 0)
        .filter_map(basic)
        // Arguments only for candidates (each read allocates `KERN_ARGMAX` bytes).
        .filter(|p| p.comm == "spotify_player")
        .filter_map(|mut p| {
            p.args = process_args(p.pid)?;
            Some(p)
        })
        .filter(Proc::is_spotify_player)
        .collect()
}

/// Sends `signal` to the process and its group when it leads one (the warm copy leads its own
/// session).
#[allow(unsafe_code)]
fn signal(process: &Proc, signal: libc::c_int) {
    // SAFETY: plain syscalls; failures (already gone) are ignored.
    unsafe {
        if process.pgid == process.pid {
            libc::killpg(process.pid, signal);
        }
        libc::kill(process.pid, signal);
    }
}

fn is_gone(process: &Proc) -> bool {
    basic(process.pid).is_none_or(|now| now.start != process.start)
}

/// Stops a process that is not our child: SIGTERM, then SIGKILL after 2 s. True when it is gone.
fn terminate(process: &Proc) -> bool {
    for (sig, patience) in [
        (libc::SIGTERM, Duration::from_secs(2)),
        (libc::SIGKILL, Duration::from_secs(1)),
    ] {
        signal(process, sig);
        let deadline = Instant::now() + patience;
        while Instant::now() < deadline {
            if is_gone(process) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
    is_gone(process)
}

/// The warm copy this daemon home started last (`warm-player.json`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    pid: i32,
    start_s: u64,
    start_us: u64,
    daemon_pid: u32,
}

fn record_path(dir: &Path) -> PathBuf {
    dir.join("warm-player.json")
}

fn read_record(dir: &Path) -> Option<Record> {
    serde_json::from_slice(&std::fs::read(record_path(dir)).ok()?).ok()
}

fn write_record(dir: &Path, process: &Proc) {
    let record = Record {
        pid: process.pid,
        start_s: process.start.0,
        start_us: process.start.1,
        daemon_pid: std::process::id(),
    };
    if let Ok(bytes) = serde_json::to_vec(&record) {
        let _ = std::fs::write(record_path(dir), bytes);
    }
}

fn clear_record(dir: &Path) {
    let _ = std::fs::remove_file(record_path(dir));
}

/// Whether `process` is a warm copy some earlier daemon left behind: the one recorded in
/// `warm-player.json` (same start time), or an orphan (parent launchd) with the daemon's exact
/// overrides. `current` is this daemon's own copy, never stale.
fn is_stale(process: &Proc, record: Option<&Record>, current: Option<i32>) -> bool {
    if !process.is_spotify_player()
        || Some(process.pid) == current
        || u32::try_from(process.pid).ok() == Some(std::process::id())
    {
        return false;
    }
    let recorded =
        record.is_some_and(|r| r.pid == process.pid && (r.start_s, r.start_us) == process.start);
    recorded || (process.role() == Role::Warm && process.ppid == 1)
}

/// Stops every stale warm copy (see [`is_stale`]); returns the pids stopped.
fn reap_stale(dir: &Path, current: Option<i32>) -> Vec<i32> {
    let record = read_record(dir);
    let mut stopped = Vec::new();
    for process in spotify_players() {
        if is_stale(&process, record.as_ref(), current) {
            log!(
                "warm spotify_player: stopping a stale copy left by an earlier daemon (pid {}, parent {}): {}",
                process.pid,
                process.ppid,
                process.args.join(" ")
            );
            if terminate(&process) {
                stopped.push(process.pid);
            } else {
                log!("warm spotify_player: pid {} did not stop", process.pid);
            }
        }
    }
    if current.is_none() {
        clear_record(dir);
    }
    stopped
}

/// Pids whose UDP socket is bound to local port `port`, from `lsof -F pn` output. Sockets that
/// merely send to the port (`…->127.0.0.1:port`) do not count.
fn parse_lsof(output: &str, port: u16) -> Vec<i32> {
    let suffix = format!(":{port}");
    let mut owners = Vec::new();
    let mut pid = None;
    for line in output.lines() {
        if let Some(value) = line.strip_prefix('p') {
            pid = value.parse::<i32>().ok();
        } else if let Some(name) = line.strip_prefix('n') {
            let local = name.split("->").next().unwrap_or_default();
            if local.ends_with(&suffix)
                && let Some(pid) = pid
                && !owners.contains(&pid)
            {
                owners.push(pid);
            }
        }
    }
    owners
}

/// Pids holding UDP `port` (`None` when lsof could not tell).
fn port_owners(port: u16) -> Option<Vec<i32>> {
    let mut child = std::process::Command::new("/usr/sbin/lsof")
        .args(["-nP", &format!("-iUDP:{port}"), "-F", "pn"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(20)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let mut output = String::new();
    child.stdout.take()?.read_to_string(&mut output).ok()?;
    // lsof exits 1 when nothing matches: that is an answer too.
    Some(parse_lsof(&output, port))
}

/// Who holds the client port.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Holder {
    /// Nobody.
    Free,
    /// lsof could not tell.
    Unknown,
    /// This pid (with its details when it is this user's process).
    Pid(i32, Option<Proc>),
}

fn holder(port: u16) -> Holder {
    match port_owners(port) {
        None => Holder::Unknown,
        Some(pids) => pids
            .first()
            .map_or(Holder::Free, |&pid| Holder::Pid(pid, process(pid))),
    }
}

/// The pid holding `port` right now (for `spotify doctor`).
#[must_use]
pub fn port_owner(port: u16) -> Option<i32> {
    port_owners(port).and_then(|pids| pids.first().copied())
}

async fn off_thread<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static, fallback: T) -> T {
    tokio::task::spawn_blocking(f).await.unwrap_or(fallback)
}

/// Stops the warm copy (daemon shutdown): SIGTERM to its process group, SIGKILL after 1 s.
#[allow(unsafe_code)]
pub fn stop(daemon: &Daemon) {
    let pid = daemon.warm_pid.swap(0, Ordering::SeqCst);
    if let Ok(pid) = libc::pid_t::try_from(pid)
        && pid > 0
    {
        // SAFETY: plain syscalls on our own child; failures (already gone) are ignored.
        unsafe {
            libc::killpg(pid, libc::SIGTERM);
            libc::kill(pid, libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: as above; reaping here is fine, the daemon is exiting.
            let reaped = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            if reaped != 0 {
                break; // Reaped now, or already reaped (ECHILD).
            }
            if Instant::now() >= deadline {
                // SAFETY: as above.
                unsafe {
                    libc::killpg(pid, libc::SIGKILL);
                    libc::kill(pid, libc::SIGKILL);
                }
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    clear_record(&daemon.dir);
}

/// How a fresh copy's start went.
enum Startup {
    /// It holds the port.
    Serving,
    /// lsof could not tell whether it holds the port.
    Unverified,
    /// It exited.
    Exited(std::io::Result<std::process::ExitStatus>),
    /// The daemon is shutting down.
    Shutdown,
    /// It did not get the port in time; who has it.
    NotServing(Holder),
}

/// Waits until `child` holds `port`.
async fn confirm(
    daemon: &Daemon,
    child: &mut tokio::process::Child,
    pid: i32,
    port: u16,
) -> Startup {
    let deadline = Instant::now() + CONFIRM_WITHIN;
    loop {
        tokio::select! {
            status = child.wait() => return Startup::Exited(status),
            () = daemon.shutdown.notified() => return Startup::Shutdown,
            () = tokio::time::sleep(Duration::from_millis(400)) => {}
        }
        let last = off_thread(move || holder(port), Holder::Unknown).await;
        match &last {
            Holder::Pid(owner, _) if *owner == pid => return Startup::Serving,
            // Taken by a copy an earlier daemon left behind after all: ours cannot bind any more.
            Holder::Pid(_, Some(other)) if is_stale(other, None, Some(pid)) => {
                return Startup::NotServing(last);
            }
            // Someone's own instance: ours will never get the port.
            Holder::Pid(_, Some(other)) if other.role() == Role::Other => {
                return Startup::NotServing(last);
            }
            _ => {}
        }
        if Instant::now() >= deadline {
            return match last {
                Holder::Unknown => Startup::Unverified,
                other => Startup::NotServing(other),
            };
        }
    }
}

fn holder_json(holder: &Holder) -> Value {
    match holder {
        Holder::Free => json!(null),
        Holder::Unknown => json!("unknown"),
        Holder::Pid(pid, Some(process)) => {
            let mut value = process.describe();
            value["pid"] = json!(pid);
            value
        }
        Holder::Pid(pid, None) => json!({"pid": pid, "kind": "other_process"}),
    }
}

/// Supervises the instance until shutdown.
#[allow(unsafe_code, clippy::too_many_lines)]
pub async fn run(daemon: Arc<Daemon>) {
    if std::env::var("SPOTIFY_WARM_PLAYER")
        .is_ok_and(|v| silicon_spotify_client::telemetry::is_off(&v))
    {
        // Copies left by earlier daemons would still hold the port and answer from stale state.
        let dir = daemon.dir.clone();
        off_thread(move || reap_stale(&dir, None), Vec::new()).await;
        set(
            &daemon,
            json!({"state": "disabled", "reason": "SPOTIFY_WARM_PLAYER=off"}),
        );
        return;
    }
    let mut failures: u32 = 0;
    // The pid the port was left to, and since when.
    let mut deferred_to: Option<(i32, String)> = None;
    loop {
        let settings = daemon.settings();
        let player = match settings.player() {
            Ok(player) => player,
            Err(error) => {
                set(&daemon, json!({"state": "unavailable", "error": error}));
                if wait(&daemon, Duration::from_secs(300)).await {
                    return;
                }
                continue;
            }
        };
        if !player.has_cached_token() {
            set(
                &daemon,
                json!({"state": "waiting_for_spotify_auth", "error": silicon_spotify_client::player::auth_required(None)}),
            );
            if wait(&daemon, Duration::from_secs(60)).await {
                return;
            }
            continue;
        }
        let config = app_config(&player);
        let port = config.client_port.unwrap_or(DEFAULT_CLIENT_PORT);
        let refresh = refresh_ms(config.playback_refresh_ms);

        // Copies left by earlier daemons would hold the port and answer from stale state.
        let dir = daemon.dir.clone();
        off_thread(move || reap_stale(&dir, None), Vec::new()).await;

        // Leave the port to whoever holds it now.
        match off_thread(move || holder(port), Holder::Unknown).await {
            Holder::Free | Holder::Unknown => deferred_to = None,
            Holder::Pid(pid, process) => {
                let transient = process
                    .as_ref()
                    .is_some_and(|p| p.is_spotify_player() && p.role() == Role::Cli);
                if transient {
                    // A one-off `spotify_player <command>` holds it for a second.
                    if wait(&daemon, Duration::from_secs(2)).await {
                        return;
                    }
                    continue;
                }
                let since = match &deferred_to {
                    Some((to, since)) if *to == pid => since.clone(),
                    _ => {
                        log!(
                            "warm spotify_player: 127.0.0.1:{port} is held by pid {pid}; not starting a copy while it runs"
                        );
                        let since = silicon_spotify_client::model::now_rfc3339();
                        deferred_to = Some((pid, since.clone()));
                        since
                    }
                };
                set(
                    &daemon,
                    json!({"state": "deferred", "port": port, "port_owner": holder_json(&Holder::Pid(pid, process)),
                        "since": since,
                        "note": "Another spotify_player holds the client port, so it answers spotify-cli's commands (from its own state; it refreshes playback only as often as its own playback_refresh_duration_in_ms says). The daemon starts its own copy once that one exits."}),
                );
                if wait(&daemon, DEFERRED_RECHECK).await {
                    return;
                }
                continue;
            }
        }

        let (master, slave) = match open_pty() {
            Ok(pair) => pair,
            Err(error) => {
                set(
                    &daemon,
                    json!({"state": "failed", "error": format!("cannot open a pseudo-terminal: {error}")}),
                );
                if wait(&daemon, Duration::from_secs(300)).await {
                    return;
                }
                continue;
            }
        };
        let stdio = |fd: &OwnedFd| fd.try_clone().map(Stdio::from);
        let (Ok(stdin), Ok(stdout), Ok(stderr)) = (stdio(&slave), stdio(&slave), stdio(&slave))
        else {
            set(
                &daemon,
                json!({"state": "failed", "error": "cannot duplicate the terminal descriptor"}),
            );
            if wait(&daemon, Duration::from_secs(300)).await {
                return;
            }
            continue;
        };
        let mut command = Command::new(&player.binary);
        if let Some(dir) = &player.config_dir {
            command.arg("-c").arg(dir);
        }
        if let Some(dir) = &player.cache_dir {
            command.arg("-C").arg(dir);
        }
        command
            .args(SIGNATURE)
            .arg("-o")
            .arg(format!("playback_refresh_duration_in_ms={refresh}"))
            .env("TERM", "xterm-256color")
            .env("COLUMNS", "120")
            .env("LINES", "40")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true);
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(lead_session_on_stdin);
        }
        let started = Instant::now();
        let spawned = command.spawn();
        // Close the parent's copies of the terminal's child side.
        drop(command);
        drop(slave);
        let mut child = match spawned {
            Ok(child) => child,
            Err(error) => {
                set(
                    &daemon,
                    json!({"state": "failed", "error": format!("cannot start spotify_player: {error}")}),
                );
                if wait(&daemon, Duration::from_secs(120)).await {
                    return;
                }
                continue;
            }
        };
        std::thread::Builder::new()
            .name("warm-pty".into())
            .spawn(move || pump(master))
            .ok();
        let pid = child.id().and_then(|p| i32::try_from(p).ok()).unwrap_or(0);
        daemon
            .warm_pid
            .store(u32::try_from(pid).unwrap_or(0), Ordering::SeqCst);
        let dir = daemon.dir.clone();
        off_thread(
            move || {
                if let Some(process) = process(pid) {
                    write_record(&dir, &process);
                }
            },
            (),
        )
        .await;
        let since = silicon_spotify_client::model::now_rfc3339();
        let base = json!({"pid": pid, "port": port, "refresh_ms": refresh, "binary": player.binary, "since": since});
        let with = |state: &str, extra: Value| {
            let mut value = base.clone();
            value["state"] = json!(state);
            if let (Some(object), Value::Object(extra)) = (value.as_object_mut(), extra) {
                object.extend(extra);
            }
            value
        };
        set(&daemon, with("starting", json!({})));
        log!("warm spotify_player started (pid {pid}); waiting for it to take 127.0.0.1:{port}");

        let status = match confirm(&daemon, &mut child, pid, port).await {
            Startup::Shutdown => {
                stop(&daemon);
                return;
            }
            Startup::Exited(status) => status,
            Startup::NotServing(holder) => {
                let _ = child.kill().await;
                daemon.warm_pid.store(0, Ordering::SeqCst);
                clear_record(&daemon.dir);
                log!(
                    "warm spotify_player pid {pid} did not get 127.0.0.1:{port} (held by {}); stopped it",
                    holder_json(&holder)
                );
                failures += 1;
                let retry = Duration::from_secs(u64::from(2 * 2_u32.pow(failures.min(6))));
                set(
                    &daemon,
                    json!({"state": "not_serving", "port": port, "port_owner": holder_json(&holder), "retry_in_s": retry.as_secs(),
                        "note": "The daemon's copy never got the client port, so it could not answer commands; it was stopped and starts again after the delay."}),
                );
                if wait(&daemon, retry).await {
                    return;
                }
                continue;
            }
            outcome @ (Startup::Serving | Startup::Unverified) => {
                if matches!(outcome, Startup::Serving) {
                    set(
                        &daemon,
                        with(
                            "running",
                            json!({"port_owner_pid": pid, "serves_cli": true}),
                        ),
                    );
                    log!(
                        "warm spotify_player pid {pid} holds 127.0.0.1:{port}; running (playback refresh every {refresh} ms)"
                    );
                } else {
                    set(
                        &daemon,
                        with(
                            "running",
                            json!({"serves_cli": null, "note": "lsof could not confirm that this copy holds the client port."}),
                        ),
                    );
                    log!(
                        "warm spotify_player pid {pid} running; lsof could not confirm it holds 127.0.0.1:{port}"
                    );
                }
                // Supervise: exit, shutdown, or losing the port.
                loop {
                    tokio::select! {
                        status = child.wait() => break status,
                        () = daemon.shutdown.notified() => {
                            stop(&daemon);
                            return;
                        }
                        () = tokio::time::sleep(RECHECK_EVERY) => {
                            let now = off_thread(move || holder(port), Holder::Unknown).await;
                            if now == Holder::Free || matches!(now, Holder::Pid(owner, _) if owner != pid) {
                                log!("warm spotify_player pid {pid} no longer holds 127.0.0.1:{port} ({}); restarting it", holder_json(&now));
                                let _ = child.kill().await;
                            }
                        }
                    }
                }
            }
        };
        daemon.warm_pid.store(0, Ordering::SeqCst);
        clear_record(&daemon.dir);
        let lived = started.elapsed();
        failures = if lived > Duration::from_secs(120) {
            0
        } else {
            failures + 1
        };
        let backoff = Duration::from_secs(u64::from(5 * 2_u32.pow(failures.min(6))));
        log!("warm spotify_player exited ({status:?}) after {lived:?}; restarting in {backoff:?}");
        set(
            &daemon,
            json!({"state": "restarting", "last_exit": format!("{status:?}"), "retry_in_s": backoff.as_secs(),
                "note": "The daemon starts a new copy after the delay. Its own log is in ~/.cache/spotify-player/."}),
        );
        if wait(&daemon, backoff).await {
            return;
        }
    }
}

/// Sleeps; returns true when shutting down.
async fn wait(daemon: &Daemon, duration: Duration) -> bool {
    tokio::select! {
        () = tokio::time::sleep(duration) => false,
        () = daemon.shutdown.notified() => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|&a| a.to_owned()).collect()
    }

    fn proc(pid: i32, ppid: i32, args: &[&str]) -> Proc {
        Proc {
            pid,
            ppid,
            pgid: pid,
            start: (1_000, 5),
            comm: "spotify_player".into(),
            args: strings(args),
        }
    }

    const WARM_V1: &[&str] = &[
        "/opt/homebrew/bin/spotify_player",
        "-o",
        "enable_streaming=Never",
        "-o",
        "enable_media_control=false",
        "-o",
        "enable_notify=false",
    ];

    #[test]
    fn roles_tell_warm_copies_from_commands_and_your_own_instance() {
        assert_eq!(role(&strings(WARM_V1)), Role::Warm);
        let mut current = WARM_V1.to_vec();
        current.extend(["-o", "playback_refresh_duration_in_ms=3000"]);
        assert_eq!(role(&strings(&current)), Role::Warm);
        let with_dirs = [
            "/usr/local/bin/spotify_player",
            "-c",
            "/Users/x/My Config",
            "-C",
            "/tmp/c",
            "-o",
            "enable_streaming=Never",
            "-o",
            "enable_media_control=false",
            "-o",
            "enable_notify=false",
        ];
        assert_eq!(role(&strings(&with_dirs)), Role::Warm);
        assert_eq!(
            role(&strings(&["spotify_player", "get", "key", "playback"])),
            Role::Cli
        );
        assert_eq!(role(&strings(&["spotify_player"])), Role::Other);
        // Your own instance with some of the same settings is still yours.
        assert_eq!(
            role(&strings(&[
                "spotify_player",
                "-o",
                "enable_streaming=Never",
                "-o",
                "enable_notify=false"
            ])),
            Role::Other
        );
    }

    #[test]
    fn only_orphaned_or_recorded_warm_copies_are_stale() {
        // An orphan (parent launchd) with the daemon's overrides.
        assert!(is_stale(&proc(41_298, 1, WARM_V1), None, Some(2_273)));
        // The daemon's own current copy.
        assert!(!is_stale(&proc(2_273, 1, WARM_V1), None, Some(2_273)));
        // Another live daemon's copy (its parent is alive).
        assert!(!is_stale(&proc(500, 499, WARM_V1), None, None));
        // Your own spotify_player, even when orphaned (nohup).
        assert!(!is_stale(&proc(600, 1, &["spotify_player"]), None, None));
        assert!(!is_stale(&proc(601, 88, &["spotify_player"]), None, None));
        // The copy recorded in warm-player.json, only while its start time matches.
        let record = Record {
            pid: 700,
            start_s: 1_000,
            start_us: 5,
            daemon_pid: 42,
        };
        assert!(is_stale(
            &proc(700, 42, &["spotify_player", "-c", "x"]),
            Some(&record),
            None
        ));
        let mut reused = proc(700, 42, &["spotify_player"]);
        reused.start = (2_000, 0);
        assert!(!is_stale(&reused, Some(&record), None));
    }

    #[test]
    fn app_toml_settings_are_read_from_the_top_level() {
        let text = "theme = \"dracula\"\nclient_port = 8_081 # custom\nplayback_refresh_duration_in_ms = 0\n\n[device]\nclient_port = 1\n";
        assert_eq!(
            parse_app_toml(text),
            AppConfig {
                client_port: Some(8081),
                playback_refresh_ms: Some(0)
            }
        );
        assert_eq!(parse_app_toml(""), AppConfig::default());
        assert_eq!(refresh_ms(Some(0)), REFRESH_MS);
        assert_eq!(refresh_ms(None), REFRESH_MS);
        assert_eq!(refresh_ms(Some(1_000)), 1_000);
        assert_eq!(refresh_ms(Some(60_000)), REFRESH_MS);
    }

    #[test]
    fn lsof_output_names_only_the_socket_bound_to_the_port() {
        let output = "p41298\nf16\nn127.0.0.1:8080\np5000\nf9\nn127.0.0.1:50553->127.0.0.1:8080\np6000\nf3\nn127.0.0.1:18080\n";
        assert_eq!(parse_lsof(output, 8080), vec![41_298]);
        assert!(parse_lsof("", 8080).is_empty());
        assert_eq!(parse_lsof("p7\nf4\nn*:8080\n", 8080), vec![7]);
    }

    #[test]
    fn procargs_are_split_into_argv() {
        let mut buffer = 3_i32.to_ne_bytes().to_vec();
        buffer.extend_from_slice(b"/opt/homebrew/bin/spotify_player\0\0\0\0");
        buffer.extend_from_slice(b"spotify_player\0-o\0enable_notify=false\0TERM=xterm\0");
        assert_eq!(
            parse_procargs(&buffer),
            Some(strings(&["spotify_player", "-o", "enable_notify=false"]))
        );
        assert_eq!(parse_procargs(&[1, 0]), None);
    }

    #[test]
    fn processes_are_read_from_the_kernel() {
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .expect("spawn sleep");
        let pid = i32::try_from(child.id()).expect("pid");
        let seen = process(pid).expect("our own child is visible");
        assert_eq!(seen.args, strings(&["/bin/sleep", "30"]));
        assert_eq!(seen.comm, "sleep");
        assert_eq!(u32::try_from(seen.ppid).ok(), Some(std::process::id()));
        assert!(!seen.is_spotify_player());
        let _ = child.kill();
        let _ = child.wait();
        assert!(is_gone(&seen));
    }

    #[test]
    fn the_port_holder_is_found_with_lsof() {
        let socket = std::net::UdpSocket::bind("127.0.0.1:0").expect("bind");
        let port = socket.local_addr().expect("addr").port();
        let me = i32::try_from(std::process::id()).expect("pid");
        assert_eq!(port_owners(port), Some(vec![me]));
        match holder(port) {
            Holder::Pid(pid, Some(process)) => {
                assert_eq!(pid, me);
                assert!(!is_stale(&process, None, None), "the test is no warm copy");
            }
            other => panic!("expected this process, got {other:?}"),
        }
        drop(socket);
        assert_eq!(holder(port), Holder::Free);
    }

    #[test]
    fn only_spotify_player_processes_are_listed() {
        // Read-only: lists (never signals) whatever runs on this machine.
        for process in spotify_players() {
            assert!(process.is_spotify_player(), "{process:?}");
        }
    }

    #[test]
    fn the_copy_hangs_up_when_the_daemon_goes_away() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        let (master, slave) = open_pty().expect("pty");
        let clone = |fd: &OwnedFd| Stdio::from(fd.try_clone().expect("dup"));
        let mut command = std::process::Command::new("/bin/sleep");
        command
            .arg("30")
            .stdin(clone(&slave))
            .stdout(clone(&slave))
            .stderr(clone(&slave));
        // SAFETY: only async-signal-safe calls between fork and exec.
        unsafe {
            command.pre_exec(lead_session_on_stdin);
        }
        let mut child = command.spawn().expect("spawn sleep");
        drop(command);
        drop(slave);
        // The daemon exiting (even on SIGKILL) closes its master, the only one.
        drop(master);
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Some(status) = child.try_wait().expect("wait") {
                break status;
            }
            if Instant::now() > deadline {
                let _ = child.kill();
                panic!("the child outlived its terminal");
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        assert_eq!(status.signal(), Some(libc::SIGHUP));
    }
}
