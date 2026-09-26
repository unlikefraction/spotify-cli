//! Talking to, starting and installing `spotify-daemon`.

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use silicon_spotify_client::ipc::{self, PROTOCOL, Request};
use silicon_spotify_client::{Error, Result, VERSION};

use crate::Ctx;
use crate::args::DaemonCommand;

/// launchd label for the per-user agent.
pub const LABEL: &str = "com.unlikefraction.spotify.daemon";

fn request(ctx: Option<&Ctx>, op: &str, args: Value) -> Request {
    Request {
        v: PROTOCOL,
        id: uuid::Uuid::now_v7().to_string(),
        op: op.to_owned(),
        home: ctx.map(|c| c.home.key()),
        args,
        client_version: VERSION.to_owned(),
        trace_id: ctx.map(|c| c.trace_id.clone()),
        isi: ctx.and_then(|c| c.isi.clone()),
    }
}

fn timeout_for(op: &str) -> Duration {
    match op {
        "trigger.wait" => Duration::from_secs(24 * 3600),
        "trigger.test" | "update.check" | "update.apply" | "spotify.launch" => {
            Duration::from_secs(90)
        }
        _ => Duration::from_secs(60),
    }
}

/// Calls the daemon, starting it (or replacing an older one) first.
///
/// # Errors
/// `daemon_unavailable` or the op's error.
pub async fn call(ctx: &Ctx, op: &str, args: Value) -> Result<Value> {
    ensure(ctx).await?;
    let mut timeout = timeout_for(op);
    if op == "trigger.wait"
        && let Some(ms) = args.get("timeout_ms").and_then(Value::as_u64)
    {
        timeout = Duration::from_millis(ms + 5_000);
    }
    ipc::call(&request(Some(ctx), op, args), timeout).await
}

async fn probe() -> Result<Value> {
    ipc::call(
        &request(None, "daemon.status", json!({})),
        Duration::from_secs(5),
    )
    .await
}

/// Makes sure a daemon of at least this version is running.
///
/// # Errors
/// `platform_unsupported` off macOS; `daemon_unavailable` when it cannot start.
pub async fn ensure(ctx: &Ctx) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(Error::platform_unsupported("Controlling Spotify"));
    }
    let autostart = !std::env::var("SPOTIFY_DAEMON_AUTOSTART")
        .is_ok_and(|v| silicon_spotify_client::telemetry::is_off(&v));
    match probe().await {
        Err(error) if error.code == "daemon_unavailable" && !autostart => {
            Err(Error::daemon_unavailable(
                "The Spotify daemon is not running and SPOTIFY_DAEMON_AUTOSTART is off.",
            ))
        }
        Ok(status) if older(&status) => replace(ctx, &status).await,
        Ok(_) => Ok(()),
        // An older daemon speaking another protocol version answers with protocol_mismatch.
        Err(error) if error.code == "protocol_mismatch" => replace(ctx, &json!({})).await,
        Err(error) if error.code == "daemon_unavailable" => match start().await {
            // The launchd agent can still point at an older install: replace what it started.
            Ok(started) if older(&started["status"]) => replace(ctx, &started["status"]).await,
            Err(error) if error.code == "protocol_mismatch" => replace(ctx, &json!({})).await,
            other => other.map(drop),
        },
        Err(error) => Err(error),
    }
}

fn older(status: &Value) -> bool {
    let running = status
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("0.0.0");
    semver::Version::parse(running)
        .ok()
        .zip(semver::Version::parse(VERSION).ok())
        .is_none_or(|(r, c)| r < c)
}

/// Replaces an older daemon with this CLI's sibling daemon.
async fn replace(ctx: &Ctx, status: &Value) -> Result<()> {
    let running = status
        .get("version")
        .and_then(Value::as_str)
        .unwrap_or("an older version");
    ctx.hint(&format!("Restarting spotify-daemon {running} → {VERSION}."));
    let restarted = restart().await;
    let still_older = probe().await.map_or(true, |s| older(&s));
    if still_older {
        // The launchd agent may point at an older install: re-point it at this CLI's daemon.
        if plist_path().is_some_and(|p| p.is_file()) {
            let _ = stop().await;
            install()?;
            start().await?;
        } else if let Err(error) = restarted
            && error.code != "protocol_mismatch"
        {
            // Why the restart failed (stuck, missing binary, did not come up) says more than
            // `daemon_outdated`.
            return Err(error);
        }
        if probe().await.map_or(true, |s| older(&s)) {
            return Err(Error::new(
                "daemon_outdated",
                format!("The running spotify-daemon is older than this CLI ({VERSION}) and could not be replaced."),
                "Reinstall so `spotify` and `spotify-daemon` come from the same release (curl -fsSL https://spotify.unlikefraction.com/install.sh | sh), then `spotify daemon restart`.",
            )
            .with_details(json!({"daemon_binary": daemon_binary().ok()})));
        }
    }
    Ok(())
}

/// The daemon binary next to this CLI (Honeycomb launchers are symlinks, so canonicalize).
///
/// # Errors
/// `daemon_missing`.
pub fn daemon_binary() -> Result<PathBuf> {
    // Prefer the path we were invoked through: Honeycomb installs expose stable launcher
    // symlinks, while the resolved package directory is replaced on every update.
    let invoked = std::env::current_exe().ok();
    let exe = invoked.as_ref().and_then(|p| std::fs::canonicalize(p).ok());
    for candidate in [invoked.as_ref(), exe.as_ref()].into_iter().flatten() {
        if let Some(path) = candidate
            .parent()
            .map(|dir| dir.join("spotify-daemon"))
            .filter(|p| p.is_file())
        {
            return Ok(path);
        }
    }
    if let Some(path) = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|d| d.join("spotify-daemon"))
            .find(|p| p.is_file())
    }) {
        return Ok(path);
    }
    Err(Error::new(
        "daemon_missing",
        format!(
            "spotify-daemon is not installed next to this CLI ({}) or on PATH.",
            exe.map(|p| p.display().to_string()).unwrap_or_default()
        ),
        "Reinstall: curl -fsSL https://spotify.unlikefraction.com/install.sh | sh (or `honeycomb install spotify`).",
    ))
}

fn log_path() -> Result<PathBuf> {
    Ok(ipc::daemon_dir()?.join("daemon.log"))
}

fn plist_path() -> Option<PathBuf> {
    ipc::real_home().map(|h| {
        h.join("Library/LaunchAgents")
            .join(format!("{LABEL}.plist"))
    })
}

fn uid() -> u32 {
    ipc::current_uid()
}

fn launchctl(args: &[&str]) -> std::io::Result<std::process::Output> {
    Command::new("/bin/launchctl")
        .args(args)
        .stdin(Stdio::null())
        .output()
}

fn launchctl_ok(args: &[&str]) -> bool {
    launchctl(args).is_ok_and(|o| o.status.success())
}

/// The agent's launchd service target, `gui/<uid>/<label>`.
fn service() -> String {
    format!("gui/{}/{LABEL}", uid())
}

/// `launchctl print` of the agent, when launchd has it loaded. A booted-out job stays loaded until
/// its process has exited, and loading it again before then fails (`bootstrap` error 5).
fn launchd_job() -> Option<String> {
    launchctl(&["print", &service()])
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
}

/// The pid of the job's running process in `launchctl print` output (none while it is stopped).
fn job_pid(print: &str) -> Option<u64> {
    print
        .lines()
        .find_map(|line| line.trim().strip_prefix("pid = "))
        .and_then(|pid| pid.trim().parse().ok())
}

/// Loads the agent when launchd does not have it, else starts it if it is not running (a no-op
/// for a running job). Failures show as the daemon not coming up.
fn launchd_start() {
    if launchd_job().is_some() {
        let _ = launchctl(&["kickstart", &service()]);
    } else if let Some(plist) = plist_path() {
        let _ = launchctl(&[
            "bootstrap",
            &format!("gui/{}", uid()),
            &plist.to_string_lossy(),
        ]);
    }
}

/// How long `start` and `restart` wait for the daemon to answer: launchd holds a respawn back
/// until the previous instance has run for ThrottleInterval (10 s).
const START_TIMEOUT: Duration = Duration::from_secs(15);

/// Waits for a daemon other than `old_pid` to answer.
async fn wait_for_new(old_pid: Option<u64>, via: &str) -> Result<Value> {
    let deadline = Instant::now() + START_TIMEOUT;
    let mut next_nudge = Instant::now() + Duration::from_secs(1);
    loop {
        tokio::time::sleep(Duration::from_millis(150)).await;
        match probe().await {
            Ok(status)
                if old_pid.is_none() || status.get("pid").and_then(Value::as_u64) != old_pid =>
            {
                return Ok(status);
            }
            // Up, but an older release: the caller replaces it.
            Err(error) if error.code == "protocol_mismatch" => return Err(error),
            _ => {}
        }
        let now = Instant::now();
        if via == "launchd" && now >= next_nudge {
            // A job booted out a moment ago is still leaving (bootstrap fails until it has), and
            // a job whose last run was short waits out its throttle: keep asking.
            launchd_start();
            next_nudge = now + Duration::from_secs(1);
        }
        if now > deadline {
            let tail = tail(20).unwrap_or_default();
            return Err(Error::daemon_unavailable(format!(
                "spotify-daemon did not come up within {} s (started via {via}).",
                START_TIMEOUT.as_secs()
            ))
            .with_details(json!({"log_tail": tail})));
        }
    }
}

/// Starts the daemon (launchd when installed, else detached) and waits for its socket.
///
/// # Errors
/// `daemon_unavailable` with the last log lines.
pub async fn start() -> Result<Value> {
    if let Ok(status) = probe().await {
        return Ok(json!({"started": false, "already_running": true, "status": status}));
    }
    let dir = ipc::daemon_dir()?;
    std::fs::create_dir_all(&dir)
        .map_err(|e| Error::daemon_unavailable(format!("Cannot create {}: {e}.", dir.display())))?;
    let installed = plist_path().is_some_and(|p| p.is_file());
    let via = if installed {
        launchd_start();
        "launchd"
    } else {
        spawn(&dir)?;
        "spawn"
    };
    let status = wait_for_new(None, via).await?;
    Ok(json!({"started": true, "via": via, "status": status}))
}

/// Starts the daemon as a detached process (no launchd agent).
fn spawn(dir: &Path) -> Result<()> {
    let binary = daemon_binary()?;
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path()?)
        .map_err(|e| Error::daemon_unavailable(format!("Cannot open the daemon log: {e}.")))?;
    let err = log
        .try_clone()
        .map_err(|e| Error::daemon_unavailable(e.to_string()))?;
    let mut command = Command::new(&binary);
    command
        .stdin(Stdio::null())
        .stdout(log)
        .stderr(err)
        .current_dir(dir);
    // The daemon serves every home: it must not inherit one caller's home, plane or backend.
    for key in [
        "SILICON_HOME",
        "NOTIFY_SOCKET",
        "SPOTIFY_TEST_APP_SECRET",
        "SPOTIFY_API_URL",
        "ISI",
    ] {
        command.env_remove(key);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn().map_err(|e| {
        Error::daemon_unavailable(format!("Cannot start {}: {e}.", binary.display()))
    })?;
    Ok(())
}

/// Asks a daemon to shut down cleanly (it stops its warm spotify_player first).
async fn request_shutdown() {
    let _ = ipc::call(
        &request(None, "daemon.shutdown", json!({})),
        Duration::from_secs(5),
    )
    .await;
}

/// Asks the daemon to stop and waits for its socket to disappear and, for the launchd agent, for
/// launchd to let go of the job (until then it cannot be started again).
///
/// # Errors
/// When it refuses to stop.
pub async fn stop() -> Result<Value> {
    let was_running = probe().await.is_ok();
    // A launchd KeepAlive agent would restart it; stop the job instead (even when its plist is
    // gone: the loaded job still restarts it). `bootout` returns before the process has exited.
    let was_loaded = launchd_job().is_some();
    if was_loaded {
        let _ = launchctl(&["bootout", &service()]);
    }
    if !was_running && !was_loaded {
        return Ok(json!({"stopped": true, "was_running": false}));
    }
    if probe().await.is_ok() {
        request_shutdown().await;
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while probe().await.is_ok() || (was_loaded && launchd_job().is_some()) {
        if Instant::now() > deadline {
            return Err(Error::new(
                "daemon_stuck",
                "spotify-daemon did not stop within 10 s.",
                "Find it with `pgrep -fl spotify-daemon` and kill it.",
            ));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Ok(json!({"stopped": true, "was_running": was_running}))
}

/// Stops the running daemon and starts a new one. With the launchd agent loaded, launchd itself
/// restarts the job (`kickstart -k`: SIGTERM, which the daemon handles like `daemon.shutdown`,
/// then a new process once the old one has exited), so the agent stays loaded; `bootout` then
/// `bootstrap` would race the exiting process and leave the agent unloaded.
///
/// # Errors
/// `daemon_stuck` or `daemon_unavailable`.
pub async fn restart() -> Result<Value> {
    let before = probe().await.ok();
    let old_pid = before
        .as_ref()
        .and_then(|s| s.get("pid"))
        .and_then(Value::as_u64);
    let installed = plist_path().is_some_and(|p| p.is_file());
    if let Some(job) = launchd_job().filter(|_| installed) {
        if old_pid.is_some() && old_pid != job_pid(&job) {
            // A daemon started outside launchd holds the single-instance lock: stop it first.
            request_shutdown().await;
            let deadline = Instant::now() + Duration::from_secs(10);
            while probe()
                .await
                .is_ok_and(|s| s.get("pid").and_then(Value::as_u64) == old_pid)
            {
                if Instant::now() > deadline {
                    return Err(Error::new(
                        "daemon_stuck",
                        "spotify-daemon did not stop within 10 s.",
                        "Find it with `pgrep -fl spotify-daemon` and kill it.",
                    ));
                }
                tokio::time::sleep(Duration::from_millis(150)).await;
            }
        }
        if launchctl_ok(&["kickstart", "-k", &service()]) {
            let status = wait_for_new(old_pid, "launchd").await?;
            return Ok(
                json!({"restarted": true, "started": true, "via": "launchd", "previous_pid": old_pid, "status": status}),
            );
        }
    }
    stop().await?;
    let mut value = start().await?;
    value["restarted"] = json!(true);
    value["previous_pid"] = json!(old_pid);
    Ok(value)
}

fn tail(lines: usize) -> Result<Vec<String>> {
    let text = std::fs::read_to_string(log_path()?).unwrap_or_default();
    let all: Vec<&str> = text.lines().collect();
    Ok(all[all.len().saturating_sub(lines)..]
        .iter()
        .map(|l| (*l).to_owned())
        .collect())
}

fn xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn install() -> Result<Value> {
    let binary = daemon_binary()?;
    let plist = plist_path().ok_or_else(|| Error::internal("no home directory"))?;
    let log = log_path()?;
    if let Some(parent) = plist.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| Error::internal(format!("cannot create {}: {e}", parent.display())))?;
    }
    std::fs::create_dir_all(ipc::daemon_dir()?).map_err(|e| Error::internal(e.to_string()))?;
    let mut env = String::from(
        "<key>PATH</key><string>/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin</string>",
    );
    if let Some(home) = std::env::var_os("SPOTIFY_DAEMON_HOME") {
        env.push_str(&format!(
            "<key>SPOTIFY_DAEMON_HOME</key><string>{}</string>",
            xml(&home.to_string_lossy())
        ));
    }
    let content = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LABEL}</string>
  <key>ProgramArguments</key><array><string>{}</string></array>
  <key>EnvironmentVariables</key><dict>{env}</dict>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>ProcessType</key><string>Interactive</string>
  <key>StandardOutPath</key><string>{}</string>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml(&binary.to_string_lossy()),
        xml(&log.to_string_lossy()),
        xml(&log.to_string_lossy()),
    );
    std::fs::write(&plist, content)
        .map_err(|e| Error::internal(format!("cannot write {}: {e}", plist.display())))?;
    let domain = format!("gui/{}", uid());
    if launchd_job().is_some() {
        let _ = launchctl(&["bootout", &service()]);
        // Loading it again fails until the old process has exited and launchd let go of it.
        let deadline = Instant::now() + Duration::from_secs(10);
        while launchd_job().is_some() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(150));
        }
    }
    let _ = launchctl(&["enable", &service()]);
    let output = launchctl(&["bootstrap", &domain, &plist.to_string_lossy()])
        .map_err(|e| Error::internal(format!("launchctl failed: {e}")))?;
    if !output.status.success() {
        return Err(Error::new(
            "launchd_failed",
            format!(
                "launchctl bootstrap failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ),
            "Run it from a logged-in GUI session (launchd agents need one). `spotify daemon start` works without launchd.",
        ));
    }
    Ok(json!({"installed": true, "label": LABEL, "plist": plist, "binary": binary, "log": log}))
}

/// Handles `spotify daemon …`.
///
/// # Errors
/// Per subcommand.
pub async fn command(ctx: &Ctx, action: DaemonCommand) -> Result<()> {
    if !cfg!(target_os = "macos") {
        return Err(Error::platform_unsupported("spotify-daemon"));
    }
    match action {
        DaemonCommand::Start => {
            let value = start().await?;
            ctx.emit(&value, |v| {
                if v["already_running"] == json!(true) {
                    format!(
                        "spotify-daemon {} is already running (pid {}).",
                        v["status"]["version"].as_str().unwrap_or("?"),
                        v["status"]["pid"]
                    )
                } else {
                    format!(
                        "spotify-daemon started via {} (pid {}).",
                        v["via"].as_str().unwrap_or("?"),
                        v["status"]["pid"]
                    )
                }
            });
        }
        DaemonCommand::Stop => {
            let value = stop().await?;
            ctx.emit(&value, |v| {
                if v["was_running"] == json!(true) {
                    "spotify-daemon stopped.".into()
                } else {
                    "spotify-daemon was not running.".into()
                }
            });
            if plist_path().is_some_and(|p| p.is_file()) {
                ctx.hint("The launchd agent is unloaded until the next `spotify daemon start` or login. Remove it with `spotify daemon uninstall`.");
            }
        }
        DaemonCommand::Restart => {
            let value = restart().await?;
            ctx.emit(&value, |v| {
                format!(
                    "spotify-daemon restarted via {} (pid {}, v{}).",
                    v["via"].as_str().unwrap_or("?"),
                    v["status"]["pid"],
                    v["status"]["version"].as_str().unwrap_or("?")
                )
            });
        }
        DaemonCommand::Status => {
            let value = match probe().await {
                Ok(status) => status,
                Err(error) if error.code == "daemon_unavailable" => json!({
                    "running": false,
                    "installed": plist_path().is_some_and(|p| p.is_file()),
                    "socket": ipc::socket_path().ok(),
                    "log": log_path().ok(),
                    "next": "spotify daemon start",
                    "log_tail": tail(8).unwrap_or_default(),
                }),
                Err(error) => return Err(error),
            };
            let mut value = value;
            value["launch_agent_installed"] = json!(plist_path().is_some_and(|p| p.is_file()));
            ctx.emit(&value, crate::render::daemon_status);
        }
        DaemonCommand::Run => {
            let binary = daemon_binary()?;
            let status = Command::new(binary)
                .status()
                .map_err(|e| Error::internal(e.to_string()))?;
            std::process::exit(status.code().unwrap_or(1));
        }
        DaemonCommand::Install => {
            let _ = stop().await;
            let mut value = install()?;
            let started = start().await?;
            value["status"] = started["status"].clone();
            ctx.emit(&value, |v| format!("Installed launchd agent {LABEL} and started spotify-daemon (pid {}).\nPlist: {}\nLog: {}", v["status"]["pid"], v["plist"].as_str().unwrap_or(""), v["log"].as_str().unwrap_or("")));
        }
        DaemonCommand::Uninstall => {
            let _ = stop().await;
            let removed = plist_path()
                .filter(|p| p.is_file())
                .map(|p| std::fs::remove_file(&p).is_ok())
                .unwrap_or(false);
            let value = json!({"uninstalled": true, "agent_removed": removed});
            ctx.emit(&value, |_| "Removed the launchd agent and stopped spotify-daemon. Triggers stay saved; they resume when the daemon starts again.".into());
        }
        DaemonCommand::Logs { lines } => {
            let lines_out = tail(lines)?;
            let value = json!({"log": log_path()?, "lines": lines_out});
            ctx.emit(&value, |v| {
                v["lines"]
                    .as_array()
                    .map(|l| {
                        l.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                    .unwrap_or_default()
            });
        }
    }
    Ok(())
}

/// Calls the daemon only if it is already running (never starts one).
pub async fn call_if_running(ctx: &Ctx, op: &str, args: Value) -> Option<Value> {
    if probe().await.is_err() {
        return None;
    }
    ipc::call(&request(Some(ctx), op, args), Duration::from_secs(5))
        .await
        .ok()
}

/// Hands a telemetry event to a running daemon (never starts one just for this).
pub fn record_telemetry(ctx: &Ctx, event: silicon_spotify_client::telemetry::Event) {
    let Ok(socket) = ipc::socket_path() else {
        return;
    };
    if !socket.exists() {
        return;
    }
    let Ok(value) = serde_json::to_value(event) else {
        return;
    };
    let request = request(
        Some(ctx),
        "telemetry.record",
        json!({"events": [value], "settings": ctx.settings()}),
    );
    if let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        let _ = runtime.block_on(ipc::call(&request, Duration::from_millis(800)));
    }
}

#[cfg(test)]
mod tests {
    use super::job_pid;

    #[test]
    fn launchd_job_pid_is_read_from_launchctl_print() {
        let running = "gui/501/com.unlikefraction.spotify.daemon = {\n\tactive count = 1\n\tpath = /Users/x/Library/LaunchAgents/com.unlikefraction.spotify.daemon.plist\n\ttype = LaunchAgent\n\tstate = running\n\n\tprogram = /Users/x/.local/bin/spotify-daemon\n\tpid = 4242\n\timmediate reason = speculative\n}\n";
        assert_eq!(job_pid(running), Some(4242));
        let waiting = "gui/501/com.unlikefraction.spotify.daemon = {\n\tactive count = 0\n\tstate = not running\n\tlast exit code = 1\n}\n";
        assert_eq!(job_pid(waiting), None);
    }
}
