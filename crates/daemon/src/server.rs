//! The Unix-socket server and the daemon-level ops (status, doctor, telemetry, update, shutdown).

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::Deserialize;
use serde_json::{Value, json};
use silicon_spotify_client::ipc::{self, MAX_FRAME, PROTOCOL, Reply, Request};
use silicon_spotify_client::model::PlayerState;
use silicon_spotify_client::{Error, Result, VERSION};
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::log;
use crate::service::{self, Daemon, args, blocking, settings_of};

/// Binds the socket (0600) and serves forever.
///
/// # Errors
/// When the socket cannot be bound.
pub async fn serve(daemon: Arc<Daemon>) -> Result<()> {
    let path = ipc::socket_path()?;
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path).map_err(|error| {
        Error::new(
            "daemon_bind_failed",
            format!("Cannot listen on {}: {error}.", path.display()),
            "Check that the directory exists and is writable by you (see `spotify daemon status`).",
        )
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    log!("listening on {}", path.display());
    loop {
        match listener.accept().await {
            Ok((stream, _)) => {
                let daemon = Arc::clone(&daemon);
                tokio::spawn(async move { connection(daemon, stream).await });
            }
            Err(error) => {
                log!("accept failed: {error}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        }
    }
}

async fn connection(daemon: Arc<Daemon>, stream: UnixStream) {
    if let Ok(cred) = stream.peer_cred()
        && cred.uid() != ipc::current_uid()
    {
        log!("refused a connection from uid {}", cred.uid());
        return;
    }
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read).take(MAX_FRAME as u64);
    let mut line = String::new();
    let read = tokio::time::timeout(Duration::from_secs(30), reader.read_line(&mut line)).await;
    if !matches!(read, Ok(Ok(n)) if n > 0) {
        return;
    }
    let reply = match serde_json::from_str::<Request>(&line) {
        Ok(request) if request.v != PROTOCOL => Reply::err(
            request.id,
            Error::new(
                "protocol_mismatch",
                format!(
                    "The CLI speaks protocol {} but this daemon speaks {PROTOCOL}.",
                    request.v
                ),
                "Run `spotify daemon restart` so the daemon matches the CLI.",
            ),
        ),
        Ok(request) => {
            let started = Instant::now();
            let id = request.id.clone();
            let mut result = dispatch(&daemon, &request).await;
            if let Err(error) = &mut result
                && error.code == "spotify_auth_required"
                && !request.op.starts_with("spotify.auth.")
            {
                start_spotify_auth(&daemon, &request, error).await;
            }
            record(&daemon, &request, &result, started.elapsed());
            match result {
                Ok(data) => Reply::ok(id, data),
                Err(error) => Reply::err(id, error),
            }
        }
        Err(error) => Reply::err(
            String::new(),
            Error::invalid(
                format!("Unreadable request: {error}."),
                "Use the spotify CLI of the same version as the daemon.",
            ),
        ),
    };
    let mut bytes = serde_json::to_vec(&reply).unwrap_or_else(|_| b"{}".to_vec());
    bytes.push(b'\n');
    let _ = write.write_all(&bytes).await;
    let _ = write.flush().await;
}

/// The product rule: when a Silicon needs spotify_player and it is not signed in, start the
/// sign-in for it (a browser tab on this Mac for a Carbon to approve), at most every 10 minutes.
async fn start_spotify_auth(daemon: &Arc<Daemon>, request: &Request, error: &mut Error) {
    let now = i64::try_from(silicon_spotify_client::model::now_ms()).unwrap_or(0);
    let last: i64 = daemon
        .db
        .get("spotify_auth_started_ms")
        .ok()
        .flatten()
        .unwrap_or(0);
    let settings = settings_of(daemon, request);
    let dir = daemon.dir.clone();
    let started = if now - last > 600_000 {
        blocking(move || {
            let player = settings.player()?;
            player.spawn_authenticate(&dir.join("spotify-auth.log"))
        })
        .await
        .ok()
    } else {
        None
    };
    if started.is_some() {
        let _ = daemon.db.put("spotify_auth_started_ms", &now);
        log!(
            "spotify_player is not signed in; started `spotify_player authenticate` for {}",
            request.op
        );
    }
    let started_at = if started.is_some() { now } else { last };
    error.message = format!(
        "{} spotify-cli opened Spotify's sign-in page in the browser on this Mac.",
        error.message
    );
    error.hint = "A Carbon must click Agree on that page (once). Then retry; `spotify auth status` confirms. If no tab appeared, run `spotify auth login`.".into();
    error.retryable = true;
    error.details = Some(
        json!({"auth_login_started": true, "started_at_ms": started_at, "log": daemon.dir.join("spotify-auth.log")}),
    );
}

fn record(daemon: &Daemon, request: &Request, result: &Result<Value>, elapsed: Duration) {
    if request.op.starts_with("telemetry.") || request.op == "daemon.status" {
        return;
    }
    if !silicon_spotify_client::telemetry::enabled(daemon.settings().telemetry) {
        return;
    }
    let mut builder = silicon_spotify_client::telemetry::Builder::new("daemon", "production");
    if let Some(trace) = &request.trace_id {
        builder.trace_id.clone_from(trace);
    }
    let mut context = serde_json::Map::new();
    context.insert("op".into(), json!(request.op));
    if let Ok(value) = result {
        if let Some(via) = value.get("via") {
            context.insert("via".into(), via.clone());
        }
        if let Some(fallback) = value.pointer("/fallback/reason/code") {
            context.insert("fallback_reason".into(), fallback.clone());
        }
    }
    let event = builder.event(
        "daemon.op.completed",
        &request.op,
        if result.is_ok() { "ok" } else { "error" },
        result.as_ref().err().map(|e| e.code.as_str()),
        u64::try_from(elapsed.as_millis()).ok(),
        context,
    );
    if let Ok(value) = serde_json::to_value(event) {
        let _ = daemon.db.push_telemetry(&[value]);
    }
}

async fn dispatch(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    match request.op.as_str() {
        "daemon.status" => status(daemon).await,
        "daemon.shutdown" => {
            let d = Arc::clone(daemon);
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(100)).await;
                crate::shutdown(&d, 0);
            });
            Ok(json!({"stopping": true}))
        }
        "doctor" => doctor(daemon, request).await,
        "telemetry.record" => {
            #[derive(Deserialize)]
            struct A {
                events: Vec<Value>,
            }
            let a: A = args(request)?;
            let settings = settings_of(daemon, request);
            if silicon_spotify_client::telemetry::enabled(settings.telemetry) {
                daemon.db.push_telemetry(&a.events)?;
            }
            Ok(json!({"queued": a.events.len()}))
        }
        "telemetry.clear" => {
            daemon.db.clear_telemetry()?;
            Ok(json!({"cleared": true}))
        }
        "update.check" | "update.apply" => {
            let _ = settings_of(daemon, request);
            crate::updater::check(daemon, request.op == "update.apply")
                .await
                .inspect(|status| {
                    if status.get("restarting").and_then(Value::as_bool) == Some(true) {
                        let d = Arc::clone(daemon);
                        tokio::spawn(async move {
                            tokio::time::sleep(Duration::from_millis(200)).await;
                            crate::shutdown(&d, 0);
                        });
                    }
                })
        }
        _ => {
            if let Some(result) = service::handle(daemon, request).await {
                return result;
            }
            if let Some(result) = crate::triggers::handle(daemon, request).await {
                return result;
            }
            Err(Error::new(
                "unknown_op",
                format!("This daemon (v{VERSION}) does not know `{}`.", request.op),
                "The CLI is newer than the daemon: run `spotify daemon restart`.",
            ))
        }
    }
}

async fn status(daemon: &Arc<Daemon>) -> Result<Value> {
    let (last, readings, notifications, watch_error, queue, managed_now) = {
        let live = daemon.live();
        (
            live.last.clone(),
            live.readings,
            live.notifications,
            live.watch_error.clone(),
            live.queue.items.len(),
            live.queue.managed_now.clone(),
        )
    };
    let warm = daemon
        .warm
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    let update = daemon
        .update
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    Ok(json!({
        "running": true,
        "version": VERSION,
        "protocol": PROTOCOL,
        "pid": std::process::id(),
        "started_at": daemon.started_at,
        "uptime_s": daemon.started.elapsed().as_secs(),
        "dir": daemon.dir,
        "socket": ipc::socket_path().ok(),
        "log": daemon.dir.join("daemon.log"),
        "install_method": crate::updater::install_method().0,
        "spotify": last.as_ref().map(|p| json!({"state": p.state, "track": p.track.as_ref().map(|t| t.label()), "observed_at": p.observed_at})),
        "readings": readings,
        "notifications": notifications,
        "watch_error": watch_error,
        "managed_queue": queue,
        "managed_now": managed_now,
        "counts": daemon.db.counts()?,
        "warm_spotify_player": warm,
        "automation": crate::macos::automation_state().as_str(),
        "update": update,
    }))
}

fn check(name: &str, ok: bool, detail: Value, fix: &str) -> Value {
    json!({"check": name, "ok": ok, "detail": detail, "fix": if ok { Value::Null } else { json!(fix) }})
}

async fn doctor(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    let settings = settings_of(daemon, request);
    let d = Arc::clone(daemon);
    blocking(move || {
        let mut checks = Vec::new();
        let installed = ["/Applications/Spotify.app", "/Applications/Spotify.app/Contents/MacOS/Spotify"]
            .iter()
            .any(|p| std::path::Path::new(p).exists())
            || std::env::var_os("HOME").is_some_and(|h| std::path::Path::new(&h).join("Applications/Spotify.app").exists());
        checks.push(check("spotify_app_installed", installed, json!(installed), "Install Spotify: https://www.spotify.com/download/mac/ or `brew install --cask spotify`."));
        match d.read() {
            Ok(playback) => {
                let running = playback.state != PlayerState::NotRunning;
                checks.push(check("spotify_app_running", running, json!(playback.state), "Run `spotify launch` (starts Spotify hidden)."));
            }
            // Reported below as the permission question it usually is.
            Err(error) if matches!(error.code.as_str(), "automation_permission_denied" | "timeout") => {}
            Err(error) => checks.push(check("spotify_app_responding", false, serde_json::to_value(&error)?, &error.hint)),
        }
        let automation = crate::macos::automation_state();
        match automation {
            crate::macos::Automation::Denied => {
                let error = silicon_spotify_client::applescript::classify("", Some(-1743));
                checks.push(check("automation_permission", false, json!({"state": automation.as_str()}), &error.hint));
            }
            crate::macos::Automation::NotAnswering => checks.push(check(
                "automation_permission",
                false,
                json!({"state": automation.as_str()}),
                "Spotify is not answering Apple Events. If macOS shows \"spotify-daemon\" wants access to control \"Spotify\", click Allow (it may be behind other windows); otherwise Spotify is busy, retry.",
            )),
            _ => checks.push(check("automation_permission", true, json!({"state": automation.as_str()}), "")),
        }
        match settings.player() {
            Ok(player) => {
                checks.push(check("spotify_player_installed", true, json!({"binary": player.binary, "version": player.version().ok()}), ""));
                let authed = player.has_cached_token();
                checks.push(check("spotify_player_signed_in", authed, json!({"cache_folder": player.cache_folder()}), "Run `spotify auth login` (a Carbon clicks Agree once in the browser)."));
            }
            Err(error) => checks.push(check("spotify_player_installed", false, serde_json::to_value(&error)?, &error.hint)),
        }
        let warm = d.warm.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        let warm_ok = warm.get("state").and_then(Value::as_str) == Some("running");
        checks.push(check("spotify_player_warm_instance", warm_ok, warm, "Optional: keeps Web API commands fast. It starts once spotify_player is signed in; see ~/.silicon-spotify/daemon.log."));
        let notifications = d.live().notifications;
        checks.push(check("playback_notifications", true, json!({"received": notifications}), ""));
        checks.push(check("daemon", true, json!({"version": VERSION, "pid": std::process::id()}), ""));
        let ok = checks.iter().all(|c| c["ok"] == json!(true) || c["check"] == json!("spotify_player_warm_instance"));
        Ok(json!({"ok": ok, "checks": checks}))
    })
    .await
}
