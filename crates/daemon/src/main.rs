//! `spotify-daemon`: the always-on half of spotify-cli.
//!
//! One daemon per macOS user serves every Silicon home on the machine. It watches Spotify.app
//! (distributed notifications plus adaptive AppleScript readings), fires playback triggers and
//! delivers them through the backend to Ting, advances the managed queue, keeps a headless
//! spotify_player warm, relays telemetry, and checks for updates hourly.
//!
//! Usage: `spotify-daemon` (foreground; launchd runs it) or `spotify-daemon --version`.
//! The `spotify daemon …` commands start, stop, install and inspect it.

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos;

#[cfg(target_os = "macos")]
mod db;
#[cfg(target_os = "macos")]
mod queue;
#[cfg(target_os = "macos")]
mod server;
#[cfg(target_os = "macos")]
mod service;
#[cfg(target_os = "macos")]
mod triggers;
#[cfg(target_os = "macos")]
mod updater;
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod warm;
#[cfg(target_os = "macos")]
mod watcher;

/// Off macOS the daemon has nothing to watch: it explains that and exits.
#[cfg(not(target_os = "macos"))]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) == Some("--version") {
        println!(
            "{}",
            serde_json::json!({"version": silicon_spotify_client::VERSION, "protocol": silicon_spotify_client::ipc::PROTOCOL})
        );
        return;
    }
    let error = silicon_spotify_client::Error::platform_unsupported("The Spotify daemon");
    eprintln!(
        "{}",
        serde_json::to_string(&serde_json::json!({"error": error})).unwrap_or_default()
    );
    std::process::exit(1);
}

#[cfg(target_os = "macos")]
use std::fs::OpenOptions;
#[cfg(target_os = "macos")]
use std::sync::{Arc, Mutex};
#[cfg(target_os = "macos")]
use std::time::Instant;

#[cfg(target_os = "macos")]
use serde_json::json;
#[cfg(target_os = "macos")]
use silicon_spotify_client::{Error, VERSION, ipc};

#[cfg(target_os = "macos")]
use crate::service::{Daemon, Live, Settings};

/// Timestamped line on stderr (launchd and the CLI launcher send it to `daemon.log`).
#[macro_export]
macro_rules! log {
    ($($arg:tt)*) => {
        eprintln!("{} {}", silicon_spotify_client::model::now_rfc3339(), format!($($arg)*))
    };
}

#[cfg(target_os = "macos")]
fn fail(error: &Error) -> ! {
    eprintln!(
        "{}",
        serde_json::to_string(&json!({"error": error})).unwrap_or_default()
    );
    std::process::exit(error.exit_code().max(1));
}

/// Stops background work, kills the warm spotify_player, removes the socket and exits.
#[cfg(target_os = "macos")]
pub fn shutdown(daemon: &Daemon, code: i32) -> ! {
    daemon.shutdown.notify_waiters();
    if let Some(pid) = daemon
        .warm
        .lock()
        .ok()
        .and_then(|w| w.get("pid").and_then(serde_json::Value::as_u64))
    {
        kill_group(pid);
    }
    if let Ok(path) = ipc::socket_path() {
        let _ = std::fs::remove_file(path);
    }
    log!("stopped");
    std::process::exit(code);
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn kill_group(pid: u64) {
    if let Ok(pid) = libc::pid_t::try_from(pid) {
        // SAFETY: plain syscalls on a pid we spawned; failures are ignored.
        unsafe {
            libc::killpg(pid, libc::SIGTERM);
            libc::kill(pid, libc::SIGTERM);
        }
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
fn private_umask() {
    // SAFETY: umask has no preconditions.
    unsafe {
        libc::umask(0o077);
    }
}

#[cfg(target_os = "macos")]
fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("--version" | "-V") => {
            println!("{}", json!({"version": VERSION, "protocol": ipc::PROTOCOL}));
            return;
        }
        Some("--help" | "-h") => {
            println!(
                "spotify-daemon {VERSION}\n\nThe always-on spotify-cli daemon (one per macOS user). You normally never run it by hand:\n  spotify daemon start      start it in the background (the CLI also does this on demand)\n  spotify daemon install    run it at login through launchd\n  spotify daemon status     what it is doing\n  spotify daemon logs       its log (~/.silicon-spotify/daemon.log)\n\nFlags: --version prints {{\"version\",\"protocol\"}}; no flags runs it in the foreground."
            );
            return;
        }
        Some(other) => fail(&Error::invalid(
            format!("Unknown argument `{other}`."),
            "spotify-daemon takes no arguments except --version and --help.",
        )),
        None => {}
    }
    if !cfg!(target_os = "macos") {
        fail(&Error::platform_unsupported("The Spotify daemon"));
    }
    private_umask();
    let dir = ipc::daemon_dir().unwrap_or_else(|e| fail(&e));
    if let Err(error) = std::fs::create_dir_all(&dir) {
        fail(&Error::new(
            "daemon_dir",
            format!("Cannot create {}: {error}.", dir.display()),
            "Check your home directory permissions.",
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    // Single instance: the lock file is kept forever and held for the process lifetime.
    let lock_path = dir.join("daemon.lock");
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&lock_path)
        .unwrap_or_else(|e| {
            fail(&Error::new(
                "daemon_dir",
                format!("Cannot open {}: {e}.", lock_path.display()),
                "Check permissions.",
            ))
        });
    if lock.try_lock().is_err() {
        fail(&Error::new(
            "daemon_running",
            "Another spotify-daemon is already running for this user.",
            "Use `spotify daemon status`; `spotify daemon restart` replaces it.",
        ));
    }
    let db = db::Db::open(&dir.join("daemon.sqlite")).unwrap_or_else(|e| fail(&e));
    let mut live = Live {
        tracker: db.get("tracker").ok().flatten().unwrap_or_default(),
        queue: db.get("queue_state").ok().flatten().unwrap_or_default(),
        ..Live::default()
    };
    live.triggers = db.triggers(true, None).unwrap_or_default();
    let settings: Settings = db.get("settings").ok().flatten().unwrap_or_default();
    let (events, _) = tokio::sync::broadcast::channel(256);
    #[cfg(target_os = "macos")]
    let script: Arc<dyn silicon_spotify_client::applescript::Runner> =
        Arc::new(macos::MainThreadScript);
    #[cfg(not(target_os = "macos"))]
    let script: Arc<dyn silicon_spotify_client::applescript::Runner> =
        Arc::new(silicon_spotify_client::applescript::Osascript::default());
    let daemon = Arc::new(Daemon {
        db,
        dir: dir.clone(),
        script,
        started_at: silicon_spotify_client::model::now_rfc3339(),
        started: Instant::now(),
        live: Mutex::new(live),
        observe_lock: Mutex::new(()),
        nudge: tokio::sync::Notify::new(),
        deliver: tokio::sync::Notify::new(),
        events,
        settings: Mutex::new(settings),
        warm: Mutex::new(json!({"state": "starting"})),
        update: Mutex::new(json!({"state": "not_checked_yet"})),
        shutdown: tokio::sync::Notify::new(),
    });
    log!(
        "spotify-daemon {VERSION} starting (pid {}, dir {})",
        std::process::id(),
        dir.display()
    );

    #[cfg(target_os = "macos")]
    {
        let observer = Arc::clone(&daemon);
        macos::observe_spotify(move || {
            observer.live().notifications += 1;
            observer.nudge.notify_one();
        });
    }

    let background = Arc::clone(&daemon);
    std::thread::Builder::new()
        .name("runtime".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .worker_threads(4)
                .build()
                .unwrap_or_else(|e| fail(&Error::internal(format!("tokio runtime: {e}"))));
            runtime.block_on(async move {
                let d = Arc::clone(&background);
                tokio::spawn(async move {
                    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
                    let interrupt = tokio::signal::ctrl_c();
                    tokio::select! {
                        _ = interrupt => {}
                        () = async { if let Some(t) = term.as_mut() { t.recv().await; } else { std::future::pending::<()>().await } } => {}
                    }
                    shutdown(&d, 0);
                });
                tokio::spawn(watcher::run(Arc::clone(&background)));
                tokio::spawn(triggers::deliver_forever(Arc::clone(&background)));
                tokio::spawn(triggers::relay_telemetry(Arc::clone(&background)));
                tokio::spawn(warm::run(Arc::clone(&background)));
                tokio::spawn(updater::run(Arc::clone(&background)));
                let pruner = Arc::clone(&background);
                tokio::spawn(async move {
                    loop {
                        let cutoff = time::OffsetDateTime::now_utc() - time::Duration::days(30);
                        let _ = pruner.db.prune(&silicon_spotify_client::model::rfc3339(cutoff));
                        tokio::time::sleep(std::time::Duration::from_secs(6 * 3600)).await;
                    }
                });
                if let Err(error) = server::serve(Arc::clone(&background)).await {
                    log!("server failed: {error}");
                    shutdown(&background, 1);
                }
            });
        })
        .unwrap_or_else(|e| fail(&Error::internal(format!("cannot start the runtime thread: {e}"))));

    #[cfg(target_os = "macos")]
    macos::run_main_loop();
    #[cfg(not(target_os = "macos"))]
    loop {
        std::thread::park();
    }
}
