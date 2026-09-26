//! Keeps one headless `spotify_player` instance running so CLI calls take ~20 ms instead of ~1.5 s.
//!
//! spotify_player's CLI forwards each command over UDP (`client_port`, default 8080) to a running
//! instance, or starts a throwaway client when none answers. The Homebrew build has no daemon
//! mode, so the daemon runs the normal terminal app on a pseudo-terminal it owns, answering the
//! terminal's startup queries (cursor position, device attributes, window size) the way a real
//! terminal would; without those answers the TUI refuses to start ("cursor position could not be
//! read"). Streaming, media keys and desktop notifications are switched off: the instance never
//! becomes a playback device, it only answers Web API commands. If you run spotify_player
//! yourself, your instance owns the port and this one simply exits and retries later.

use std::io::{Read as _, Write as _};
use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::process::Command;

use crate::log;
use crate::service::Daemon;

fn set(daemon: &Daemon, value: serde_json::Value) {
    *daemon
        .warm
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

/// Opens a pseudo-terminal pair sized 40×120.
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
    Ok(unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) })
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

/// Reads the terminal side until the child exits, answering queries.
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

/// Supervises the instance until shutdown.
#[allow(unsafe_code)]
pub async fn run(daemon: Arc<Daemon>) {
    if std::env::var("SPOTIFY_WARM_PLAYER")
        .is_ok_and(|v| silicon_spotify_client::telemetry::is_off(&v))
    {
        set(
            &daemon,
            json!({"state": "disabled", "reason": "SPOTIFY_WARM_PLAYER=off"}),
        );
        return;
    }
    let mut failures: u32 = 0;
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
            .args([
                "-o",
                "enable_streaming=Never",
                "-o",
                "enable_media_control=false",
                "-o",
                "enable_notify=false",
            ])
            .env("TERM", "xterm-256color")
            .env("COLUMNS", "120")
            .env("LINES", "40")
            .stdin(stdin)
            .stdout(stdout)
            .stderr(stderr)
            .kill_on_drop(true);
        // SAFETY: only async-signal-safe calls between fork and exec: a new session with the
        // pseudo-terminal (fd 0) as its controlling terminal.
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::ioctl(0, libc::TIOCSCTTY.into(), 0) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let started = std::time::Instant::now();
        let spawned = command.spawn();
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
        let _ = master.as_raw_fd();
        std::thread::Builder::new()
            .name("warm-pty".into())
            .spawn(move || pump(master))
            .ok();
        set(
            &daemon,
            json!({"state": "running", "pid": child.id(), "binary": player.binary, "since": silicon_spotify_client::model::now_rfc3339()}),
        );
        log!("warm spotify_player started (pid {:?})", child.id());
        tokio::select! {
            status = child.wait() => {
                let lived = started.elapsed();
                failures = if lived > Duration::from_secs(120) { 0 } else { failures + 1 };
                let backoff = Duration::from_secs(u64::from(5 * 2_u32.pow(failures.min(6))));
                log!("warm spotify_player exited ({status:?}) after {lived:?}; restarting in {backoff:?}");
                set(&daemon, json!({"state": "restarting", "last_exit": format!("{status:?}"), "retry_in_s": backoff.as_secs(),
                    "note": "If you run spotify_player yourself, its instance answers CLI calls and this one exits; that is fine. Its own log is in ~/.cache/spotify-player/."}));
                if wait(&daemon, backoff).await {
                    return;
                }
            }
            () = daemon.shutdown.notified() => {
                let _ = child.kill().await;
                return;
            }
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
