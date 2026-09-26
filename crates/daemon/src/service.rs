//! The daemon's shared state and request handlers (everything except triggers).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silicon_spotify_client::applescript::{self, Runner};
use silicon_spotify_client::control::{Controller, PlayTarget, RepeatMode, Strategy, VolumeTarget};
use silicon_spotify_client::ipc::Request;
use silicon_spotify_client::model::{Item, Playback, PlayerState, Track, WebPlayback, now_rfc3339};
use silicon_spotify_client::player::{Output, SpotifyPlayer};
use silicon_spotify_client::timing::SeekTarget;
use silicon_spotify_client::trigger::Tracker;
use silicon_spotify_client::uri::{Kind, SpotifyUri};
use silicon_spotify_client::{Error, Result};
use tokio::sync::{Notify, broadcast};

use crate::db::{Db, StoredTrigger};
use crate::log;
use crate::queue::{QueueItem, QueueState};

/// Per-request control settings (from the caller's `spotify config`).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    /// Control path.
    #[serde(default)]
    pub strategy: Option<Strategy>,
    /// Launch Spotify.app when closed.
    #[serde(default)]
    pub launch_spotify: Option<bool>,
    /// spotify_player verification timeout.
    #[serde(default)]
    pub verify_timeout_ms: Option<u64>,
    /// spotify_player binary.
    #[serde(default)]
    pub spotify_player_binary: Option<String>,
    /// spotify_player `-c`.
    #[serde(default)]
    pub spotify_player_config_dir: Option<String>,
    /// spotify_player `-C`.
    #[serde(default)]
    pub spotify_player_cache_dir: Option<String>,
    /// Caller's telemetry preference.
    #[serde(default)]
    pub telemetry: Option<bool>,
    /// Caller's backend origin (used for telemetry relay).
    #[serde(default)]
    pub api_url: Option<String>,
    /// Caller's auto-update preference.
    #[serde(default)]
    pub auto_update: Option<bool>,
}

impl Settings {
    /// Locates spotify_player with these settings.
    ///
    /// # Errors
    /// `spotify_player_missing`.
    pub fn player(&self) -> Result<SpotifyPlayer> {
        let mut player = SpotifyPlayer::locate(
            self.spotify_player_binary
                .as_deref()
                .map(std::path::Path::new),
        )?;
        player.config_dir = self.spotify_player_config_dir.as_ref().map(PathBuf::from);
        player.cache_dir = self.spotify_player_cache_dir.as_ref().map(PathBuf::from);
        Ok(player)
    }

    /// spotify_player ready for Web API calls (installed and signed in).
    ///
    /// # Errors
    /// `spotify_player_missing` or `spotify_auth_required`.
    pub fn authed_player(&self) -> Result<SpotifyPlayer> {
        let player = self.player()?;
        player.require_auth()?;
        Ok(player)
    }
}

/// Mutable live state guarded by one mutex.
#[derive(Default)]
pub struct Live {
    /// Play tracker (persisted).
    pub tracker: Tracker,
    /// Active triggers (cache of the DB).
    pub triggers: Vec<StoredTrigger>,
    /// Latest reading.
    pub last: Option<Playback>,
    /// When it was read.
    pub last_at: Option<Instant>,
    /// Managed queue state machine (persisted).
    pub queue: QueueState,
    /// Last watcher error (surfaced in status).
    pub watch_error: Option<Error>,
    /// Notifications received from Spotify.
    pub notifications: u64,
    /// Readings taken.
    pub readings: u64,
    /// Lookups of a managed item's missing facts so far, by item id (in memory; see
    /// [`fill_facts_later`]).
    pub fact_tries: std::collections::HashMap<String, u32>,
    /// Whether a background lookup of missing queue-item facts runs.
    pub facts_backfill: bool,
}

/// Shared daemon state.
pub struct Daemon {
    /// Durable state.
    pub db: Db,
    /// `~/.silicon-spotify`.
    pub dir: PathBuf,
    /// AppleScript runner (main thread on macOS).
    pub script: Arc<dyn Runner>,
    /// When the daemon started.
    pub started_at: String,
    /// Monotonic start.
    pub started: Instant,
    /// Live state.
    pub live: Mutex<Live>,
    /// Serializes read-and-process so readings are applied in order.
    pub observe_lock: Mutex<()>,
    /// Serializes item changes: managed-queue hand-offs (`spotify next` and the watcher's) and
    /// explicit `play <item>` and `previous`, so two hand-offs never send the same item and no two
    /// changes reach Spotify out of order.
    pub hand_off: Mutex<()>,
    /// Library writes and reads in flight, for [`LIBRARY_SETTLE`] (see
    /// [`Daemon::after_library_write`] and [`Daemon::before_library_read`]).
    pub library: Mutex<LibrarySettle>,
    /// Wakes the watcher.
    pub nudge: Notify,
    /// Wakes the delivery worker.
    pub deliver: Notify,
    /// Firing updates for `trigger wait` and `trigger test`.
    pub events: broadcast::Sender<Value>,
    /// Most recent caller settings (used by background work).
    pub settings: Mutex<Settings>,
    /// spotify_player warm instance status.
    pub warm: Mutex<Value>,
    /// The warm spotify_player's pid (0 = none), for shutdown.
    pub warm_pid: std::sync::atomic::AtomicU32,
    /// Update checker status.
    pub update: Mutex<Value>,
    /// Stop signal.
    pub shutdown: Notify,
}

impl Daemon {
    /// Locks live state.
    pub fn live(&self) -> std::sync::MutexGuard<'_, Live> {
        self.live
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Current background settings.
    pub fn settings(&self) -> Settings {
        self.settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    /// Runs a closure with a controller built from `settings`.
    pub fn with_controller<T>(
        &self,
        settings: &Settings,
        f: impl FnOnce(&Controller<'_>) -> Result<T>,
    ) -> Result<T> {
        let player = settings.authed_player();
        let controller = Controller {
            script: self.script.as_ref(),
            player: player.as_ref().map_err(Clone::clone),
            strategy: settings.strategy.unwrap_or_default(),
            verify_timeout: Duration::from_millis(settings.verify_timeout_ms.unwrap_or(2500)),
            launch_spotify: settings.launch_spotify.unwrap_or(true),
        };
        f(&controller)
    }

    /// Reads Spotify.app now.
    ///
    /// # Errors
    /// AppleScript errors.
    pub fn read(&self) -> Result<Playback> {
        let output = self.script.run(&applescript::status())?;
        applescript::parse_status(&output)
    }

    /// Persists queue state.
    pub fn save_queue(&self, live: &Live) {
        if let Err(error) = self.db.put("queue_state", &live.queue) {
            log!("could not persist the queue: {error}");
        }
    }

    /// Records that a library or playlist change just finished (see [`LIBRARY_SETTLE`]).
    pub fn after_library_write(&self) {
        self.library_settle().wrote(Instant::now());
    }

    /// Before reading the library or a playlist: waits until [`LIBRARY_SETTLE`] has passed since
    /// the last change, so the read cannot get spotify_player's copy of an older response. Keep
    /// the returned guard until the read has finished.
    #[must_use = "the read is in flight until the guard is dropped"]
    pub fn before_library_read(&self) -> LibraryRead<'_> {
        loop {
            let wait = {
                let mut settle = self.library_settle();
                match settle.delay(Instant::now()) {
                    None => {
                        settle.reads += 1;
                        return LibraryRead(self);
                    }
                    Some(wait) => wait,
                }
            };
            // Another change may finish meanwhile: look again after the wait.
            std::thread::sleep(wait);
        }
    }

    fn library_settle(&self) -> std::sync::MutexGuard<'_, LibrarySettle> {
        self.library
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// A library or playlist read in flight (from [`Daemon::before_library_read`]).
pub struct LibraryRead<'a>(&'a Daemon);

impl Drop for LibraryRead<'_> {
    fn drop(&mut self) {
        self.0.library_settle().read_finished(Instant::now());
    }
}

/// When library reads may fetch fresh, given spotify_player's shared responses
/// ([`LIBRARY_SETTLE`]).
#[derive(Debug, Default)]
pub struct LibrarySettle {
    /// When a change last finished, or a read that was in flight when one finished.
    last_write: Option<Instant>,
    /// Reads in flight.
    reads: usize,
    /// A change finished while reads were in flight. Each of them may complete after the change
    /// and still carry the older data, which spotify_player then shares for another second, so
    /// their completion counts as a change too.
    overlapped: bool,
}

impl LibrarySettle {
    /// A change finished at `now`.
    fn wrote(&mut self, now: Instant) {
        self.last_write = Some(now);
        if self.reads > 0 {
            self.overlapped = true;
        }
    }

    /// How long a read starting at `now` must wait (`None`: it need not).
    fn delay(&self, now: Instant) -> Option<Duration> {
        settle_delay(self.last_write, now)
    }

    /// A read finished at `now`.
    fn read_finished(&mut self, now: Instant) {
        self.reads = self.reads.saturating_sub(1);
        if self.overlapped {
            self.last_write = Some(now);
            self.overlapped = self.reads > 0;
        }
    }
}

/// spotify_player hands every identical Web API GET that arrives within 1 s of a response's
/// completion that same response, and a write (POST/PUT/DELETE) does not drop it. So a
/// `playlist list` right after `playlist delete` could still show the playlist when another list
/// finished just before the delete. A read issued this long after the write finished always
/// fetches fresh: a shared response completed before the write did, or it belongs to a read that
/// was in flight during the write, and this long after that read finished counts instead
/// ([`LibrarySettle`]).
pub const LIBRARY_SETTLE: Duration = Duration::from_millis(1_100);

/// How long a read must wait after the last library write (`None`: it need not).
fn settle_delay(last_write: Option<Instant>, now: Instant) -> Option<Duration> {
    let ready = last_write? + LIBRARY_SETTLE;
    (ready > now).then(|| ready - now)
}

/// Parses `args` into `T` with a clear error.
///
/// # Errors
/// `invalid_input` naming the op.
pub fn args<T: for<'de> Deserialize<'de>>(request: &Request) -> Result<T> {
    let value = if request.args.is_null() {
        json!({})
    } else {
        request.args.clone()
    };
    serde_json::from_value(value).map_err(|error| {
        Error::invalid(
            format!("Bad arguments for daemon op `{}`: {error}.", request.op),
            "The CLI and daemon may be different versions; run `spotify daemon restart`.",
        )
    })
}

#[derive(Deserialize)]
struct WithSettings {
    #[serde(default)]
    settings: Settings,
}

/// Extracts caller settings and remembers them for background work.
pub fn settings_of(daemon: &Daemon, request: &Request) -> Settings {
    let settings = serde_json::from_value::<WithSettings>(request.args.clone())
        .map(|w| w.settings)
        .unwrap_or_default();
    if settings.strategy.is_some() || settings.api_url.is_some() || settings.telemetry.is_some() {
        *daemon
            .settings
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = settings.clone();
        let _ = daemon.db.put("settings", &settings);
    }
    settings
}

/// Runs a blocking closure on the blocking pool.
///
/// # Errors
/// The closure's error, or `internal` if the task panicked.
pub async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|error| Error::internal(format!("background task failed: {error}")))?
}

fn outcome_json(outcome: &silicon_spotify_client::control::Outcome) -> Value {
    serde_json::to_value(outcome).unwrap_or(Value::Null)
}

/// Dispatches player/library ops. Returns `None` for ops handled elsewhere.
pub async fn handle(daemon: &Arc<Daemon>, request: &Request) -> Option<Result<Value>> {
    let settings = settings_of(daemon, request);
    let d = Arc::clone(daemon);
    let result = match request.op.as_str() {
        "player.status" => {
            #[derive(Deserialize)]
            struct A {
                #[serde(default)]
                full: bool,
            }
            let a: A = match args(request) {
                Ok(a) => a,
                Err(e) => return Some(Err(e)),
            };
            let s = settings.clone();
            blocking(move || {
                let (playback, warnings) = if a.full {
                    d.with_controller(&s, |c| c.status_full())?
                } else {
                    (d.read()?, Vec::new())
                };
                let live = d.live();
                Ok(json!({
                    "playback": playback,
                    "warnings": warnings,
                    "managed_queue": live.queue.items.len(),
                    "managed_now": live.queue.managed_now,
                }))
            })
            .await
        }
        "player.play" => {
            #[derive(Deserialize)]
            struct A {
                target: PlayTarget,
            }
            let a: A = match args(request) {
                Ok(a) => a,
                Err(e) => return Some(Err(e)),
            };
            let s = settings.clone();
            let result = blocking(move || {
                if matches!(a.target, PlayTarget::Resume) {
                    // Resuming changes no item: nothing for the queue to know.
                    return d
                        .with_controller(&s, |c| c.play(&a.target))
                        .map(|o| outcome_json(&o));
                }
                explicit_change(&d, |d| {
                    let outcome = d.with_controller(&s, |c| c.play(&a.target))?;
                    // Starting something else abandons the managed-queue resume point, but only
                    // once it started: a failed play leaves playback, and so where the queue
                    // returns to, as it was.
                    let mut live = d.live();
                    live.queue.resume = None;
                    d.save_queue(&live);
                    Ok(outcome)
                })
            })
            .await;
            daemon.nudge.notify_one();
            result
        }
        "player.pause" => control(daemon, settings, |c| c.pause()).await,
        "player.toggle" => control(daemon, settings, |c| c.toggle()).await,
        "player.previous" => {
            let d = Arc::clone(daemon);
            let result = blocking(move || {
                explicit_change(&d, |d| d.with_controller(&settings, |c| c.previous()))
            })
            .await;
            daemon.nudge.notify_one();
            result
        }
        "player.next" => next(daemon, settings).await,
        "player.seek" => {
            #[derive(Deserialize)]
            struct A {
                target: SeekTarget,
            }
            match args::<A>(request) {
                Ok(a) => control(daemon, settings, move |c| c.seek(a.target)).await,
                Err(e) => Err(e),
            }
        }
        "player.volume" => {
            #[derive(Deserialize)]
            struct A {
                target: VolumeTarget,
            }
            match args::<A>(request) {
                Ok(a) => control(daemon, settings, move |c| c.volume(a.target)).await,
                Err(e) => Err(e),
            }
        }
        "player.shuffle" => {
            #[derive(Deserialize)]
            struct A {
                on: Option<bool>,
            }
            match args::<A>(request) {
                Ok(a) => control(daemon, settings, move |c| c.shuffle(a.on)).await,
                Err(e) => Err(e),
            }
        }
        "player.repeat" => {
            #[derive(Deserialize)]
            struct A {
                mode: RepeatMode,
            }
            match args::<A>(request) {
                Ok(a) => control(daemon, settings, move |c| c.repeat(a.mode)).await,
                Err(e) => Err(e),
            }
        }
        "player.like" => {
            #[derive(Deserialize)]
            struct A {
                like: bool,
            }
            match args::<A>(request) {
                Ok(a) => {
                    let result = control(daemon, settings, move |c| c.like(a.like)).await;
                    daemon.after_library_write();
                    result
                }
                Err(e) => Err(e),
            }
        }
        "spotify.launch" => {
            blocking(move || {
                // Already running: nothing to launch (a read error falls through to `open`).
                if let Ok(playback) = d.read()
                    && playback.state != PlayerState::NotRunning
                {
                    return Ok(json!({"launched": false, "already_running": true, "playback": playback}));
                }
                silicon_spotify_client::control::launch_spotify()?;
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    std::thread::sleep(Duration::from_millis(400));
                    let playback = d.read()?;
                    if playback.state != PlayerState::NotRunning {
                        return Ok(json!({"launched": true, "already_running": false, "playback": playback}));
                    }
                    if Instant::now() > deadline {
                        return Err(Error::new(
                            "spotify_not_running",
                            "Spotify.app did not start within 20 s.",
                            "Open it manually and sign in.",
                        )
                        .retryable());
                    }
                }
            })
            .await
        }
        "track.info" => {
            #[derive(Deserialize)]
            struct A {
                uri: Option<SpotifyUri>,
            }
            let a: A = match args(request) {
                Ok(a) => a,
                Err(e) => return Some(Err(e)),
            };
            track_info(&d, settings.clone(), a.uri).await
        }
        "lyrics" => {
            #[derive(Deserialize)]
            struct A {
                uri: Option<SpotifyUri>,
            }
            let a: A = match args(request) {
                Ok(a) => a,
                Err(e) => return Some(Err(e)),
            };
            let s = settings.clone();
            blocking(move || lyrics(&d, &s, a.uri.as_ref())).await
        }
        "search" => {
            #[derive(Deserialize)]
            struct A {
                query: String,
                #[serde(default)]
                kinds: Vec<Kind>,
                #[serde(default)]
                limit: Option<usize>,
            }
            let a: A = match args(request) {
                Ok(a) => a,
                Err(e) => return Some(Err(e)),
            };
            let s = settings.clone();
            blocking(move || search(&s, &a.query, &a.kinds, a.limit.unwrap_or(10))).await
        }
        "devices.list" => {
            let s = settings.clone();
            blocking(move || {
                let value = s.authed_player()?.json(&["get", "key", "devices"])?;
                let devices: Vec<_> = value
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(silicon_spotify_client::model::Device::from_json)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(json!({"devices": devices}))
            })
            .await
        }
        "devices.connect" => {
            #[derive(Deserialize)]
            struct A {
                id: Option<String>,
                name: Option<String>,
            }
            let a: A = match args(request) {
                Ok(a) => a,
                Err(e) => return Some(Err(e)),
            };
            let s = settings.clone();
            blocking(move || {
                let player = s.authed_player()?;
                let output = match (&a.id, &a.name) {
                    (Some(id), _) => player.run(&["connect", "--id", id])?,
                    (None, Some(name)) => player.run(&["connect", "--name", name])?,
                    (None, None) => {
                        return Err(Error::invalid(
                            "Give a device id or name.",
                            "List them with `spotify devices`.",
                        ));
                    }
                };
                Ok(json!({"connected": a.id.or(a.name), "message": output.stdout}))
            })
            .await
        }
        "library.get" => {
            #[derive(Deserialize)]
            struct A {
                key: String,
                #[serde(default)]
                limit: Option<usize>,
            }
            let a: A = match args(request) {
                Ok(a) => a,
                Err(e) => return Some(Err(e)),
            };
            let s = settings.clone();
            blocking(move || {
                let _read = d.before_library_read();
                library(&s, &a.key, a.limit)
            })
            .await
        }
        "queue.list" => queue_list(daemon, settings).await,
        "queue.add" => queue_add(daemon, request, settings).await,
        "queue.remove" | "queue.move" | "queue.clear" => queue_edit(daemon, request),
        op if op.starts_with("playlist.") => playlist(daemon, request, settings).await,
        "podcast.saved" => {
            let s = settings.clone();
            blocking(move || saved_shows(&s)).await
        }
        "spotify.auth.status" => {
            let s = settings.clone();
            blocking(move || spotify_auth_status(&s)).await
        }
        "spotify.auth.login" => {
            let s = settings.clone();
            let dir = daemon.dir.clone();
            blocking(move || {
                let player = s.player()?;
                let log = dir.join("spotify-auth.log");
                let pid = player.spawn_authenticate(&log)?;
                Ok(json!({
                    "started": true,
                    "pid": pid,
                    "log": log,
                    "next": "A browser tab with Spotify's consent page opened on this Mac. A Carbon must click Agree. Then run `spotify auth status` to confirm.",
                }))
            })
            .await
        }
        _ => return None,
    };
    Some(result)
}

async fn control(
    daemon: &Arc<Daemon>,
    settings: Settings,
    f: impl FnOnce(&Controller<'_>) -> Result<silicon_spotify_client::control::Outcome> + Send + 'static,
) -> Result<Value> {
    let d = Arc::clone(daemon);
    let result = blocking(move || {
        d.with_controller(&settings, |c| f(c))
            .map(|o| outcome_json(&o))
    })
    .await;
    daemon.nudge.notify_one();
    result
}

/// `next`: plays the managed queue's next item when it has one, else Spotify's next. Runs under
/// [`Daemon::hand_off`], so concurrent `next`s are applied one after the other and never hand
/// off the same item.
async fn next(daemon: &Arc<Daemon>, settings: Settings) -> Result<Value> {
    let d = Arc::clone(daemon);
    let result = blocking(move || {
        let _serial = d
            .hand_off
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match crate::watcher::advance_queue(&d, &settings)? {
            crate::watcher::Advance::Played(reply) => Ok(reply),
            crate::watcher::Advance::Nothing { skipped } => {
                // Skipping an item that is still being switched to: let Spotify show it first,
                // so its own next moves past it (not past what was playing before).
                if let Some(uri) = &skipped {
                    crate::watcher::wait_until_current(&d, uri, Duration::from_millis(1_500));
                }
                hold(&d);
                let mut reply = d
                    .with_controller(&settings, |c| c.next())
                    .map(|o| outcome_json(&o))?;
                if let Some(uri) = skipped {
                    reply["skipped"] = json!(uri);
                }
                Ok(reply)
            }
        }
    })
    .await;
    daemon.nudge.notify_one();
    result
}

/// An explicit item change: the queue must not override it.
fn hold(daemon: &Daemon) {
    let mut live = daemon.live();
    live.queue.hold(silicon_spotify_client::model::now_ms());
    daemon.save_queue(&live);
}

/// Runs an explicit item change (`play <item>`, `previous`) under [`Daemon::hand_off`], so it and
/// a `spotify next` or a managed-queue hand-off reach Spotify one after the other, in the order
/// they got the lock. The queue's hold opens first, so the queue does not override the change,
/// and opens again once the change has landed: a slow start (Spotify.app launching, a long
/// verification) must not outlast it, or the queue would replace what just started. A hand-off
/// the watcher decided on meanwhile is dropped with it.
///
/// When Spotify.app still plays the managed item it played before (the change failed, or
/// `previous` restarted that item), that item stays the managed one, so the queue still moves on
/// (or resumes) after it.
fn explicit_change(
    daemon: &Daemon,
    change: impl FnOnce(&Daemon) -> Result<silicon_spotify_client::control::Outcome>,
) -> Result<Value> {
    let _serial = daemon
        .hand_off
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let managed_before = daemon.live().queue.managed_now.clone();
    hold(daemon);
    let result = change(daemon);
    // What plays now: the verified reading of a change that landed; one AppleScript read after a
    // failure, and only when there is a managed item to keep.
    let playing = match &result {
        Ok(outcome) => outcome.playback.track.as_ref().map(|t| t.uri.clone()),
        Err(_) if managed_before.is_some() => daemon
            .read()
            .ok()
            .and_then(|playback| playback.track)
            .map(|track| track.uri),
        Err(_) => None,
    };
    let keep = managed_before.filter(|uri| playing.as_ref() == Some(uri));
    if result.is_ok() || keep.is_some() {
        let mut live = daemon.live();
        if result.is_ok() {
            live.queue.hold(silicon_spotify_client::model::now_ms());
        }
        if let Some(uri) = keep
            && live.queue.managed_now.is_none()
            && live.queue.pending.is_none()
        {
            live.queue.managed_now = Some(uri);
        }
        daemon.save_queue(&live);
    }
    result.map(|outcome| outcome_json(&outcome))
}

/// `track.info`: an item by URI, or the item playing now. For a song (a `spotify:track:` URI, or
/// the current item when it is one) the reply also says whether it is in Liked Songs (`liked`),
/// looked up at the same time and left out when that lookup fails or takes too long.
async fn track_info(
    daemon: &Arc<Daemon>,
    settings: Settings,
    uri: Option<SpotifyUri>,
) -> Result<Value> {
    let d = Arc::clone(daemon);
    let s = settings.clone();
    let (info, liked) = match uri {
        Some(uri) => {
            let song = (uri.kind == Kind::Track).then(|| uri.id.clone());
            tokio::join!(
                blocking(move || item_info(&d, &s, &uri)),
                is_liked(&settings, song)
            )
        }
        None => {
            let playback = blocking(move || d.read()).await?;
            let track = playback.track.clone().ok_or_else(Error::nothing_playing)?;
            let song = (track.kind == "track").then(|| track.id.clone());
            tokio::join!(
                blocking(move || Ok(current_info(&s, &playback, track))),
                is_liked(&settings, song)
            )
        }
    };
    let mut info = info?;
    if let (Some(liked), Some(object)) = (liked, info.as_object_mut()) {
        object.insert("liked".into(), json!(liked));
    }
    Ok(info)
}

/// `spotify track <uri>` (not podcast items: spotify_player cannot look those up by id).
fn item_info(daemon: &Daemon, settings: &Settings, uri: &SpotifyUri) -> Result<Value> {
    let kind = match uri.kind {
        Kind::Track => "track",
        Kind::Album => "album",
        Kind::Artist => "artist",
        Kind::Playlist => "playlist",
        Kind::Show | Kind::Episode => {
            return Err(Error::unsupported(
                "spotify_player cannot look up podcast items by id.",
                "Use `spotify podcast search '<name>'` to find shows and episodes.",
            ));
        }
    };
    let _read = (kind == "playlist").then(|| daemon.before_library_read());
    let value = read_json(
        &settings.authed_player()?,
        &["get", "item", "--id", &uri.id, kind],
    )?;
    Ok(item_envelope(kind, value))
}

/// `spotify track` for the item playing now: Spotify.app's view, plus the song's artists, album
/// and explicit flag from the Web API when it is a song (a failed lookup is a warning).
fn current_info(settings: &Settings, playback: &Playback, track: Track) -> Value {
    let mut web = Value::Null;
    let mut warnings = Vec::new();
    if track.kind == "track" {
        match settings
            .authed_player()
            .and_then(|p| p.json(&["get", "item", "--id", &track.id, "track"]))
        {
            Ok(value) => web = value,
            Err(error) => warnings.push(error),
        }
    }
    let artists = web.get("artists").and_then(Value::as_array).map(|a| {
        a.iter()
            .map(|x| json!({"id": x.get("id"), "name": x.get("name")}))
            .collect::<Vec<_>>()
    });
    json!({
        "track": track,
        "playback": {
            "state": playback.state, "position_ms": playback.position_ms, "position": playback.position,
            "remaining_ms": playback.remaining_ms, "progress": playback.progress,
        },
        "artists": artists,
        "album": web.get("album"),
        "explicit": web.get("explicit"),
        "warnings": warnings,
    })
}

/// How long `spotify track` waits for the Liked Songs check before leaving `liked` out.
const LIKED_LOOKUP_TIMEOUT: Duration = Duration::from_millis(2_500);

/// Whether the song `id` is in Liked Songs (`GET /v1/me/tracks/contains`, with spotify_player's
/// cached token). `None` without an id, a token or an answer within [`LIKED_LOOKUP_TIMEOUT`]
/// (a rate limit or the pause after one, the network, a refused token).
async fn is_liked(settings: &Settings, id: Option<String>) -> Option<bool> {
    let id = id.filter(|id| !id.is_empty() && id.chars().all(|c| c.is_ascii_alphanumeric()))?;
    let settings = settings.clone();
    let tokens = blocking(move || {
        Ok(settings
            .authed_player()
            .map(|player| cached_access_tokens(&player))
            .unwrap_or_default())
    })
    .await
    .ok()
    .filter(|tokens| !tokens.is_empty())?;
    let answer = tokio::time::timeout(
        LIKED_LOOKUP_TIMEOUT,
        web_api_get(
            &tokens,
            "https://api.spotify.com/v1/me/tracks/contains",
            &[("ids", id.as_str())],
            LIKED_LOOKUP_TIMEOUT,
        ),
    )
    .await
    .ok()?
    .found()?;
    liked_from_web(&answer)
}

/// The answer of `GET /v1/me/tracks/contains` for one id: `[true]` or `[false]`.
fn liked_from_web(value: &Value) -> Option<bool> {
    match value.as_array()?.as_slice() {
        [liked] => liked.as_bool(),
        _ => None,
    }
}

fn lyrics(daemon: &Daemon, settings: &Settings, uri: Option<&SpotifyUri>) -> Result<Value> {
    let id = match uri {
        Some(uri) if uri.kind == Kind::Track => uri.id.clone(),
        Some(_) => {
            return Err(Error::invalid(
                "Lyrics exist only for tracks.",
                "Pass a spotify:track:<id>, or omit it for the current song.",
            ));
        }
        None => {
            let playback = daemon.read()?;
            let track = playback.track.ok_or_else(Error::nothing_playing)?;
            if track.kind != "track" {
                return Err(Error::unsupported(
                    "The current item is not a song (podcast episodes have no lyrics).",
                    "Play a song first.",
                ));
            }
            track.id
        }
    };
    let output = settings.authed_player()?.run(&["lyrics", "--id", &id])?;
    let text = output.stdout;
    let (title, body) = text.split_once("\n\n").unwrap_or((text.as_str(), ""));
    let found = !body.trim().is_empty() && body.trim() != "Lyrics not found";
    if !found {
        return Err(Error::not_found(
            format!("Spotify has no lyrics for {title}."),
            "Lyrics come from Spotify's lyrics provider; many tracks (instrumentals, new or regional releases) have none.",
        )
        .with_details(json!({"track": format!("spotify:track:{id}")})));
    }
    Ok(json!({
        "track": format!("spotify:track:{id}"),
        "title": title,
        "lines": body.lines().collect::<Vec<_>>(),
        "text": body,
        "synced": false,
    }))
}

fn search(settings: &Settings, query: &str, kinds: &[Kind], limit: usize) -> Result<Value> {
    let max = silicon_spotify_client::player::SEARCH_MAX_PER_KIND as usize;
    if !(1..=max).contains(&limit) {
        return Err(Error::invalid(
            format!("limit must be from 1 to {max}, not {limit}."),
            format!("spotify_player returns at most {max} results per kind."),
        ));
    }
    if query.trim().is_empty() {
        return Err(Error::invalid(
            "The search query is empty.",
            "Example: spotify search 'arctic monkeys 505' --type track",
        ));
    }
    let value = settings.authed_player()?.json(&["search", query])?;
    let mut out = serde_json::Map::new();
    let wanted = |kind: Kind| kinds.is_empty() || kinds.contains(&kind);
    for (key, kind) in [
        ("tracks", Kind::Track),
        ("albums", Kind::Album),
        ("artists", Kind::Artist),
        ("playlists", Kind::Playlist),
        ("shows", Kind::Show),
        ("episodes", Kind::Episode),
    ] {
        if !wanted(kind) {
            continue;
        }
        let items: Vec<Item> = value
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| Item::from_player_json(kind.as_str(), v))
                    .take(limit)
                    .collect()
            })
            .unwrap_or_default();
        out.insert(key.into(), serde_json::to_value(items)?);
    }
    Ok(json!({"query": query, "results": out, "limit": limit}))
}

/// A read (`get key …`, `get item …`) that spotify_player may page through many Web API requests
/// for: one network blip in a long listing should not fail it, so a transient failure
/// ([`retry_read`]) is retried once after a short pause. Only for reads; writes are never repeated.
///
/// # Errors
/// The second attempt's error, or the first's when it is not transient.
fn read_json(player: &SpotifyPlayer, args: &[&str]) -> Result<Value> {
    match player.json(args) {
        Err(error) if retry_read(&error) => {
            log!(
                "spotify_player {}: {} ({}); retrying once",
                args.join(" "),
                error.code,
                error.message
            );
            std::thread::sleep(READ_RETRY_PAUSE);
            player.json(args)
        }
        other => other,
    }
}

/// The pause before [`read_json`]'s retry.
const READ_RETRY_PAUSE: Duration = Duration::from_millis(500);

/// Whether a failed read is worth one more try at once: the network (`transport`) or the warm
/// spotify_player restarting (`spotify_player_busy`). Rate limits and timeouts are not: a retry
/// right away would fail the same way, or double a long wait.
fn retry_read(error: &Error) -> bool {
    error.retryable && matches!(error.code.as_str(), "transport" | "spotify_player_busy")
}

fn library(settings: &Settings, key: &str, limit: Option<usize>) -> Result<Value> {
    let (player_key, kind) = match key {
        "liked" | "tracks" | "user-liked-tracks" => ("user-liked-tracks", "track"),
        "albums" | "user-saved-albums" => ("user-saved-albums", "album"),
        "artists" | "user-followed-artists" => ("user-followed-artists", "artist"),
        "top" | "top-tracks" | "user-top-tracks" => ("user-top-tracks", "track"),
        "playlists" | "user-playlists" => ("user-playlists", "playlist"),
        other => {
            return Err(Error::invalid(
                format!("`{other}` is not a library section."),
                "Use liked, albums, artists, top or playlists.",
            ));
        }
    };
    let value = read_json(&settings.authed_player()?, &["get", "key", player_key])?;
    let all: Vec<Item> = value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| Item::from_player_json(kind, v))
                .collect()
        })
        .unwrap_or_default();
    let total = all.len();
    let items: Vec<Item> = all.into_iter().take(limit.unwrap_or(usize::MAX)).collect();
    Ok(json!({"section": key, "total": total, "items": items}))
}

/// `spotify track <uri>`: `{kind, item, …per-kind fields, raw}` for every kind. Playlists also
/// carry what `playlist show` reports (`playlist`, `owner`, `collaborative`, `track_count`,
/// `duration`, `tracks`) plus `duration_ms`.
fn item_envelope(kind: &str, value: Value) -> Value {
    let mut out = json!({"item": Item::from_player_json(kind, &value)});
    if let (Some(object), Value::Object(view)) = (
        out.as_object_mut(),
        silicon_spotify_client::model::item_view(kind, &value),
    ) {
        object.extend(view);
    }
    if kind == "playlist"
        && let Some(object) = out.as_object_mut()
    {
        let (view, total_ms) = playlist_parts(&value);
        object.insert("kind".into(), json!("playlist"));
        if object.get("item").is_none_or(Value::is_null) {
            object.insert("item".into(), view["playlist"].clone());
        }
        if let Value::Object(view) = view {
            for (key, field) in view {
                object.entry(key).or_insert(field);
            }
        }
        object.entry("duration_ms").or_insert(json!(total_ms));
    }
    out["raw"] = value;
    out
}

/// `playlist show`.
fn playlist_view(value: &Value) -> Value {
    playlist_parts(value).0
}

/// The `playlist show` view and the total length in milliseconds.
fn playlist_parts(value: &Value) -> (Value, u64) {
    let playlist = value.get("playlist").cloned().unwrap_or(Value::Null);
    let item = Item::from_player_json("playlist", &playlist);
    let tracks: Vec<Item> = value
        .get("tracks")
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|t| Item::from_player_json("track", t))
                .collect()
        })
        .unwrap_or_default();
    let total_ms: u64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
    let view = json!({
        "playlist": item,
        "collaborative": playlist.get("collaborative"),
        "owner": playlist.get("owner"),
        "track_count": tracks.len(),
        "duration": silicon_spotify_client::timing::clock(total_ms),
        "tracks": tracks,
    });
    (view, total_ms)
}

/// `playlist.*` arguments.
#[derive(Deserialize, Default)]
#[serde(default)]
struct PlaylistArgs {
    id: Option<String>,
    name: Option<String>,
    description: Option<String>,
    public: bool,
    collab: bool,
    items: Vec<SpotifyUri>,
    from: Option<String>,
    to: Option<String>,
    delete: bool,
    limit: Option<usize>,
}

async fn playlist(daemon: &Arc<Daemon>, request: &Request, settings: Settings) -> Result<Value> {
    let a: PlaylistArgs = args(request)?;
    let op = request.op.clone();
    let d = Arc::clone(daemon);
    blocking(move || {
        let writes = !matches!(
            op.as_str(),
            "playlist.list" | "playlist.show" | "playlist.rename"
        );
        let read = (!writes).then(|| d.before_library_read());
        let result = playlist_op(&settings, &op, &a);
        drop(read);
        if writes {
            d.after_library_write();
        }
        result
    })
    .await
}

/// One `playlist.*` op (blocking).
#[allow(clippy::too_many_lines)]
fn playlist_op(settings: &Settings, op: &str, a: &PlaylistArgs) -> Result<Value> {
    {
        let player = settings.authed_player()?;
        let id = |what: &str| -> Result<String> {
            let raw = a.id.clone().ok_or_else(|| {
                Error::invalid(
                    format!("{what} needs a playlist id."),
                    "List ids with `spotify playlist list`.",
                )
            })?;
            Ok(SpotifyUri::parse(&raw, Some(Kind::Playlist))?.id)
        };
        match op {
            "playlist.list" => library(settings, "playlists", a.limit),
            "playlist.show" => Ok(playlist_view(&read_json(
                &player,
                &["get", "item", "--id", &id("show")?, "playlist"],
            )?)),
            "playlist.create" => {
                let name = a.name.clone().filter(|n| !n.trim().is_empty()).ok_or_else(|| Error::invalid("A playlist needs a name.", "Example: spotify playlist create 'Deep focus' --description 'no vocals'"))?;
                let description = a.description.clone().unwrap_or_default();
                let (id, output) =
                    create_playlist(&player, &name, &description, a.public, a.collab)?;
                Ok(
                    json!({"created": true, "id": id, "uri": id.as_deref().map(playlist_uri), "name": name, "public": a.public, "collaborative": a.collab, "message": output.stdout}),
                )
            }
            "playlist.delete" => {
                let id = id("delete")?;
                let output = player.run(&["playlist", "delete", &id])?;
                let unfollowed = !output.stdout.contains("nothing to be done");
                Ok(
                    json!({"deleted": unfollowed, "id": id, "message": output.stdout,
                    "note": "Spotify has no hard delete: this unfollows the playlist (it disappears from your library; collaborators and followers keep it)."}),
                )
            }
            "playlist.add" | "playlist.remove" => {
                let playlist = id(if op == "playlist.add" {
                    "add"
                } else {
                    "remove"
                })?;
                if a.items.is_empty() {
                    return Err(Error::invalid(
                        "No tracks or albums given.",
                        "Example: spotify playlist add <playlist> spotify:track:<id> spotify:album:<id>",
                    ));
                }
                let action = if op == "playlist.add" {
                    "add"
                } else {
                    "delete"
                };
                let mut results = Vec::new();
                for item in &a.items {
                    let flag = match item.kind {
                        Kind::Track => "--track-id",
                        Kind::Album => "--album-id",
                        _ => {
                            return Err(Error::unsupported(
                                format!(
                                    "{} cannot be added to playlists through spotify_player (tracks and albums only).",
                                    item.kind
                                ),
                                "Pass spotify:track:<id> or spotify:album:<id> items.",
                            ));
                        }
                    };
                    let output =
                        player.run(&["playlist", "edit", flag, &item.id, action, &playlist])?;
                    results.push(json!({"item": item.uri(), "message": output.stdout}));
                }
                Ok(
                    json!({"playlist": format!("spotify:playlist:{playlist}"), "action": action, "results": results}),
                )
            }
            "playlist.rename" => Err(Error::unsupported(
                "Renaming or re-describing a playlist is not possible through spotify_player or AppleScript.",
                "Rename it in the Spotify app. Everything else (create, delete, add, remove, import, fork, sync) works here.",
            )),
            "playlist.import" => {
                let from =
                    SpotifyUri::parse(a.from.as_deref().unwrap_or_default(), Some(Kind::Playlist))?
                        .id;
                let to =
                    SpotifyUri::parse(a.to.as_deref().unwrap_or_default(), Some(Kind::Playlist))?
                        .id;
                let output = import_playlist(&player, &from, &to, a.delete)?;
                Ok(json!({"imported": true, "from": from, "to": to, "message": output.stdout}))
            }
            "playlist.fork" => {
                let from = id("fork")?;
                let Some(name) = a.name.clone() else {
                    let output = player.run(&["playlist", "fork", &from])?;
                    let (id, name) = forked_playlist(&output.stdout).unzip();
                    return Ok(
                        json!({"forked": true, "from": from, "id": id, "uri": id.as_deref().map(playlist_uri), "name": name, "message": output.stdout}),
                    );
                };
                if name.trim().is_empty() || name.chars().count() > 100 {
                    return Err(Error::invalid(
                        "A playlist name must be 1 to 100 characters.",
                        "Example: spotify playlist fork <playlist-id> --name 'Deep focus (mine)'",
                    ));
                }
                // spotify_player cannot name a fork, so do what its fork does under the chosen
                // name: read the source (so a bad id creates nothing), create, import.
                let source = player.json(&["get", "item", "--id", &from, "playlist"])?;
                let description = a
                    .description
                    .clone()
                    .or_else(|| {
                        source
                            .pointer("/playlist/desc")
                            .and_then(Value::as_str)
                            .map(str::to_owned)
                    })
                    .unwrap_or_default();
                let (id, created) =
                    create_playlist(&player, &name, &description, a.public, a.collab)?;
                let id = id.ok_or_else(|| {
                    Error::new("spotify_player_failed", format!("spotify_player created the playlist '{name}' but did not print its id, so nothing was imported into it."), format!("Find its id with `spotify playlist list`, then run `spotify playlist import {from} <id>`."))
                        .with_details(json!({"stdout": created.stdout}))
                })?;
                let uri = playlist_uri(&id);
                let imported = import_playlist(&player, &from, &id, false).map_err(|error| Error {
                    message: format!("Created the playlist '{name}' ({uri}), but importing {from} into it failed: {}", error.message),
                    hint: format!("Retry the import with `spotify playlist import {from} {id}` (or remove the playlist with `spotify playlist delete {id}`). {}", error.hint),
                    details: Some(json!({"created": {"id": id, "uri": uri, "name": name}, "cause": error.details})),
                    ..error
                })?;
                Ok(
                    json!({"forked": true, "from": from, "id": id, "uri": uri, "name": name, "public": a.public, "collaborative": a.collab,
                    "message": format!("Forked {from}.\nNew playlist: {id}:{name}\n{}", imported.stdout),
                    "note": "spotify_player cannot name a fork, so this created a playlist with that name and imported the source into it, as its fork does."}),
                )
            }
            "playlist.sync" => {
                let mut args = vec!["playlist", "sync"];
                if a.delete {
                    args.push("--delete");
                }
                let id =
                    a.id.as_deref()
                        .map(|raw| SpotifyUri::parse(raw, Some(Kind::Playlist)).map(|u| u.id))
                        .transpose()?;
                if let Some(id) = &id {
                    args.push(id);
                }
                let output = player.run(&args)?;
                Ok(json!({"synced": true, "id": id, "message": output.stdout}))
            }
            other => Err(Error::invalid(
                format!("Unknown playlist op `{other}`."),
                "Run `spotify playlist --help`.",
            )),
        }
    }
}

/// `spotify_player playlist new`; returns the new playlist's bare id (when the output names it).
fn create_playlist(
    player: &SpotifyPlayer,
    name: &str,
    description: &str,
    public: bool,
    collab: bool,
) -> Result<(Option<String>, Output)> {
    let output = player.run(&new_playlist_args(name, description, public, collab))?;
    Ok((created_playlist_id(&output.stdout), output))
}

/// Arguments for `spotify_player playlist new`. `--` keeps a name or description that starts
/// with `-` from being read as a flag, and an empty description is left out because
/// spotify_player rejects an empty one (it then uses none).
fn new_playlist_args<'a>(
    name: &'a str,
    description: &'a str,
    public: bool,
    collab: bool,
) -> Vec<&'a str> {
    let mut args = vec!["playlist", "new"];
    if public {
        args.push("--public");
    }
    if collab {
        args.push("--collab");
    }
    args.push("--");
    args.push(name);
    if !description.is_empty() {
        args.push(description);
    }
    args
}

/// `spotify_player playlist import` (also records the import for `playlist sync`).
fn import_playlist(player: &SpotifyPlayer, from: &str, to: &str, delete: bool) -> Result<Output> {
    let mut args = vec!["playlist", "import"];
    if delete {
        args.push("--delete");
    }
    args.push(from);
    args.push(to);
    player.run(&args)
}

/// The id in `playlist new` output, `Playlist 'NAME' with id 'ID' was created.`, where ID is a
/// bare id or (0.25) a full `spotify:playlist:` URI.
fn created_playlist_id(stdout: &str) -> Option<String> {
    let (_, rest) = stdout.rsplit_once("with id '")?;
    bare_playlist_id(rest.split('\'').next()?)
}

/// The new playlist (id, name) in `playlist fork` output: `Forked FROM.`, then
/// `New playlist: ID:NAME` (ID bare or a URI), then the import report (`Importing from …`).
fn forked_playlist(stdout: &str) -> Option<(String, String)> {
    let (_, rest) = stdout.split_once("New playlist: ")?;
    let rest = rest.strip_prefix("spotify:playlist:").unwrap_or(rest);
    let end = rest
        .find(|c: char| !c.is_ascii_alphanumeric())
        .unwrap_or(rest.len());
    let (id, tail) = rest.split_at(end);
    // Names can span lines, so the name runs up to the import report.
    let name = tail.strip_prefix(':').map_or("", |tail| {
        tail.split_once("\nImporting from ")
            .map_or_else(|| tail.lines().next().unwrap_or_default(), |(name, _)| name)
    });
    Some((bare_playlist_id(id)?, name.to_owned()))
}

fn bare_playlist_id(raw: &str) -> Option<String> {
    SpotifyUri::parse(raw, Some(Kind::Playlist))
        .ok()
        .filter(|uri| uri.kind == Kind::Playlist)
        .map(|uri| uri.id)
}

fn playlist_uri(id: &str) -> String {
    format!("spotify:playlist:{id}")
}

fn saved_shows(settings: &Settings) -> Result<Value> {
    let player = settings.player()?;
    let dir = player
        .cache_folder()
        .ok_or_else(|| Error::internal("no spotify_player cache folder"))?;
    let path = dir.join("SavedShows_cache.json");
    let text = std::fs::read_to_string(&path).map_err(|_| {
        Error::not_found(
            "spotify_player has not cached your saved podcasts yet.",
            "The cache fills when spotify_player (the warm instance the daemon runs) loads your library; wait a minute after `spotify daemon start`, or search with `spotify podcast search '<name>'`.",
        )
    })?;
    let value: Value = serde_json::from_str(&text)?;
    let shows: Vec<Item> = value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| Item::from_player_json("show", v))
                .collect()
        })
        .unwrap_or_default();
    let modified = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .map(|t| silicon_spotify_client::model::rfc3339(time::OffsetDateTime::from(t)));
    Ok(json!({"shows": shows, "source": "spotify_player cache", "cached_at": modified}))
}

fn spotify_auth_status(settings: &Settings) -> Result<Value> {
    let player = match settings.player() {
        Ok(p) => p,
        Err(error) => {
            return Ok(json!({"installed": false, "authenticated": false, "error": error}));
        }
    };
    let version = player.version().ok();
    let cached = player.has_cached_token();
    let mut out = json!({
        "installed": true,
        "binary": player.binary,
        "version": version,
        "cache_folder": player.cache_folder(),
        "token_cached": cached,
        "authenticated": false,
    });
    if cached {
        match player.json(&["get", "key", "devices"]) {
            Ok(_) => out["authenticated"] = json!(true),
            Err(error) => out["error"] = serde_json::to_value(&error)?,
        }
    } else {
        out["error"] = serde_json::to_value(silicon_spotify_client::player::auth_required(None))?;
    }
    Ok(out)
}

/// How long `spotify queue` may spend looking up facts still missing from queued episodes.
const LIST_LOOKUP_BUDGET: Duration = Duration::from_millis(2_500);

async fn queue_list(daemon: &Arc<Daemon>, settings: Settings) -> Result<Value> {
    // Facts a rate limit kept out of `queue add`: look them up now when the Web API is free
    // (briefly, and not while a background lookup runs), else leave them to the background.
    let look_now = web_api_pause_left().is_zero() && {
        let live = daemon.live();
        !live.facts_backfill && !facts_wanted(&live).is_empty()
    };
    if look_now {
        let tokens = access_tokens(&settings).await;
        if !tokens.is_empty() {
            let deadline = tokio::time::Instant::now() + LIST_LOOKUP_BUDGET;
            backfill_round(daemon, &tokens, deadline).await;
        }
    }
    fill_facts_later(daemon, &settings);
    let (managed, managed_now, resume) = {
        let live = daemon.live();
        (
            live.queue.items.clone(),
            live.queue.managed_now.clone(),
            live.queue.resume.clone(),
        )
    };
    let d = Arc::clone(daemon);
    let upcoming = blocking(move || -> Result<Value> {
        let queue = settings.authed_player().and_then(|player| {
            let value = player.json(&["get", "key", "queue"])?;
            Ok((player, value))
        });
        match queue {
            Ok((player, value)) => {
                let (items, repeats) = spotify_upcoming(&value);
                let more = !items.is_empty();
                let mut upcoming = json!({"items": items});
                if repeats > 0 {
                    // Why, as far as spotify_player's view of playback (its memory) shows it,
                    // checked against whether Spotify.app repeats now.
                    let playback = player
                        .json(&["get", "key", "playback"])
                        .ok()
                        .and_then(|p| WebPlayback::from_player_json(&p));
                    let repeating = d.read().ok().and_then(|app| app.repeating);
                    upcoming["current_repeats_left_out"] = json!(repeats);
                    upcoming["note"] = json!(repeats_note(
                        repeats,
                        &value,
                        playback.as_ref(),
                        repeating,
                        more
                    ));
                }
                Ok(upcoming)
            }
            Err(error) => Ok(json!({"items": [], "error": error})),
        }
    })
    .await?;
    Ok(json!({
        "managed": managed,
        "managed_now": managed_now,
        "resume_after": resume,
        "spotify_upcoming": upcoming,
        "note": "`managed` items are spotify-cli's own queue: they play next, in order, and can be added, removed, moved and cleared. `spotify_upcoming` is Spotify's native queue/context (read-only: Spotify offers no API to remove or reorder it).",
    }))
}

/// The note on the copies of the item playing now that [`spotify_upcoming`] left out. Spotify
/// lists the current item again when nothing else follows it: with repeat-one, when it was played
/// without a context, or at the end of its context with repeat off. The note names one of those
/// causes only when `playback` (spotify_player's view) shows it for that same item, and none
/// otherwise; `more` says whether other items follow the repeats.
///
/// The view is spotify_player's memory, up to one refresh interval (see
/// [`crate::warm::REFRESH_MS`]) behind. `repeating` is whether Spotify.app repeats now (it cannot
/// tell repeat-one from repeating the context): a view it contradicts names no cause.
fn repeats_note(
    repeats: usize,
    queue: &Value,
    playback: Option<&WebPlayback>,
    repeating: Option<bool>,
    more: bool,
) -> String {
    let current = queue.get("currently_playing").filter(|c| !c.is_null());
    let kind = current
        .and_then(|c| c.get("type"))
        .and_then(Value::as_str)
        .filter(|kind| matches!(*kind, "track" | "episode"))
        .unwrap_or("item");
    let current = current.and_then(crate::watcher::item_uri);
    let cause = playback
        .filter(|p| current.is_some() && p.item_uri == current)
        .filter(|p| {
            let view = p.repeat_state.as_deref().map(|mode| mode != "off");
            !matches!((repeating, view), (Some(app), Some(view)) if app != view)
        })
        .and_then(|p| {
            if p.repeat_state.as_deref() == Some("track") {
                Some("repeat-one is on".to_owned())
            } else if p.context_uri.is_none() {
                Some(format!("the {kind} was played without a context"))
            } else if p.repeat_state.as_deref() == Some("off") && !more {
                Some(match p.context_type.as_deref() {
                    Some("collection") => "nothing else is up next in Liked Songs".to_owned(),
                    Some(context) => format!("nothing else is up next in its {context}"),
                    None => "nothing else is up next in its context".to_owned(),
                })
            } else {
                None
            }
        })
        .map(|cause| format!(" ({cause})"))
        .unwrap_or_default();
    format!(
        "Spotify listed the {kind} playing now {repeats} more time(s) as upcoming{cause}; those are left out."
    )
}

/// Spotify's upcoming items from `get key queue`, without the leading run of the item playing
/// now (Spotify lists it again when nothing else follows it; see [`repeats_note`]), and how many
/// such repeats were left out.
fn spotify_upcoming(value: &Value) -> (Vec<Item>, usize) {
    let current = value
        .get("currently_playing")
        .and_then(crate::watcher::item_uri);
    let mut repeats = 0;
    let mut items = Vec::new();
    for entry in value
        .get("queue")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if items.is_empty() && current.is_some() && crate::watcher::item_uri(entry) == current {
            repeats += 1;
            continue;
        }
        let kind = entry.get("type").and_then(Value::as_str).unwrap_or("track");
        items.extend(Item::from_player_json(kind, entry));
    }
    (items, repeats)
}

/// (name, artists or show, duration) looked up for a queued item.
type TrackFacts = (Option<String>, Option<String>, Option<u64>);

/// Whether none of the facts is missing.
fn complete(facts: &TrackFacts) -> bool {
    facts.0.is_some() && facts.1.is_some() && facts.2.is_some()
}

/// `facts` with what it lacks taken from `found`, field by field (what `facts` has is kept).
fn merge(facts: TrackFacts, found: TrackFacts) -> TrackFacts {
    (
        facts.0.or(found.0),
        facts.1.or(found.1),
        facts.2.or(found.2),
    )
}

/// What a caller already knows about an item it queues (e.g. from the search hit it picked).
#[derive(Clone, Debug, Default, Deserialize)]
struct Known {
    uri: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    by: Option<String>,
    #[serde(default)]
    duration_ms: Option<u64>,
}

impl Known {
    /// Its facts; an empty name or `by` counts as missing.
    fn facts(&self) -> TrackFacts {
        (
            text(self.name.clone()),
            text(self.by.clone()),
            self.duration_ms,
        )
    }
}

/// A name or `by` worth recording: an empty one counts as missing, so a later lookup can still
/// fill it in ([`merge`] keeps what is there).
fn text(value: Option<String>) -> Option<String> {
    value.filter(|v| !v.trim().is_empty())
}

/// Why `uri` cannot be queued, with a hint that fits its kind.
fn not_queueable(uri: &SpotifyUri) -> Error {
    let article = if matches!(uri.kind, Kind::Album | Kind::Artist) {
        "an"
    } else {
        "a"
    };
    let hint = match uri.kind {
        Kind::Album => {
            "Play the whole album with `spotify play <uri>`; to queue some of its songs, list them with `spotify track <uri>` and queue those."
        }
        Kind::Playlist => {
            "Play the whole playlist with `spotify play <uri>`; to queue some of its songs, list them with `spotify playlist show <id>` and queue those."
        }
        Kind::Artist => {
            "Play the artist with `spotify play <uri>`; to queue their songs, list the top tracks with `spotify track <uri>` and queue those."
        }
        Kind::Show => {
            "Play the show with `spotify podcast play <uri>`, or queue one of its episodes (find them with `spotify podcast search '<show>' --episodes`)."
        }
        Kind::Track | Kind::Episode => "",
    };
    Error::invalid(
        format!(
            "{} is {article} {}; the queue holds tracks and episodes.",
            uri.uri(),
            uri.kind
        ),
        hint,
    )
}

/// How long one `queue add` may spend looking up episode names in total.
const EPISODE_LOOKUP_BUDGET: Duration = Duration::from_secs(8);

/// Name, show and length of an episode, which spotify_player cannot look up by id: the Web API
/// (`GET /v1/episodes/{id}`, see [`web_api_get`]). What a rate limit keeps out is looked up again
/// later ([`fill_facts_later`]).
async fn episode_facts(tokens: &[String], id: &str) -> Fetched<TrackFacts> {
    let url = format!("https://api.spotify.com/v1/episodes/{id}");
    web_api_get(tokens, &url, &[], Duration::from_secs(4))
        .await
        .map(|value| episode_from_web(&value))
}

/// Lookups per queued episode before its missing facts are left out for good (a rate limit's
/// refusal does not count).
const FACT_TRIES: u32 = 3;
/// Longest a background lookup of missing facts keeps waiting out rate limits.
const BACKFILL_FOR: Duration = Duration::from_secs(600);
/// Between background passes after a lookup failed for a reason other than a rate limit.
const BACKFILL_RETRY: Duration = Duration::from_secs(20);

/// The Spotify id of a queued episode that still lacks its name, show or length.
fn episode_missing_facts(item: &QueueItem) -> Option<&str> {
    let id = item.uri.strip_prefix("spotify:episode:")?;
    (item.name.is_none() || item.by.is_none() || item.duration_ms.is_none()).then_some(id)
}

/// Managed items worth looking up again: (item id, episode id) of each queued episode that
/// lacks facts and has had fewer than [`FACT_TRIES`] lookups.
fn facts_wanted(live: &Live) -> Vec<(String, String)> {
    live.queue
        .items
        .iter()
        .filter(|item| live.fact_tries.get(&item.id).copied().unwrap_or(0) < FACT_TRIES)
        .filter_map(|item| Some((item.id.clone(), episode_missing_facts(item)?.to_owned())))
        .collect()
}

/// Adds `found` to what managed item `item_id` lacks, field by field, and saves the queue.
fn fill_item(daemon: &Daemon, item_id: &str, found: TrackFacts) {
    let mut live = daemon.live();
    let Some(item) = live.queue.items.iter_mut().find(|item| item.id == item_id) else {
        return;
    };
    let before = (item.name.clone(), item.by.clone(), item.duration_ms);
    let merged = merge(before.clone(), found);
    if merged == before {
        return;
    }
    (item.name, item.by, item.duration_ms) = merged;
    daemon.save_queue(&live);
}

/// spotify_player's cached Web API access tokens (none when it is not signed in).
async fn access_tokens(settings: &Settings) -> Vec<String> {
    let settings = settings.clone();
    blocking(move || {
        Ok(settings
            .authed_player()
            .map(|player| cached_access_tokens(&player))
            .unwrap_or_default())
    })
    .await
    .unwrap_or_default()
}

/// One pass over [`facts_wanted`] until `deadline`. Each lookup counts as a try, except one a
/// rate limit refused, which ends the pass (true then), and one `deadline` cut short (the pass's
/// own budget, shorter than a request's timeout in `spotify queue`), which is left for a later
/// pass.
async fn backfill_round(
    daemon: &Daemon,
    tokens: &[String],
    deadline: tokio::time::Instant,
) -> bool {
    let wanted = facts_wanted(&daemon.live());
    for (item_id, episode_id) in wanted {
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        let Ok(fetched) =
            tokio::time::timeout_at(deadline, episode_facts(tokens, &episode_id)).await
        else {
            break;
        };
        if matches!(fetched, Fetched::Limited) {
            return true;
        }
        *daemon.live().fact_tries.entry(item_id.clone()).or_default() += 1;
        if let Fetched::Found(found) = fetched {
            fill_item(daemon, &item_id, found);
        }
    }
    false
}

/// Looks up, in the background, the facts queued episodes still lack. `queue add` gets none
/// while Spotify rate-limits the client it shares with spotify_player (or when the lookup is
/// slow), and would otherwise keep the item nameless for good. After a rate limit this waits
/// until its `Retry-After` has passed, then asks again; other failures get [`FACT_TRIES`]
/// lookups in all. At most one runs at a time. True when one runs to look for them.
fn fill_facts_later(daemon: &Arc<Daemon>, settings: &Settings) -> bool {
    {
        let mut live = daemon.live();
        let Live {
            queue, fact_tries, ..
        } = &mut *live;
        fact_tries.retain(|id, _| queue.items.iter().any(|item| &item.id == id));
        if facts_wanted(&live).is_empty() {
            return false;
        }
        if live.facts_backfill {
            return true;
        }
        live.facts_backfill = true;
    }
    let daemon = Arc::clone(daemon);
    let settings = settings.clone();
    tokio::spawn(async move {
        tokio::select! {
            () = backfill_facts(&daemon, &settings) => {}
            () = daemon.shutdown.notified() => daemon.live().facts_backfill = false,
        }
    });
    true
}

/// The loop behind [`fill_facts_later`]; it clears `facts_backfill` when it ends.
async fn backfill_facts(daemon: &Daemon, settings: &Settings) {
    let give_up = tokio::time::Instant::now() + BACKFILL_FOR;
    let mut wait = Duration::ZERO;
    loop {
        let at = tokio::time::Instant::now() + wait.max(web_api_pause_left());
        let tokens = if at > give_up {
            Vec::new()
        } else {
            tokio::time::sleep_until(at).await;
            access_tokens(settings).await
        };
        let limited = !tokens.is_empty()
            && backfill_round(
                daemon,
                &tokens,
                tokio::time::Instant::now() + EPISODE_LOOKUP_BUDGET,
            )
            .await;
        {
            // Checked and cleared together, so an item queued after this starts a new lookup.
            let mut live = daemon.live();
            if tokens.is_empty() || facts_wanted(&live).is_empty() {
                live.facts_backfill = false;
                return;
            }
        }
        wait = if limited {
            Duration::from_secs(1)
        } else {
            BACKFILL_RETRY
        };
    }
}

/// What a Web API lookup came back with.
#[derive(Debug, PartialEq)]
enum Fetched<T> {
    /// The answer.
    Found(T),
    /// Refused by a rate limit (now, or the pause after an earlier one): worth asking again once
    /// the `Retry-After` has passed ([`web_api_pause_left`]).
    Limited,
    /// Anything else: refused tokens, another status, the network, the timeout.
    Failed,
}

impl<T> Fetched<T> {
    fn found(self) -> Option<T> {
        match self {
            Self::Found(value) => Some(value),
            Self::Limited | Self::Failed => None,
        }
    }

    fn map<U>(self, f: impl FnOnce(T) -> U) -> Fetched<U> {
        match self {
            Self::Found(value) => Fetched::Found(f(value)),
            Self::Limited => Fetched::Limited,
            Self::Failed => Fetched::Failed,
        }
    }
}

/// Until when (unix ms) [`web_api_get`] stays away from the Web API after a rate limit.
static WEB_API_PAUSED_UNTIL_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How long [`web_api_get`] still stays away after a rate limit (zero when it does not).
fn web_api_pause_left() -> Duration {
    let until = WEB_API_PAUSED_UNTIL_MS.load(std::sync::atomic::Ordering::Relaxed);
    Duration::from_millis(until.saturating_sub(silicon_spotify_client::model::now_ms()))
}

/// How long to stay away after a 429: its `Retry-After` in seconds, 30 s without a usable one,
/// at most 10 minutes (these lookups are niceties; a bogus header must not disable them for long).
fn rate_limit_pause(retry_after: Option<&str>) -> Duration {
    let secs = retry_after
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(30);
    Duration::from_secs(secs.clamp(1, 600))
}

/// `GET` a Spotify Web API `url` with the access tokens spotify_player keeps in its cache folder
/// (the warm copy keeps them fresh; see [`cached_access_tokens`]), trying the next token when one
/// is refused (401/403). [`Fetched::Limited`] on a rate limit (429), [`Fetched::Failed`] on any
/// other status, a network error or after `timeout` per request; nothing is logged or shown.
/// After a rate limit it sends nothing until the `Retry-After` has passed (answering `Limited`
/// meanwhile), so these lookups do not prolong it for spotify_player, which shares the token's
/// client.
async fn web_api_get(
    tokens: &[String],
    url: &str,
    query: &[(&str, &str)],
    timeout: Duration,
) -> Fetched<Value> {
    use std::sync::atomic::Ordering;
    if !web_api_pause_left().is_zero() {
        return Fetched::Limited;
    }
    silicon_spotify_client::api::ensure_crypto();
    // The token goes to api.spotify.com only: never follow a redirect with it.
    let Ok(client) = reqwest::Client::builder()
        .timeout(timeout)
        .redirect(reqwest::redirect::Policy::none())
        .https_only(true)
        .build()
    else {
        return Fetched::Failed;
    };
    for token in tokens.iter().take(2) {
        let mut request = client.get(url).bearer_auth(token);
        if !query.is_empty() {
            request = request.query(query);
        }
        let Ok(response) = request.send().await else {
            return Fetched::Failed;
        };
        match response.status().as_u16() {
            200 => {
                return response
                    .json::<Value>()
                    .await
                    .map_or(Fetched::Failed, Fetched::Found);
            }
            // That token is stale or for a client without the scope: try the next.
            401 | 403 => {}
            429 => {
                let pause = rate_limit_pause(
                    response
                        .headers()
                        .get(reqwest::header::RETRY_AFTER)
                        .and_then(|value| value.to_str().ok()),
                );
                let until = silicon_spotify_client::model::now_ms()
                    .saturating_add(u64::try_from(pause.as_millis()).unwrap_or(u64::MAX));
                WEB_API_PAUSED_UNTIL_MS.fetch_max(until, Ordering::Relaxed);
                return Fetched::Limited;
            }
            _ => return Fetched::Failed,
        }
    }
    Fetched::Failed
}

/// (name, show, duration) of a Web API episode object.
fn episode_from_web(value: &Value) -> TrackFacts {
    (
        text(value.get("name").and_then(Value::as_str).map(str::to_owned)),
        text(
            value
                .pointer("/show/name")
                .and_then(Value::as_str)
                .map(str::to_owned),
        ),
        value.get("duration_ms").and_then(Value::as_u64),
    )
}

/// The unexpired Web API access tokens in spotify_player's cache folder, newest first.
fn cached_access_tokens(player: &SpotifyPlayer) -> Vec<String> {
    let Some(entries) = player
        .cache_folder()
        .and_then(|dir| std::fs::read_dir(dir).ok())
    else {
        return Vec::new();
    };
    let now = time::OffsetDateTime::now_utc();
    let mut found: Vec<(std::time::SystemTime, String)> = entries
        .filter_map(std::result::Result::ok)
        .filter(|entry| entry.file_name().to_string_lossy().ends_with("_token.json"))
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            let value: Value = serde_json::from_slice(&std::fs::read(entry.path()).ok()?).ok()?;
            let expired = value
                .get("expires_at")
                .and_then(Value::as_str)
                .and_then(|at| {
                    time::OffsetDateTime::parse(at, &time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .is_some_and(|at| at <= now);
            let token = value.get("access_token")?.as_str()?.to_owned();
            (!expired).then_some((modified, token))
        })
        .collect();
    found.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    found.into_iter().map(|(_, token)| token).collect()
}

async fn queue_add(daemon: &Arc<Daemon>, request: &Request, settings: Settings) -> Result<Value> {
    #[derive(Deserialize)]
    struct A {
        uris: Vec<SpotifyUri>,
        #[serde(default)]
        next: bool,
        /// Facts the caller already has (optional).
        #[serde(default)]
        known: Vec<Known>,
    }
    let a: A = args(request)?;
    if a.uris.is_empty() {
        return Err(Error::invalid(
            "Nothing to queue.",
            "Example: spotify queue add spotify:track:<id> [more…]",
        ));
    }
    for uri in &a.uris {
        SpotifyUri::new(uri.kind, &uri.id)?;
        if !matches!(uri.kind, Kind::Track | Kind::Episode) {
            return Err(not_queueable(uri));
        }
    }
    let who = request.isi.clone().or_else(|| request.home.clone());
    let uris = a.uris.clone();
    let known = a.known.clone();
    let lookup = settings.authed_player().ok();
    let signed_in = lookup.is_some();
    let mut details = blocking(move || -> Result<Vec<TrackFacts>> {
        Ok(uris
            .iter()
            .map(|uri| {
                let known = known
                    .iter()
                    .find(|k| k.uri == uri.uri())
                    .map_or_else(TrackFacts::default, Known::facts);
                // Episodes are looked up below (the Web API); tracks through spotify_player.
                if complete(&known) || uri.kind != Kind::Track {
                    return known;
                }
                let found = lookup
                    .as_ref()
                    .and_then(|p| p.json(&["get", "item", "--id", &uri.id, "track"]).ok())
                    .and_then(|v| Item::from_player_json("track", &v))
                    .map_or_else(TrackFacts::default, |item| {
                        (
                            text(Some(item.name)),
                            text(Some(item.by.join(", "))),
                            item.duration_ms,
                        )
                    });
                merge(known, found)
            })
            .collect())
    })
    .await?;
    let episode_lacks =
        |uri: &SpotifyUri, facts: &TrackFacts| uri.kind == Kind::Episode && !complete(facts);
    // Whether missing facts can be looked up later, and whether a rate limit refused them now.
    let (mut can_look, mut limited) = (false, false);
    if signed_in
        && a.uris
            .iter()
            .zip(&details)
            .any(|(uri, facts)| episode_lacks(uri, facts))
    {
        let tokens = access_tokens(&settings).await;
        can_look = !tokens.is_empty();
        // Names are a nicety: on a slow network, stop looking them up rather than hold the
        // `queue add` (many episodes at 4 s each) near the caller's timeout. What is still
        // missing then (or refused by a rate limit) is looked up in the background.
        let deadline = tokio::time::Instant::now() + EPISODE_LOOKUP_BUDGET;
        for (uri, facts) in a.uris.iter().zip(details.iter_mut()) {
            if tokens.is_empty() || tokio::time::Instant::now() >= deadline {
                break;
            }
            // A search hit may name the episode but not its show: fill in what is missing.
            if !episode_lacks(uri, facts) {
                continue;
            }
            match tokio::time::timeout_at(deadline, episode_facts(&tokens, &uri.id)).await {
                Ok(Fetched::Found(found)) => *facts = merge(std::mem::take(facts), found),
                Ok(Fetched::Limited) => limited = true,
                Ok(Fetched::Failed) | Err(_) => {}
            }
        }
    }
    let added: Vec<QueueItem> = a
        .uris
        .iter()
        .zip(details)
        .map(|(uri, (name, by, duration_ms))| QueueItem {
            id: format!("q_{}", &uuid::Uuid::now_v7().simple().to_string()[24..]),
            uri: uri.uri(),
            name,
            by,
            duration_ms,
            added_at: now_rfc3339(),
            added_by: who.clone(),
            attempts: 0,
        })
        .collect();
    let queue = {
        let mut live = daemon.live();
        if a.next {
            for (index, item) in added.iter().cloned().enumerate() {
                live.queue.items.insert(index, item);
            }
        } else {
            live.queue.items.extend(added.iter().cloned());
        }
        daemon.save_queue(&live);
        live.queue.items.clone()
    };
    // Remember what the queue will interrupt while there is still time.
    let needs_resume = {
        let live = daemon.live();
        live.queue.managed_now.is_none() && live.queue.resume.is_none()
    };
    if needs_resume {
        let d = Arc::clone(daemon);
        let s = daemon.settings();
        let _ = blocking(move || {
            crate::watcher::capture_resume(&d, &s);
            Ok(())
        })
        .await;
    }
    daemon.nudge.notify_one();
    let mut out = json!({"added": added, "queue": queue});
    let pending: Vec<&str> = added
        .iter()
        .filter(|item| episode_missing_facts(item).is_some())
        .map(|item| item.id.as_str())
        .collect();
    if can_look && !pending.is_empty() && fill_facts_later(daemon, &settings) {
        out["note"] = json!(pending_note(pending.len(), limited));
        out["metadata_pending"] = json!(pending);
    }
    Ok(out)
}

/// The note on `queue add` when the Web API has not described some of the episodes yet;
/// `limited` when a rate limit refused a lookup.
fn pending_note(episodes: usize, limited: bool) -> String {
    let what = if episodes == 1 {
        "1 queued episode".to_owned()
    } else {
        format!("{episodes} queued episodes")
    };
    let why = if limited {
        " (Spotify is rate-limiting this client right now)"
    } else {
        ""
    };
    format!(
        "Spotify's Web API has not given the name, show or length of {what} yet{why}. The daemon asks again in the background; `spotify queue` shows what it finds."
    )
}

fn queue_edit(daemon: &Arc<Daemon>, request: &Request) -> Result<Value> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct A {
        item: Option<String>,
        from: Option<String>,
        to: Option<usize>,
    }
    let a: A = args(request)?;
    let mut live = daemon.live();
    let find = |queue: &[QueueItem], reference: &str| -> Result<usize> {
        if let Ok(position) = reference.parse::<usize>()
            && position >= 1
            && position <= queue.len()
        {
            return Ok(position - 1);
        }
        queue
            .iter()
            .position(|q| q.id == reference || q.uri == reference || q.uri.ends_with(&format!(":{reference}")))
            .ok_or_else(|| {
                Error::not_found(
                    format!("`{reference}` is not in the managed queue (positions 1–{}).", queue.len()),
                    "See positions and ids with `spotify queue`. Items in Spotify's own upcoming list cannot be removed (Spotify has no API for it).",
                )
            })
    };
    let result = match request.op.as_str() {
        "queue.remove" => {
            let reference = a.item.ok_or_else(|| {
                Error::invalid(
                    "Say which item to remove.",
                    "spotify queue remove <position|id|uri>",
                )
            })?;
            let index = find(&live.queue.items, &reference)?;
            let removed = live.queue.items.remove(index);
            if index == 0
                && live
                    .queue
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.uri == removed.uri)
            {
                live.queue.pending = None;
            }
            json!({"removed": removed, "queue": live.queue.items})
        }
        "queue.move" => {
            let reference = a.from.ok_or_else(|| {
                Error::invalid(
                    "Say which item to move.",
                    "spotify queue move <position|id> <new position>",
                )
            })?;
            let index = find(&live.queue.items, &reference)?;
            let to = a.to.unwrap_or(1).clamp(1, live.queue.items.len()) - 1;
            let item = live.queue.items.remove(index);
            live.queue.items.insert(to, item.clone());
            json!({"moved": item, "position": to + 1, "queue": live.queue.items})
        }
        _ => {
            let cleared = live.queue.items.len();
            live.queue.items.clear();
            live.queue.pending = None;
            if live.queue.managed_now.is_none() {
                live.queue.resume = None;
            }
            json!({"cleared": cleared, "queue": [], "note": "Only the managed queue was cleared; Spotify's own upcoming list is unchanged."})
        }
    };
    daemon.save_queue(&live);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0NiLR6uUU0Mk0bfN4VRu5u";

    fn track_json(id: &str, name: &str, secs: u64) -> Value {
        json!({"id": id, "name": name, "type": "track", "artists": [{"name": "A"}], "duration": {"secs": secs, "nanos": 0}})
    }

    #[test]
    fn every_kind_of_track_lookup_shares_the_envelope() {
        let playlist = json!({
            "playlist": {"id": ID, "name": "Deep focus", "owner": ["someone", "id"], "collaborative": false, "desc": "calm"},
            "tracks": [track_json("4uLU6hMCjMI75M1A2tKUQC", "One", 200), track_json("5uLU6hMCjMI75M1A2tKUQC", "Two", 100)],
        });
        let out = item_envelope("playlist", playlist.clone());
        assert_eq!(out["kind"], json!("playlist"));
        assert_eq!(out["item"]["name"], json!("Deep focus"));
        assert_eq!(out["item"]["uri"], json!(format!("spotify:playlist:{ID}")));
        assert_eq!(out["raw"], playlist);
        // What `playlist show` reports is kept.
        assert_eq!(out["track_count"], json!(2));
        assert_eq!(out["duration_ms"], json!(300_000));
        assert_eq!(out["duration"], json!("5:00"));
        assert_eq!(out["collaborative"], json!(false));
        assert_eq!(out["tracks"][1]["name"], json!("Two"));
        assert_eq!(out["playlist"]["name"], json!("Deep focus"));
        let track = item_envelope("track", track_json(ID, "One", 200));
        for key in ["kind", "item", "raw"] {
            assert!(!out[key].is_null() && !track[key].is_null(), "{key}");
        }
        // `playlist show` keeps its own shape.
        let show = playlist_view(&playlist);
        assert!(show.get("kind").is_none() && show.get("raw").is_none());
        assert_eq!(show["track_count"], json!(2));
    }

    #[test]
    fn reads_wait_out_spotify_players_shared_responses_after_a_write() {
        let now = Instant::now();
        assert_eq!(settle_delay(None, now), None);
        let delay = settle_delay(Some(now), now + Duration::from_millis(100));
        assert_eq!(delay, Some(LIBRARY_SETTLE - Duration::from_millis(100)));
        assert_eq!(settle_delay(Some(now), now + LIBRARY_SETTLE), None);
    }

    #[test]
    fn a_read_in_flight_during_a_write_counts_as_one_when_it_finishes() {
        let ms = Duration::from_millis;
        let start = Instant::now();
        let mut settle = LibrarySettle::default();
        assert_eq!(settle.delay(start), None);
        // A read starts, a write finishes while it is in flight, the read finishes 400 ms later.
        settle.reads += 1;
        settle.wrote(start + ms(100));
        settle.read_finished(start + ms(500));
        // spotify_player may share that read's (older) response until 1 s after it completed.
        assert_eq!(
            settle.delay(start + ms(1_200)),
            Some(LIBRARY_SETTLE - ms(700))
        );
        assert_eq!(settle.delay(start + ms(500) + LIBRARY_SETTLE), None);
        // Reads that overlap no write change nothing.
        settle.reads += 1;
        settle.read_finished(start + ms(5_000));
        assert_eq!(settle.delay(start + ms(5_000)), None);
        // Two overlapping reads: the later one to finish counts.
        settle.reads += 2;
        settle.wrote(start + ms(6_000));
        settle.read_finished(start + ms(6_100));
        settle.read_finished(start + ms(6_300));
        assert_eq!(settle.delay(start + ms(6_300)), Some(LIBRARY_SETTLE));
        assert_eq!(settle.reads, 0);
        assert!(!settle.overlapped);
    }

    #[test]
    fn upcoming_leaves_out_repeats_of_the_current_item() {
        let episode = |id: &str| json!({"id": id, "type": "episode", "name": "Ep", "duration": {"secs": 60, "nanos": 0}});
        let current = "4IzpgR6RCEkRqMHbJF38Wp";
        let repeated = json!({
            "currently_playing": episode(current),
            "queue": (0..10).map(|_| episode(current)).collect::<Vec<_>>(),
        });
        let (items, repeats) = spotify_upcoming(&repeated);
        assert!(items.is_empty());
        assert_eq!(repeats, 10);
        // A normal queue is untouched, including a later repeat of the current track.
        let normal = json!({
            "currently_playing": track_json(ID, "Now", 100),
            "queue": [track_json("4uLU6hMCjMI75M1A2tKUQC", "Next", 100), track_json(ID, "Now", 100)],
        });
        let (items, repeats) = spotify_upcoming(&normal);
        assert_eq!(repeats, 0);
        assert_eq!(
            items.iter().map(|i| i.name.as_str()).collect::<Vec<_>>(),
            vec!["Next", "Now"]
        );
        assert_eq!(items[0].kind, "track");
    }

    /// spotify_player's view of playback: `item` (`spotify:<kind>:<id>`) in `context`
    /// (`(uri, type)`) with `repeat`.
    fn view(item: &str, context: Option<(&str, &str)>, repeat: &str) -> WebPlayback {
        let mut parts = item.split(':').skip(1);
        let (kind, id) = (parts.next().unwrap_or("track"), parts.next().unwrap_or(ID));
        let context = context.map_or(Value::Null, |(uri, kind)| json!({"uri": uri, "type": kind}));
        WebPlayback::from_player_json(&json!({
            "item": {"id": id, "type": kind},
            "context": context,
            "repeat_state": repeat,
            "is_playing": false,
        }))
        .expect("playback")
    }

    #[test]
    fn the_repeats_note_names_only_a_cause_the_view_shows() {
        let track = format!("spotify:track:{ID}");
        let queue = json!({"currently_playing": track_json(ID, "Chandni Raat", 191)});
        let album = Some(("spotify:album:37fimO5ahI9qtvEN7OqlME", "album"));
        // The end of a one-track album with repeat off: not an episode, not repeat-one.
        let off = view(&track, album, "off");
        let end = repeats_note(10, &queue, Some(&off), Some(false), false);
        assert_eq!(
            end,
            "Spotify listed the track playing now 10 more time(s) as upcoming (nothing else is up next in its album); those are left out."
        );
        // Without Spotify.app's answer the view alone decides.
        assert_eq!(repeats_note(10, &queue, Some(&off), None, false), end);
        // A track played on its own.
        let alone = repeats_note(10, &queue, Some(&view(&track, None, "off")), None, false);
        assert!(
            alone.contains("(the track was played without a context)"),
            "{alone}"
        );
        // Repeat-one, whatever the context.
        let repeat_one = view(&track, album, "track");
        let one = repeats_note(3, &queue, Some(&repeat_one), Some(true), true);
        assert!(one.contains("(repeat-one is on)"), "{one}");
        // An episode played on its own.
        let episode = "4IzpgR6RCEkRqMHbJF38Wp";
        let queue_episode = json!({"currently_playing": {"id": episode, "type": "episode"}});
        let view_episode = view(&format!("spotify:episode:{episode}"), None, "off");
        assert_eq!(
            repeats_note(10, &queue_episode, Some(&view_episode), Some(false), false),
            "Spotify listed the episode playing now 10 more time(s) as upcoming (the episode was played without a context); those are left out."
        );
        // No cause is claimed without a view of this item, or when none of the causes shows.
        let bare =
            "Spotify listed the track playing now 10 more time(s) as upcoming; those are left out.";
        assert_eq!(repeats_note(10, &queue, None, Some(false), false), bare);
        let other = view("spotify:track:4uLU6hMCjMI75M1A2tKUQC", None, "track");
        assert_eq!(
            repeats_note(10, &queue, Some(&other), Some(true), false),
            bare
        );
        assert_eq!(
            repeats_note(10, &queue, Some(&off), Some(false), true),
            bare
        );
        let context_repeat = view(&track, album, "context");
        assert_eq!(
            repeats_note(10, &queue, Some(&context_repeat), Some(true), false),
            bare
        );
        // The view lags Spotify.app by up to a refresh: a repeat setting Spotify.app contradicts
        // (repeat-one switched on or off in the app since) names no cause.
        assert_eq!(
            repeats_note(10, &queue, Some(&off), Some(true), false),
            bare
        );
        assert_eq!(
            repeats_note(10, &queue, Some(&repeat_one), Some(false), false),
            bare
        );
        for note in [end, alone, bare.to_owned()] {
            assert!(
                !note.contains("episode") && !note.contains("repeat-one"),
                "{note}"
            );
        }
    }

    #[test]
    fn known_facts_are_completed_field_by_field() {
        // A search hit that names the episode but not its show (spotify_player's hits carry none).
        let known = Known {
            uri: format!("spotify:episode:{ID}"),
            name: Some("How to Speak Clearly".into()),
            by: Some("  ".into()),
            duration_ms: Some(8_780_886),
        };
        let facts = known.facts();
        assert_eq!(
            facts,
            (Some("How to Speak Clearly".into()), None, Some(8_780_886))
        );
        assert!(!complete(&facts));
        let found = (
            Some("How to Speak Clearly & With Confidence | Matt Abrahams".into()),
            Some("Huberman Lab".into()),
            Some(8_780_000),
        );
        // What the caller knew wins; only the gap is filled.
        let merged = merge(facts, found);
        assert_eq!(
            merged,
            (
                Some("How to Speak Clearly".into()),
                Some("Huberman Lab".into()),
                Some(8_780_886)
            )
        );
        assert!(complete(&merged));
        assert_eq!(
            merge(TrackFacts::default(), (None, Some("Show".into()), None)),
            (None, Some("Show".into()), None)
        );
    }

    fn queued(id: &str, uri: &str, facts: TrackFacts) -> QueueItem {
        QueueItem {
            id: id.into(),
            uri: uri.into(),
            name: facts.0,
            by: facts.1,
            duration_ms: facts.2,
            added_at: String::new(),
            added_by: None,
            attempts: 0,
        }
    }

    #[test]
    fn queued_episodes_missing_facts_are_looked_up_again_a_few_times() {
        let episode = |id: &str| format!("spotify:episode:{id}");
        let full = (Some("Ep".into()), Some("Show".into()), Some(60_000));
        let mut live = Live::default();
        live.queue.items = vec![
            queued(
                "q_1",
                &episode("4IzpgR6RCEkRqMHbJF38Wp"),
                TrackFacts::default(),
            ),
            queued(
                "q_2",
                &episode("5IzpgR6RCEkRqMHbJF38Wp"),
                (Some("Ep".into()), None, Some(60_000)),
            ),
            queued("q_3", &episode("6IzpgR6RCEkRqMHbJF38Wp"), full),
            // Tracks come from spotify_player, not this lookup.
            queued("q_4", &format!("spotify:track:{ID}"), TrackFacts::default()),
            queued(
                "q_5",
                &episode("7IzpgR6RCEkRqMHbJF38Wp"),
                TrackFacts::default(),
            ),
        ];
        live.fact_tries.insert("q_5".into(), FACT_TRIES);
        live.fact_tries.insert("q_2".into(), FACT_TRIES - 1);
        assert_eq!(
            facts_wanted(&live),
            vec![
                ("q_1".to_owned(), "4IzpgR6RCEkRqMHbJF38Wp".to_owned()),
                ("q_2".to_owned(), "5IzpgR6RCEkRqMHbJF38Wp".to_owned()),
            ]
        );
    }

    #[test]
    fn found_facts_fill_only_what_the_queued_item_lacks() {
        let daemon = daemon_playing(&format!("spotify:track:{ID}"));
        daemon.live().queue.items = vec![queued(
            "q_1",
            "spotify:episode:4IzpgR6RCEkRqMHbJF38Wp",
            (Some("Known name".into()), None, None),
        )];
        fill_item(
            &daemon,
            "q_1",
            (
                Some("Web name".into()),
                Some("Huberman Lab".into()),
                Some(8_780_886),
            ),
        );
        // An item no longer queued is left alone.
        fill_item(&daemon, "q_gone", (Some("x".into()), None, None));
        let live = daemon.live();
        let item = &live.queue.items[0];
        assert_eq!(item.name.as_deref(), Some("Known name"));
        assert_eq!(item.by.as_deref(), Some("Huberman Lab"));
        assert_eq!(item.duration_ms, Some(8_780_886));
        assert_eq!(live.queue.items.len(), 1);
        assert_eq!(episode_missing_facts(item), None);
    }

    #[tokio::test]
    async fn a_rate_limited_lookup_is_not_a_try_and_nothing_is_sent_meanwhile() {
        use std::sync::atomic::Ordering;
        let daemon = daemon_playing(&format!("spotify:track:{ID}"));
        daemon.live().queue.items = vec![queued(
            "q_1",
            "spotify:episode:4IzpgR6RCEkRqMHbJF38Wp",
            TrackFacts::default(),
        )];
        // While an earlier 429's Retry-After runs, lookups answer `Limited` without a request
        // (the token here is fake: a request would fail, not be limited).
        let before = WEB_API_PAUSED_UNTIL_MS.load(Ordering::Relaxed);
        WEB_API_PAUSED_UNTIL_MS.store(
            silicon_spotify_client::model::now_ms() + 60_000,
            Ordering::Relaxed,
        );
        assert!(web_api_pause_left() > Duration::from_secs(50));
        let tokens = ["not-a-token".to_owned()];
        assert_eq!(
            episode_facts(&tokens, "4IzpgR6RCEkRqMHbJF38Wp").await,
            Fetched::Limited
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(1);
        assert!(backfill_round(&daemon, &tokens, deadline).await);
        WEB_API_PAUSED_UNTIL_MS.store(before, Ordering::Relaxed);
        // The item stays wanted, with no try used up.
        let live = daemon.live();
        assert_eq!(live.fact_tries.get("q_1"), None);
        assert_eq!(facts_wanted(&live).len(), 1);
    }

    #[test]
    fn nothing_is_looked_up_later_when_no_queued_episode_lacks_facts() {
        let daemon = Arc::new(daemon_playing(&format!("spotify:track:{ID}")));
        daemon.live().queue.items = vec![
            queued(
                "q_1",
                "spotify:episode:4IzpgR6RCEkRqMHbJF38Wp",
                (Some("Ep".into()), Some("Show".into()), Some(1)),
            ),
            queued("q_2", &format!("spotify:track:{ID}"), TrackFacts::default()),
        ];
        daemon.live().fact_tries.insert("q_gone".into(), 1);
        assert!(!fill_facts_later(&daemon, &Settings::default()));
        let live = daemon.live();
        assert!(!live.facts_backfill);
        // Tries of items no longer queued are forgotten.
        assert!(live.fact_tries.is_empty());
    }

    #[test]
    fn the_pending_note_counts_the_episodes_and_names_only_a_seen_rate_limit() {
        assert_eq!(
            pending_note(1, true),
            "Spotify's Web API has not given the name, show or length of 1 queued episode yet (Spotify is rate-limiting this client right now). The daemon asks again in the background; `spotify queue` shows what it finds."
        );
        let slow = pending_note(2, false);
        assert!(slow.contains("of 2 queued episodes yet. "), "{slow}");
        assert!(!slow.contains("rate-limiting"), "{slow}");
    }

    #[test]
    fn unqueueable_items_are_named_with_the_right_article_and_hint() {
        let error = |kind: Kind| not_queueable(&SpotifyUri::new(kind, ID).expect("uri"));
        let album = error(Kind::Album);
        assert!(album.message.contains("is an album;"), "{}", album.message);
        assert!(album.hint.contains("spotify play"));
        assert!(error(Kind::Artist).message.contains("is an artist;"));
        assert!(error(Kind::Playlist).message.contains("is a playlist;"));
        let show = error(Kind::Show);
        assert!(show.message.contains("is a show;"));
        assert!(show.hint.contains("spotify podcast play"));
        assert_eq!(album.code, "invalid_input");
    }

    /// Spotify.app playing `uri` (every script answers with its status).
    struct Playing(String);

    impl Runner for Playing {
        fn run(&self, _source: &str) -> Result<String> {
            let s = applescript::SEP;
            Ok(format!(
                "ok{s}playing{s}50{s}false{s}false{s}60000{s}{}{s}Song{s}A{s}Album{s}A{s}200000{s}1{s}1{s}0{s}{s}{s}true{s}true",
                self.0
            ))
        }
    }

    fn daemon_playing(uri: &str) -> Daemon {
        Daemon {
            db: Db::open(std::path::Path::new(":memory:")).expect("db"),
            dir: std::env::temp_dir(),
            script: Arc::new(Playing(uri.to_owned())),
            started_at: String::new(),
            started: Instant::now(),
            live: Mutex::default(),
            observe_lock: Mutex::new(()),
            hand_off: Mutex::new(()),
            library: Mutex::default(),
            nudge: Notify::new(),
            deliver: Notify::new(),
            events: broadcast::channel(4).0,
            settings: Mutex::default(),
            warm: Mutex::new(Value::Null),
            warm_pid: std::sync::atomic::AtomicU32::new(0),
            update: Mutex::new(Value::Null),
            shutdown: Notify::new(),
        }
    }

    fn outcome_on(daemon: &Daemon) -> Result<silicon_spotify_client::control::Outcome> {
        Ok(silicon_spotify_client::control::Outcome {
            action: "play".into(),
            via: silicon_spotify_client::control::Via::Applescript,
            fallback: None,
            result: None,
            playback: daemon.read()?,
        })
    }

    #[test]
    fn explicit_changes_keep_the_managed_item_only_while_it_plays() {
        let managed = format!("spotify:track:{ID}");
        let other = "spotify:track:4uLU6hMCjMI75M1A2tKUQC";
        let failed = || Err(Error::new("verification_failed", "no", "retry"));
        // A failed change that left the managed item playing keeps it managed.
        let daemon = daemon_playing(&managed);
        daemon.live().queue.managed_now = Some(managed.clone());
        assert!(explicit_change(&daemon, |_| failed()).is_err());
        assert_eq!(daemon.live().queue.managed_now.as_deref(), Some(&*managed));
        // So does a `previous` that restarted it.
        let restarted = explicit_change(&daemon, outcome_on).expect("restart");
        assert_eq!(restarted["playback"]["track"]["uri"], json!(managed));
        assert_eq!(daemon.live().queue.managed_now.as_deref(), Some(&*managed));
        // Anything else playing now is the user's, not the queue's.
        let daemon = daemon_playing(other);
        daemon.live().queue.managed_now = Some(managed.clone());
        assert!(explicit_change(&daemon, |_| failed()).is_err());
        assert_eq!(daemon.live().queue.managed_now, None);
        daemon.live().queue.managed_now = Some(managed);
        explicit_change(&daemon, outcome_on).expect("play");
        assert_eq!(daemon.live().queue.managed_now, None);
    }

    #[test]
    fn a_slow_explicit_change_reopens_the_hold_and_drops_a_hand_off_decided_meanwhile() {
        let daemon = daemon_playing("spotify:track:4uLU6hMCjMI75M1A2tKUQC");
        let claim = crate::queue::Pending {
            uri: format!("spotify:track:{ID}"),
            since_ms: 1,
            sent: false,
        };
        let changed = explicit_change(&daemon, |d| {
            // The change outlasted the hold, and the watcher decided on a hand-off meanwhile.
            let mut live = d.live();
            live.queue.hold_until_ms = 0;
            live.queue.pending = Some(claim.clone());
            drop(live);
            outcome_on(d)
        });
        assert!(changed.is_ok());
        let mut live = daemon.live();
        assert!(live.queue.hold_until_ms > silicon_spotify_client::model::now_ms());
        // The watcher finds its claim gone and sends nothing.
        assert!(!live.queue.send_claimed(&claim));
        drop(live);
        // A failed change does not reopen the hold.
        daemon.live().queue.hold_until_ms = 0;
        let failed = explicit_change(&daemon, |d| {
            d.live().queue.hold_until_ms = 0;
            Err(Error::new("verification_failed", "no", "retry"))
        });
        assert!(failed.is_err());
        assert_eq!(daemon.live().queue.hold_until_ms, 0);
    }

    #[test]
    fn liked_comes_from_the_contains_answer_for_one_song() {
        assert_eq!(liked_from_web(&json!([true])), Some(true));
        assert_eq!(liked_from_web(&json!([false])), Some(false));
        // Anything else (an error body, several answers) leaves `liked` out.
        for other in [
            json!([]),
            json!([true, false]),
            json!(["yes"]),
            json!({"error": {"status": 429}}),
            Value::Null,
        ] {
            assert_eq!(liked_from_web(&other), None, "{other}");
        }
    }

    #[test]
    fn rate_limits_pause_web_api_lookups_for_their_retry_after() {
        assert_eq!(rate_limit_pause(Some("7")), Duration::from_secs(7));
        assert_eq!(rate_limit_pause(Some(" 12 ")), Duration::from_secs(12));
        // No usable header: a default pause; a bogus one is capped.
        assert_eq!(rate_limit_pause(None), Duration::from_secs(30));
        assert_eq!(
            rate_limit_pause(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            Duration::from_secs(30)
        );
        assert_eq!(rate_limit_pause(Some("86400")), Duration::from_secs(600));
        assert_eq!(rate_limit_pause(Some("0")), Duration::from_secs(1));
    }

    #[test]
    fn only_transient_read_failures_are_retried() {
        let transport = silicon_spotify_client::player::classify(
            "error sending request for url (https://api.spotify.com/v1/me/tracks?offset=450)",
            "",
            &["get", "key", "user-liked-tracks"],
        );
        assert_eq!(transport.code, "transport");
        assert!(retry_read(&transport));
        let busy = silicon_spotify_client::player::classify(
            "Address already in use",
            "",
            &["get", "key", "user-playlists"],
        );
        assert!(retry_read(&busy));
        let limited = silicon_spotify_client::player::classify(
            "429 Too Many Requests",
            "",
            &["get", "key", "user-liked-tracks"],
        );
        assert!(limited.retryable && !retry_read(&limited));
        let missing = silicon_spotify_client::player::classify(
            "404 not found",
            "",
            &["get", "item", "--id", ID, "playlist"],
        );
        assert!(!retry_read(&missing));
        let slow = Error::new("timeout", "slow", "retry").retryable();
        assert!(!retry_read(&slow));
    }

    #[test]
    fn episodes_are_described_from_the_web_api() {
        let value = json!({"id": ID, "name": "How to Speak Clearly", "duration_ms": 7_320_000, "show": {"name": "Huberman Lab"}});
        assert_eq!(
            episode_from_web(&value),
            (
                Some("How to Speak Clearly".into()),
                Some("Huberman Lab".into()),
                Some(7_320_000)
            )
        );
        assert_eq!(episode_from_web(&json!({})), (None, None, None));
        // A blank show is missing, so it stays wanted rather than recorded as known.
        let blank = json!({"name": "Ep", "duration_ms": 1, "show": {"name": " "}});
        assert_eq!(episode_from_web(&blank), (Some("Ep".into()), None, Some(1)));
    }

    #[test]
    fn created_playlist_ids_are_bare_whatever_spotify_player_prints() {
        for stdout in [
            format!("Playlist 'Deep focus' with id 'spotify:playlist:{ID}' was created."),
            format!("Playlist 'Deep focus' with id '{ID}' was created."),
            // A name that looks like the marker does not confuse it.
            format!("Playlist 'with id 'x'' with id 'spotify:playlist:{ID}' was created."),
        ] {
            assert_eq!(
                created_playlist_id(&stdout).as_deref(),
                Some(ID),
                "{stdout}"
            );
        }
        assert_eq!(created_playlist_id("Playlist 'x' was created."), None);
        assert_eq!(
            created_playlist_id("Playlist 'x' with id 'spotify:track:abc' was created."),
            None
        );
    }

    #[test]
    fn new_playlist_args_survive_dashes_and_empty_descriptions() {
        assert_eq!(
            new_playlist_args("Deep focus", "", false, false),
            ["playlist", "new", "--", "Deep focus"]
        );
        assert_eq!(
            new_playlist_args("-80s-", "- no vocals", true, true),
            [
                "playlist",
                "new",
                "--public",
                "--collab",
                "--",
                "-80s-",
                "- no vocals"
            ]
        );
    }

    #[test]
    fn forked_playlists_are_parsed_from_the_report() {
        let stdout = format!(
            "Forking '37i9dQZF1DXcBWIGoYBM5M'...\n\nForked 37i9dQZF1DXcBWIGoYBM5M.\nNew playlist: {ID}:Deep focus: late\nImporting from 37i9dQZF1DXcBWIGoYBM5M:Deep focus: late to {ID}:Deep focus: late...\nNew tracks imported to Deep focus: late:"
        );
        assert_eq!(
            forked_playlist(&stdout),
            Some((ID.to_owned(), "Deep focus: late".to_owned()))
        );
        let uri_form = format!("Forked x.\nNew playlist: spotify:playlist:{ID}:Mix\n");
        assert_eq!(
            forked_playlist(&uri_form),
            Some((ID.to_owned(), "Mix".to_owned()))
        );
        let two_lines = format!(
            "Forked x.\nNew playlist: {ID}:Late night\nfocus, part 2\nImporting from x:y to {ID}:z..."
        );
        assert_eq!(
            forked_playlist(&two_lines).map(|(_, name)| name).as_deref(),
            Some("Late night\nfocus, part 2")
        );
        let id_only = format!("Forked x.\nNew playlist: {ID}\nImporting from x:y to {ID}:z...");
        assert_eq!(
            forked_playlist(&id_only),
            Some((ID.to_owned(), String::new()))
        );
        assert_eq!(forked_playlist("Forked x.\nImporting…"), None);
    }
}
