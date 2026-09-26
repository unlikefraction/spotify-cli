//! Talking to, starting and installing `spotify-daemon`.

use std::fs::OpenOptions;
use std::path::PathBuf;
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
        Err(error) if error.code == "daemon_unavailable" => start().await.map(drop),
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
    stop().await?;
    start().await?;
    let still_older = probe().await.map_or(true, |s| older(&s));
    if still_older {
        // The launchd agent may point at an older install: re-point it at this CLI's daemon.
        if plist_path().is_some_and(|p| p.is_file()) {
            let _ = stop().await;
            install()?;
            start().await?;
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
        let target = format!("gui/{}/{LABEL}", uid());
        let kick = launchctl(&["kickstart", &target]);
        if !kick.as_ref().is_ok_and(|o| o.status.success()) {
            // Not loaded (e.g. after `launchctl bootout`): load it.
            if let Some(plist) = plist_path() {
                let _ = launchctl(&[
                    "bootstrap",
                    &format!("gui/{}", uid()),
                    &plist.to_string_lossy(),
                ]);
            }
        }
        "launchd"
    } else {
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
            .current_dir(&dir);
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
        "spawn"
    };
    let deadline = Instant::now() + Duration::from_secs(12);
    loop {
        tokio::time::sleep(Duration::from_millis(150)).await;
        if let Ok(status) = probe().await {
            return Ok(json!({"started": true, "via": via, "status": status}));
        }
        if Instant::now() > deadline {
            let tail = tail(20).unwrap_or_default();
            return Err(Error::daemon_unavailable(format!(
                "spotify-daemon did not come up within 12 s (started via {via})."
            ))
            .with_details(json!({"log_tail": tail})));
        }
    }
}

/// Asks the daemon to stop and waits for its socket to disappear.
///
/// # Errors
/// When it refuses to stop.
pub async fn stop() -> Result<Value> {
    // A launchd KeepAlive agent would restart it; stop the job instead.
    if plist_path().is_some_and(|p| p.is_file()) {
        let _ = launchctl(&["bootout", &format!("gui/{}/{LABEL}", uid())]);
    }
    if probe().await.is_err() {
        return Ok(json!({"stopped": true, "was_running": false}));
    }
    let _ = ipc::call(
        &request(None, "daemon.shutdown", json!({})),
        Duration::from_secs(5),
    )
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while probe().await.is_ok() {
        if Instant::now() > deadline {
            return Err(Error::new(
                "daemon_stuck",
                "spotify-daemon did not stop within 10 s.",
                "Find it with `pgrep -fl spotify-daemon` and kill it.",
            ));
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
    Ok(json!({"stopped": true, "was_running": true}))
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
    let _ = launchctl(&["bootout", &format!("{domain}/{LABEL}")]);
    let _ = launchctl(&["enable", &format!("{domain}/{LABEL}")]);
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
            stop().await?;
            let value = start().await?;
            ctx.emit(&value, |v| {
                format!(
                    "spotify-daemon restarted (pid {}, v{}).",
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
