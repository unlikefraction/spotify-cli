//! The daemon's shared state and request handlers (everything except triggers).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silicon_spotify_client::applescript::{self, Runner};
use silicon_spotify_client::control::{Controller, PlayTarget, RepeatMode, Strategy, VolumeTarget};
use silicon_spotify_client::ipc::Request;
use silicon_spotify_client::model::{Item, Playback, PlayerState, now_rfc3339};
use silicon_spotify_client::player::SpotifyPlayer;
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
                // Starting something explicit abandons the managed-queue resume point.
                if !matches!(a.target, PlayTarget::Resume) {
                    let mut live = d.live();
                    live.queue.resume = None;
                    live.queue.hold(silicon_spotify_client::model::now_ms());
                    d.save_queue(&live);
                }
                d.with_controller(&s, |c| c.play(&a.target))
                    .map(|o| outcome_json(&o))
            })
            .await;
            daemon.nudge.notify_one();
            result
        }
        "player.pause" => control(daemon, settings, |c| c.pause()).await,
        "player.toggle" => control(daemon, settings, |c| c.toggle()).await,
        "player.previous" => {
            hold(daemon);
            control(daemon, settings, |c| c.previous()).await
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
                Ok(a) => control(daemon, settings, move |c| c.like(a.like)).await,
                Err(e) => Err(e),
            }
        }
        "spotify.launch" => {
            blocking(move || {
                silicon_spotify_client::control::launch_spotify()?;
                let deadline = Instant::now() + Duration::from_secs(20);
                loop {
                    std::thread::sleep(Duration::from_millis(400));
                    let playback = d.read()?;
                    if playback.state != PlayerState::NotRunning {
                        return Ok(json!({"launched": true, "playback": playback}));
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
            let s = settings.clone();
            blocking(move || track_info(&d, &s, a.uri.as_ref())).await
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
            blocking(move || library(&s, &a.key, a.limit)).await
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

/// `next`: plays the managed queue's head when it has items, else Spotify's next.
async fn next(daemon: &Arc<Daemon>, settings: Settings) -> Result<Value> {
    let has_queue = !daemon.live().queue.items.is_empty();
    if has_queue {
        let d = Arc::clone(daemon);
        let played = blocking(move || crate::watcher::advance_queue(&d, &settings)).await?;
        daemon.nudge.notify_one();
        return Ok(played);
    }
    hold(daemon);
    control(daemon, settings, |c| c.next()).await
}

/// An explicit item change: the queue must not override it.
fn hold(daemon: &Daemon) {
    let mut live = daemon.live();
    live.queue.hold(silicon_spotify_client::model::now_ms());
    daemon.save_queue(&live);
}

fn track_info(daemon: &Daemon, settings: &Settings, uri: Option<&SpotifyUri>) -> Result<Value> {
    match uri {
        Some(uri) => {
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
            let value = settings
                .authed_player()?
                .json(&["get", "item", "--id", &uri.id, kind])?;
            if kind == "playlist" {
                return Ok(playlist_view(&value));
            }
            Ok(json!({"item": Item::from_player_json(kind, &value), "raw": value}))
        }
        None => {
            let playback = daemon.read()?;
            let track = playback.track.clone().ok_or_else(Error::nothing_playing)?;
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
            Ok(json!({
                "track": track,
                "playback": {
                    "state": playback.state, "position_ms": playback.position_ms, "position": playback.position,
                    "remaining_ms": playback.remaining_ms, "progress": playback.progress,
                },
                "artists": artists,
                "album": web.get("album"),
                "explicit": web.get("explicit"),
                "warnings": warnings,
            }))
        }
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
    let value = settings
        .authed_player()?
        .json(&["get", "key", player_key])?;
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

fn playlist_view(value: &Value) -> Value {
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
    json!({
        "playlist": item,
        "collaborative": playlist.get("collaborative"),
        "owner": playlist.get("owner"),
        "track_count": tracks.len(),
        "duration": silicon_spotify_client::timing::clock(total_ms),
        "tracks": tracks,
    })
}

async fn playlist(daemon: &Arc<Daemon>, request: &Request, settings: Settings) -> Result<Value> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct A {
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
    let a: A = args(request)?;
    let op = request.op.clone();
    let _ = daemon;
    blocking(move || {
        let player = settings.authed_player()?;
        let id = |what: &str| -> Result<String> {
            let raw = a.id.clone().ok_or_else(|| Error::invalid(format!("{what} needs a playlist id."), "List ids with `spotify playlist list`."))?;
            Ok(SpotifyUri::parse(&raw, Some(Kind::Playlist))?.id)
        };
        match op.as_str() {
            "playlist.list" => library(&settings, "playlists", a.limit),
            "playlist.show" => Ok(playlist_view(&player.json(&["get", "item", "--id", &id("show")?, "playlist"])?)),
            "playlist.create" => {
                let name = a.name.clone().filter(|n| !n.trim().is_empty()).ok_or_else(|| Error::invalid("A playlist needs a name.", "Example: spotify playlist create 'Deep focus' --description 'no vocals'"))?;
                let description = a.description.clone().unwrap_or_default();
                let mut args = vec!["playlist", "new"];
                if a.public {
                    args.push("--public");
                }
                if a.collab {
                    args.push("--collab");
                }
                args.push(&name);
                args.push(&description);
                let output = player.run(&args)?;
                // "Playlist 'NAME' with id 'ID' was created."
                let id = output.stdout.rsplit_once("with id '").and_then(|(_, rest)| rest.split('\'').next()).map(str::to_owned);
                Ok(json!({"created": true, "id": id, "uri": id.as_ref().map(|i| format!("spotify:playlist:{i}")), "name": name, "public": a.public, "collaborative": a.collab, "message": output.stdout}))
            }
            "playlist.delete" => {
                let id = id("delete")?;
                let output = player.run(&["playlist", "delete", &id])?;
                let unfollowed = !output.stdout.contains("nothing to be done");
                Ok(json!({"deleted": unfollowed, "id": id, "message": output.stdout,
                    "note": "Spotify has no hard delete: this unfollows the playlist (it disappears from your library; collaborators and followers keep it)."}))
            }
            "playlist.add" | "playlist.remove" => {
                let playlist = id(if op == "playlist.add" { "add" } else { "remove" })?;
                if a.items.is_empty() {
                    return Err(Error::invalid("No tracks or albums given.", "Example: spotify playlist add <playlist> spotify:track:<id> spotify:album:<id>"));
                }
                let action = if op == "playlist.add" { "add" } else { "delete" };
                let mut results = Vec::new();
                for item in &a.items {
                    let flag = match item.kind {
                        Kind::Track => "--track-id",
                        Kind::Album => "--album-id",
                        _ => {
                            return Err(Error::unsupported(
                                format!("{} cannot be added to playlists through spotify_player (tracks and albums only).", item.kind),
                                "Pass spotify:track:<id> or spotify:album:<id> items.",
                            ));
                        }
                    };
                    let output = player.run(&["playlist", "edit", flag, &item.id, action, &playlist])?;
                    results.push(json!({"item": item.uri(), "message": output.stdout}));
                }
                Ok(json!({"playlist": format!("spotify:playlist:{playlist}"), "action": action, "results": results}))
            }
            "playlist.rename" => Err(Error::unsupported(
                "Renaming or re-describing a playlist is not possible through spotify_player or AppleScript.",
                "Rename it in the Spotify app. Everything else (create, delete, add, remove, import, fork, sync) works here.",
            )),
            "playlist.import" => {
                let from = SpotifyUri::parse(a.from.as_deref().unwrap_or_default(), Some(Kind::Playlist))?.id;
                let to = SpotifyUri::parse(a.to.as_deref().unwrap_or_default(), Some(Kind::Playlist))?.id;
                let mut args = vec!["playlist", "import"];
                if a.delete {
                    args.push("--delete");
                }
                args.push(&from);
                args.push(&to);
                let output = player.run(&args)?;
                Ok(json!({"imported": true, "from": from, "to": to, "message": output.stdout}))
            }
            "playlist.fork" => {
                let id = id("fork")?;
                let output = player.run(&["playlist", "fork", &id])?;
                Ok(json!({"forked": true, "from": id, "message": output.stdout}))
            }
            "playlist.sync" => {
                let mut args = vec!["playlist", "sync"];
                if a.delete {
                    args.push("--delete");
                }
                let id = a.id.as_deref().map(|raw| SpotifyUri::parse(raw, Some(Kind::Playlist)).map(|u| u.id)).transpose()?;
                if let Some(id) = &id {
                    args.push(id);
                }
                let output = player.run(&args)?;
                Ok(json!({"synced": true, "id": id, "message": output.stdout}))
            }
            other => Err(Error::invalid(format!("Unknown playlist op `{other}`."), "Run `spotify playlist --help`.")),
        }
    })
    .await
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

async fn queue_list(daemon: &Arc<Daemon>, settings: Settings) -> Result<Value> {
    let (managed, managed_now, resume) = {
        let live = daemon.live();
        (
            live.queue.items.clone(),
            live.queue.managed_now.clone(),
            live.queue.resume.clone(),
        )
    };
    let upcoming = blocking(move || -> Result<Value> {
        match settings
            .authed_player()
            .and_then(|p| p.json(&["get", "key", "queue"]))
        {
            Ok(value) => {
                let items: Vec<Item> = value
                    .get("queue")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| {
                                let kind = v.get("type").and_then(Value::as_str).unwrap_or("track");
                                Item::from_player_json(kind, v)
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(json!({"items": items}))
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

/// (name, artists, duration) looked up for a queued item.
type TrackFacts = (Option<String>, Option<String>, Option<u64>);

async fn queue_add(daemon: &Arc<Daemon>, request: &Request, settings: Settings) -> Result<Value> {
    #[derive(Deserialize)]
    struct A {
        uris: Vec<SpotifyUri>,
        #[serde(default)]
        next: bool,
    }
    let a: A = args(request)?;
    if a.uris.is_empty() {
        return Err(Error::invalid(
            "Nothing to queue.",
            "Example: spotify queue add spotify:track:<id> [more…]",
        ));
    }
    for uri in &a.uris {
        if !matches!(uri.kind, Kind::Track | Kind::Episode) {
            return Err(Error::invalid(
                format!(
                    "{} is a {}; the queue holds tracks and episodes.",
                    uri.uri(),
                    uri.kind
                ),
                "To play a whole album/playlist use `spotify play <uri>`; to queue its tracks, list them with `spotify playlist show <id>` and queue those.",
            ));
        }
    }
    let who = request.isi.clone().or_else(|| request.home.clone());
    let uris = a.uris.clone();
    let details = blocking(move || -> Result<Vec<TrackFacts>> {
        let player = settings.authed_player().ok();
        Ok(uris
            .iter()
            .map(|uri| {
                if uri.kind != Kind::Track {
                    return (None, None, None);
                }
                player
                    .as_ref()
                    .and_then(|p| p.json(&["get", "item", "--id", &uri.id, "track"]).ok())
                    .and_then(|v| Item::from_player_json("track", &v))
                    .map_or((None, None, None), |item| {
                        (Some(item.name), Some(item.by.join(", ")), item.duration_ms)
                    })
            })
            .collect())
    })
    .await?;
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
    Ok(json!({"added": added, "queue": queue}))
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
