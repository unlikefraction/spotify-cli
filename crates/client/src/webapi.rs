//! Starting tracks, episodes, Liked Songs and shows through the Spotify Web API, so Spotify.app
//! stays in the background.
//!
//! AppleScript's `play track` makes Spotify.app come to the front. `PUT /v1/me/player/play` does
//! not: it tells a Spotify Connect device what to play, and it plays it where it is. A single
//! track sent as a list of ids leaves the desktop app stopped with nothing loaded, so a track or
//! episode start always names a list (`context_uri`) and the item in it (`offset`): the caller's
//! `--context`, else the track's album or the episode's show, looked up once per item
//! (`GET /v1/tracks/{id}`, `GET /v1/episodes/{id}`, with `market=from_token`). That lookup tells
//! an item not playable in the account's market ([`not_playable`]: nothing is started), and a
//! song that plays from another release there (track relinking): its answer is that release
//! (`id`, `linked_from`) but the album of the release asked for, which lists the song under the
//! id it links from, so the start names it by that id ([`ItemFacts::listed_as`]). Liked Songs
//! (`spotify:user:<id>:collection`) and shows start as lists of their own ([`start_list`]).
//!
//! Where a start goes is decided at each start from `GET /v1/me/player/devices`
//! ([`choose_device`]): when Spotify.app on this Mac is the remote for another active device (a
//! speaker, a phone), the start plays there, so it never moves playback off that device;
//! otherwise it goes to Spotify.app on this Mac by device id, which works when no device is
//! active. When Spotify.app plays but nothing listed is active, it plays on a device the Web API
//! does not list: the start names no device, so it plays there too. Never to spotify_player's
//! own device.
//!
//! Requests carry spotify_player's cached access token ([`cached_access_tokens`]; the daemon's
//! warm copy keeps it fresh) and share its client's rate limit. After a 429 nothing is sent
//! until its `Retry-After` has passed ([`pause_left`]): the daemon's own lookups observe the
//! same pause.
//!
//! [`crate::control::Controller`] verifies every start against Spotify.app and falls back to
//! AppleScript when it did not take effect. A start whose answer was a 5xx or never came may
//! still have gone through ([`unanswered`]): the controller looks for it in Spotify.app first,
//! so nothing is started twice.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::{Value, json};

use crate::model::{Device, Track, WebItem};
use crate::player::SpotifyPlayer;
use crate::uri::{Kind, SpotifyUri};
use crate::{Error, Result};

/// The Spotify Web API's origin and version.
pub const BASE: &str = "https://api.spotify.com/v1";

/// HTTP methods the controller uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `GET`.
    Get,
    /// `PUT`.
    Put,
}

impl Method {
    /// The method's name.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Get => "GET",
            Self::Put => "PUT",
        }
    }
}

/// One Web API answer.
#[derive(Clone, Debug, PartialEq)]
pub struct Reply {
    /// HTTP status.
    pub status: u16,
    /// The JSON body (`Null` when empty or not JSON).
    pub body: Value,
    /// The `Retry-After` header of a 429.
    pub retry_after: Option<String>,
}

/// Sends Spotify Web API requests. [`HttpWebApi`] is the real one; tests use fakes.
pub trait WebApi: Send + Sync {
    /// Sends `method` to `BASE + path` with `query` and a JSON `body`.
    ///
    /// # Errors
    /// Only when no answer came (no token, the network, a timeout); every HTTP status is a
    /// [`Reply`].
    fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Reply>;

    /// How long requests still stay away after a rate limit (default: the process-wide pause
    /// the daemon's lookups share, [`pause_left`]).
    fn pause_left(&self) -> Duration {
        pause_left()
    }

    /// Records a 429 and its `Retry-After` (default: [`pause_after_rate_limit`]).
    fn rate_limited(&self, retry_after: Option<&str>) {
        pause_after_rate_limit(retry_after);
    }
}

// ---------------------------------------------------------------------------------------------
// Rate limits

/// Until when (unix ms) Web API requests stay away after a rate limit.
static PAUSED_UNTIL_MS: AtomicU64 = AtomicU64::new(0);

/// How long Web API requests still stay away after a rate limit (zero when they do not).
#[must_use]
pub fn pause_left() -> Duration {
    let until = PAUSED_UNTIL_MS.load(Ordering::Relaxed);
    Duration::from_millis(until.saturating_sub(crate::model::now_ms()))
}

/// How long to stay away after a 429: its `Retry-After` in seconds, 30 s without a usable one,
/// at most 10 minutes (a bogus header must not disable the Web API for long).
#[must_use]
pub fn rate_limit_pause(retry_after: Option<&str>) -> Duration {
    let secs = retry_after
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(30);
    Duration::from_secs(secs.clamp(1, 600))
}

/// Records a 429: requests stay away for [`rate_limit_pause`] (never shortening a longer pause
/// already running). Returns that pause.
pub fn pause_after_rate_limit(retry_after: Option<&str>) -> Duration {
    let pause = rate_limit_pause(retry_after);
    let until =
        crate::model::now_ms().saturating_add(u64::try_from(pause.as_millis()).unwrap_or(u64::MAX));
    PAUSED_UNTIL_MS.fetch_max(until, Ordering::Relaxed);
    pause
}

/// Sets when the pause ends (unix ms, 0 for none) and returns the previous end. For tests that
/// need a pause, and for putting the previous one back afterwards.
pub fn replace_pause(until_ms: u64) -> u64 {
    PAUSED_UNTIL_MS.swap(until_ms, Ordering::Relaxed)
}

fn paused(left: Duration) -> Error {
    Error::new(
        "rate_limited",
        format!(
            "The Spotify Web API rate limited this client recently; it is left alone for {} s more.",
            left.as_secs().max(1)
        ),
        "Wait until the pause has passed; AppleScript handles playback meanwhile.",
    )
    .retryable()
    .with_details(json!({"retry_after_s": left.as_secs().max(1)}))
}

// ---------------------------------------------------------------------------------------------
// Tokens and the real client

/// The unexpired Web API access tokens in spotify_player's cache folder (`*_token.json`), newest
/// first. Never logged or shown.
#[must_use]
pub fn cached_access_tokens(player: &SpotifyPlayer) -> Vec<String> {
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

/// The Spotify Web API over HTTPS with spotify_player's cached tokens (read at each request, so
/// a refresh by spotify_player is picked up), trying the next token when one is refused (401).
///
/// Blocking, and callable from any thread, inside an async runtime or not: each request is
/// handed to a small runtime of its own (`shared`) from a short-lived thread. That runtime and
/// its HTTP client live as long as the process, so a start's lookup, device list and play request
/// share one connection instead of a TLS handshake each.
#[cfg(feature = "api")]
#[derive(Clone)]
pub struct HttpWebApi {
    player: SpotifyPlayer,
    /// Per request.
    pub timeout: Duration,
}

#[cfg(feature = "api")]
impl std::fmt::Debug for HttpWebApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpWebApi")
            .field("cache", &self.player.cache_folder())
            .field("timeout", &self.timeout)
            .finish_non_exhaustive()
    }
}

#[cfg(feature = "api")]
impl HttpWebApi {
    /// Requests with the tokens of `player`'s cache folder.
    #[must_use]
    pub fn new(player: &SpotifyPlayer) -> Self {
        Self {
            player: player.clone(),
            timeout: Duration::from_secs(3),
        }
    }

    async fn send_async(
        &self,
        client: &reqwest::Client,
        tokens: Vec<String>,
        method: Method,
        url: String,
        query: Vec<(String, String)>,
        body: Option<Value>,
    ) -> Result<Reply> {
        let mut last = None;
        for token in tokens.iter().take(2) {
            let mut request = match method {
                Method::Get => client.get(&url),
                Method::Put => client.put(&url),
            }
            .timeout(self.timeout)
            .bearer_auth(token);
            if !query.is_empty() {
                request = request.query(&query);
            }
            if let Some(body) = &body {
                request = request.json(body);
            }
            let response = request.send().await.map_err(|error| {
                Error::new(
                    "transport",
                    format!(
                        "Could not reach the Spotify Web API: {}.",
                        crate::model::truncate(&error.to_string(), 200)
                    ),
                    "Check the network connection and retry.",
                )
                .retryable()
                // Not connected: nothing was sent. A timeout or a broken answer: it may have been.
                .with_details(json!({"connect": error.is_connect(), "timeout": error.is_timeout()}))
            })?;
            let status = response.status().as_u16();
            let retry_after = response
                .headers()
                .get(reqwest::header::RETRY_AFTER)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned);
            let text = response.text().await.unwrap_or_default();
            let reply = Reply {
                status,
                body: serde_json::from_str(&text).unwrap_or(Value::Null),
                retry_after,
            };
            // That token is stale: the next one (a refused request did nothing).
            if status == 401 {
                last = Some(reply);
                continue;
            }
            return Ok(reply);
        }
        last.ok_or_else(|| Error::internal("no token was tried"))
    }
}

#[cfg(feature = "api")]
impl WebApi for HttpWebApi {
    fn send(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, &str)],
        body: Option<&Value>,
    ) -> Result<Reply> {
        self.player.require_auth()?;
        let tokens = cached_access_tokens(&self.player);
        if tokens.is_empty() {
            return Err(Error::new(
                "web_api_failed",
                "spotify_player's cached Spotify Web API token has expired (the daemon's spotify_player refreshes it while it runs).",
                "Retry in a moment; `spotify daemon status` shows whether its spotify_player runs.",
            )
            .retryable());
        }
        let url = format!("{BASE}{path}");
        let query: Vec<(String, String)> = query
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        let body = body.cloned();
        let shared = shared()?;
        // Its own thread: blocking on a runtime from inside another one (the daemon's) panics.
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    shared.runtime.block_on(self.send_async(
                        &shared.client,
                        tokens,
                        method,
                        url,
                        query,
                        body,
                    ))
                })
                .join()
                .unwrap_or_else(|_| Err(Error::internal("the Web API request panicked")))
        })
    }
}

/// The runtime and HTTP client every [`HttpWebApi`] request uses.
#[cfg(feature = "api")]
struct Shared {
    runtime: tokio::runtime::Runtime,
    client: reqwest::Client,
}

/// [`Shared`], made on first use: one worker thread keeps idle connections alive between
/// requests.
#[cfg(feature = "api")]
fn shared() -> Result<&'static Shared> {
    static SHARED: std::sync::OnceLock<std::result::Result<Shared, String>> =
        std::sync::OnceLock::new();
    SHARED
        .get_or_init(|| {
            crate::api::ensure_crypto();
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .thread_name("spotify-web-api")
                .enable_all()
                .build()
                .map_err(|error| format!("async runtime: {error}"))?;
            // The token goes to api.spotify.com only: never follow a redirect with it.
            let client = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .https_only(true)
                .pool_idle_timeout(Duration::from_secs(90))
                .build()
                .map_err(|error| format!("HTTP client: {error}"))?;
            Ok(Shared { runtime, client })
        })
        .as_ref()
        .map_err(|error| Error::internal(error.clone()))
}

// ---------------------------------------------------------------------------------------------
// Answers

/// A Web API answer as a result: the body of a 2xx, or a classified error. Records a 429's pause.
fn answer(web: &dyn WebApi, request: &str, reply: Reply) -> Result<Value> {
    let status = reply.status;
    if (200..300).contains(&status) {
        return Ok(reply.body);
    }
    let message = reply
        .body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let reason = reply
        .body
        .pointer("/error/reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let details = json!({
        "request": request,
        "status": status,
        "message": crate::model::truncate(&message, 300),
        "reason": if reason.is_empty() { Value::Null } else { json!(reason) },
    });
    let error = match status {
        429 => {
            web.rate_limited(reply.retry_after.as_deref());
            Error::new(
                "rate_limited",
                "The Spotify Web API is rate limiting this account.",
                "Wait a minute and retry; AppleScript handles playback meanwhile.",
            )
            .retryable()
        }
        403 if reason == "PREMIUM_REQUIRED" || message.to_ascii_lowercase().contains("premium") => {
            Error::new(
                "premium_required",
                "Spotify refused this Web API playback command because the account is not Premium.",
                "Playback control through the Web API needs Spotify Premium. AppleScript-based commands (play, pause, next, seek, volume) still work: set `spotify config set '{\"strategy\": \"applescript\"}'`.",
            )
        }
        404 if reason == "NO_ACTIVE_DEVICE" || message.to_ascii_lowercase().contains("device") => {
            Error::new(
                "no_active_device",
                format!("The Spotify Web API does not reach Spotify.app on this Mac ({message})."),
                "Play anything in Spotify.app once; AppleScript handles playback meanwhile.",
            )
        }
        404 => Error::not_found(
            format!(
                "Spotify could not find that item ({}).",
                if message.is_empty() {
                    "HTTP 404"
                } else {
                    &message
                }
            ),
            "Search for the right id with `spotify search '<query>'` and pass its uri.",
        ),
        _ => Error::new(
            "web_api_failed",
            format!(
                "The Spotify Web API answered HTTP {status} to {request}{}.",
                if message.is_empty() {
                    String::new()
                } else {
                    format!(": {message}")
                }
            ),
            "Retry; AppleScript handles playback meanwhile.",
        )
        .retryable(),
    };
    Err(error.with_details(details))
}

/// Whether `error` is a start request's (`PUT /me/player/play`) whose effect is unknown: Spotify
/// answered 5xx, or no answer came although the request may have left (a timeout, a broken
/// connection). The start may have gone through, so it must be looked for in Spotify.app before
/// anything starts it again.
#[must_use]
pub fn unanswered(error: &Error) -> bool {
    error
        .details
        .as_ref()
        .and_then(|details| details.get("unanswered"))
        .and_then(Value::as_bool)
        == Some(true)
}

/// Marks a start request's error [`unanswered`] when its effect is unknown.
fn mark_unanswered(mut error: Error) -> Error {
    let details = error.details.as_ref();
    let unknown = match error.code.as_str() {
        "transport" => {
            details
                .and_then(|d| d.get("connect"))
                .and_then(Value::as_bool)
                != Some(true)
        }
        _ => details
            .and_then(|d| d.get("status"))
            .and_then(Value::as_u64)
            .is_some_and(|status| status >= 500),
    };
    if unknown {
        let mut map = match error.details.take() {
            Some(Value::Object(map)) => map,
            _ => serde_json::Map::new(),
        };
        map.insert("unanswered".into(), json!(true));
        error.details = Some(Value::Object(map));
    }
    error
}

/// Sends a request unless a rate limit's pause runs, and classifies the answer.
fn call(
    web: &dyn WebApi,
    method: Method,
    path: &str,
    query: &[(&str, &str)],
    body: Option<&Value>,
) -> Result<Value> {
    let left = web.pause_left();
    if !left.is_zero() {
        return Err(paused(left));
    }
    let request = format!("{} {path}", method.as_str());
    let reply = web.send(method, path, query, body)?;
    answer(web, &request, reply)
}

// ---------------------------------------------------------------------------------------------
// Items

/// What a start needs to know about a track or episode.
#[derive(Clone, Debug, PartialEq)]
pub struct ItemFacts {
    /// The uri asked for.
    pub requested: String,
    /// The uri that plays in the account's market: the requested one, or the release Spotify
    /// relinks it to (the lookup's `id`).
    pub uri: String,
    /// The uri [`Self::context`] lists the item under, which a start in that list names. For a
    /// relinked song that is the id it links from (normally the one asked for), not the id that
    /// plays: the lookup names the album of the release asked for (seen 2026-09: a song relinked
    /// to another release came back with the album of the release asked for, which holds it under
    /// the id asked for). Otherwise [`Self::uri`].
    pub listed_as: String,
    /// Its title (empty when the lookup gives none).
    pub name: String,
    /// Its artists (a track) or its show's publisher (an episode), for messages.
    pub by: Vec<String>,
    /// The track's album or the episode's show.
    pub context: Option<String>,
    /// Where the track is in its album, counting every disc from 0, when the lookup tells
    /// (disc 1); otherwise it is looked up when a start by uri is refused.
    pub position: Option<u32>,
    disc_number: u32,
    track_number: u32,
    /// The lookup's `is_playable` (in the account's market): `Some(false)` when Spotify cannot
    /// play the item here.
    playable: Option<bool>,
    /// The lookup's `restrictions.reason` when it gives one: `market`, `product` (not on the
    /// account's plan) or `explicit` (the account does not play explicit content).
    restriction: Option<String>,
    /// The Web API's item, to recognise it in Spotify.app (by id, `linked_from`, or title,
    /// length and album).
    item: Value,
}

impl ItemFacts {
    /// Builds facts from a Web API track or episode object for `requested`.
    #[must_use]
    pub fn from_web(requested: &SpotifyUri, value: &Value) -> Option<Self> {
        let kind = requested.kind;
        let id = value.get("id").and_then(Value::as_str)?;
        let linked_from = value
            .get("linked_from")
            .filter(|l| l.is_object())
            .and_then(|link| {
                link.get("uri")
                    .and_then(Value::as_str)
                    .filter(|uri| uri.starts_with("spotify:"))
                    .map(str::to_owned)
                    .or_else(|| {
                        link.get("id")
                            .and_then(Value::as_str)
                            .map(|id| format!("spotify:{}:{id}", kind.as_str()))
                    })
            });
        // Relinked, the `id` is the release that plays here.
        let uri = match (&linked_from, value.get("uri").and_then(Value::as_str)) {
            (None, Some(uri)) => uri.to_owned(),
            _ => format!("spotify:{}:{id}", kind.as_str()),
        };
        let listed_as = linked_from.unwrap_or_else(|| uri.clone());
        let (context, album_name) = match kind {
            Kind::Episode => (
                value.pointer("/show/uri").and_then(Value::as_str),
                value.pointer("/show/name").and_then(Value::as_str),
            ),
            _ => (
                value.pointer("/album/uri").and_then(Value::as_str),
                value.pointer("/album/name").and_then(Value::as_str),
            ),
        };
        let number = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(0)
        };
        let (disc_number, track_number) = (number("disc_number"), number("track_number"));
        let position =
            (kind == Kind::Track && disc_number == 1 && track_number > 0).then(|| track_number - 1);
        let mut item = json!({
            "id": id,
            "type": kind.as_str(),
            "name": value.get("name").cloned().unwrap_or(Value::Null),
            "duration_ms": value.get("duration_ms").cloned().unwrap_or(Value::Null),
            "album": {"name": album_name},
        });
        if let Some(linked) = value.get("linked_from").filter(|l| l.is_object()) {
            item["linked_from"] = linked.clone();
        }
        let by = match kind {
            Kind::Episode => value
                .pointer("/show/publisher")
                .and_then(Value::as_str)
                .map(|p| vec![p.to_owned()])
                .unwrap_or_default(),
            _ => value
                .get("artists")
                .and_then(Value::as_array)
                .map(|artists| {
                    artists
                        .iter()
                        .filter_map(|a| a.get("name").and_then(Value::as_str))
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        };
        Some(Self {
            requested: requested.uri(),
            uri,
            listed_as,
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_owned(),
            by,
            context: context.map(str::to_owned),
            position,
            disc_number,
            track_number,
            playable: value.get("is_playable").and_then(Value::as_bool),
            restriction: value
                .pointer("/restrictions/reason")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|reason| !reason.is_empty())
                .map(str::to_owned),
            item,
        })
    }

    /// The lookup's `is_playable` in the account's market (`None` when it does not say):
    /// `Some(false)` means no release of it plays there, and a start is [`not_playable`].
    #[must_use]
    pub const fn playable(&self) -> Option<bool> {
        self.playable
    }

    /// Whether `track` (what Spotify.app shows) is this item: the id asked for, the id it plays
    /// under, or the same song relinked (same title, a length within a second, same album).
    #[must_use]
    pub fn is(&self, track: &Track) -> bool {
        track.uri == self.requested
            || WebItem::from_player_json(&json!({"item": self.item}))
                .same_song(track)
                .is_some()
    }
}

/// Looked-up items by uri, for the life of the process.
static FACTS: Mutex<Option<HashMap<String, ItemFacts>>> = Mutex::new(None);
const FACTS_KEPT: usize = 500;

/// A track's or episode's facts (`GET /v1/tracks/{id}` or `/v1/episodes/{id}` in the account's
/// market), remembered for the life of the process.
///
/// # Errors
/// Classified Web API errors; `unsupported` for other kinds.
pub fn item_facts(web: &dyn WebApi, uri: &SpotifyUri) -> Result<ItemFacts> {
    let key = uri.uri();
    if let Some(found) = FACTS
        .lock()
        .ok()
        .and_then(|cache| cache.as_ref()?.get(&key).cloned())
    {
        return Ok(found);
    }
    let path = match uri.kind {
        Kind::Track => format!("/tracks/{}", uri.id),
        Kind::Episode => format!("/episodes/{}", uri.id),
        other => {
            return Err(Error::unsupported(
                format!("Only tracks and episodes are started by item, not {other:?}."),
                "Pass a track or episode uri.",
            ));
        }
    };
    let value = call(web, Method::Get, &path, &[("market", "from_token")], None)?;
    let facts = ItemFacts::from_web(uri, &value).ok_or_else(|| {
        Error::new(
            "web_api_failed",
            format!("The Spotify Web API's answer for {key} names no item."),
            "Retry; AppleScript handles playback meanwhile.",
        )
    })?;
    // An item not playable now may become playable: asked again next time.
    if facts.playable != Some(false)
        && let Ok(mut cache) = FACTS.lock()
    {
        let cache = cache.get_or_insert_with(HashMap::new);
        if cache.len() >= FACTS_KEPT {
            cache.clear();
        }
        cache.insert(key, facts.clone());
    }
    Ok(facts)
}

/// What an earlier [`item_facts`] lookup in this process found for `uri`, without asking the Web
/// API: the album an AppleScript fallback plays the track in, the facts to recognise a start by.
#[must_use]
pub fn cached_facts(uri: &SpotifyUri) -> Option<ItemFacts> {
    FACTS
        .lock()
        .ok()
        .and_then(|cache| cache.as_ref()?.get(&uri.uri()).cloned())
}

/// Where `facts`' track is in its album across discs, from the album's track list (up to 200
/// tracks).
fn album_position(web: &dyn WebApi, facts: &ItemFacts) -> Result<Option<u32>> {
    if let Some(position) = facts.position {
        return Ok(Some(position));
    }
    let Some(album) = facts
        .context
        .as_deref()
        .and_then(|c| c.strip_prefix("spotify:album:"))
    else {
        return Ok(None);
    };
    let wanted = [facts.uri.as_str(), facts.requested.as_str()];
    let mut index = 0u32;
    for page in 0..4u32 {
        let offset = (page * 50).to_string();
        let value = call(
            web,
            Method::Get,
            &format!("/albums/{album}/tracks"),
            &[
                ("market", "from_token"),
                ("limit", "50"),
                ("offset", &offset),
            ],
            None,
        )?;
        let items = value
            .get("items")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        for item in &items {
            let uri = item.get("uri").and_then(Value::as_str).unwrap_or_default();
            let linked = item
                .pointer("/linked_from/uri")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let numbers = (
                item.get("disc_number").and_then(Value::as_u64),
                item.get("track_number").and_then(Value::as_u64),
            );
            if wanted.contains(&uri)
                || wanted.contains(&linked)
                || numbers
                    == (
                        Some(u64::from(facts.disc_number)),
                        Some(u64::from(facts.track_number)),
                    )
            {
                return Ok(Some(index));
            }
            index += 1;
        }
        if items.len() < 50 || value.get("next").is_none_or(Value::is_null) {
            break;
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// Devices

/// spotify_player's default device name; it never plays (the daemon disables its streaming).
const SPOTIFY_PLAYER_DEVICE: &str = "spotify-player";

/// This Mac's name as Spotify.app shows it (`scutil --get ComputerName`), once per process.
fn computer_name() -> Option<String> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        if !cfg!(target_os = "macos") {
            return None;
        }
        let output = std::process::Command::new("/usr/sbin/scutil")
            .args(["--get", "ComputerName"])
            .stdin(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .output()
            .ok()?;
        let name = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        (output.status.success() && !name.is_empty()).then_some(name)
    })
    .clone()
}

/// Equal ignoring case and the kind of apostrophe (`Sam’s MacBook` and `Sam's MacBook`).
fn same_name(a: &str, b: &str) -> bool {
    let norm = |s: &str| {
        s.trim()
            .replace(['\u{2019}', '\u{2018}'], "'")
            .to_lowercase()
    };
    norm(a) == norm(b)
}

/// Whether the `GET /v1/me/player/devices` entry `raw` takes no Web API commands.
fn restricted(raw: &Value) -> bool {
    raw.get("is_restricted")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

/// Picks Spotify.app on this Mac from a `GET /v1/me/player/devices` answer: the one `Computer`
/// device named like this Mac. Only when this Mac's name is unknown, the only `Computer` device:
/// with the name known, a lone computer named otherwise is another one (a second Mac, the Web
/// Player in a browser) while Spotify.app here has not registered (just launched, offline), and a
/// start sent there would play on it. Never spotify_player's own device.
///
/// # Errors
/// `device_not_found` naming the devices seen.
pub fn pick_device(devices: &Value, computer_name: Option<&str>) -> Result<Device> {
    let listed: Vec<(Device, &Value)> = devices
        .get("devices")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|raw| Device::from_json(raw).map(|device| (device, raw)))
                .collect()
        })
        .unwrap_or_default();
    let computers: Vec<&Device> = listed
        .iter()
        .filter(|(d, raw)| {
            d.kind.eq_ignore_ascii_case("computer")
                && !d.id.is_empty()
                && !same_name(&d.name, SPOTIFY_PLAYER_DEVICE)
                && !restricted(raw)
        })
        .map(|(d, _)| d)
        .collect();
    let chosen = match computer_name {
        Some(name) => {
            let mut matching = computers.iter().filter(|d| same_name(&d.name, name));
            match (matching.next(), matching.next()) {
                (Some(one), None) => Some(*one),
                _ => None,
            }
        }
        None => match computers.as_slice() {
            [only] => Some(*only),
            _ => None,
        },
    };
    chosen.cloned().ok_or_else(|| {
        Error::new(
            "device_not_found",
            format!(
                "The Spotify Web API lists no device that is clearly Spotify.app on this Mac{}.",
                computer_name.map_or_else(String::new, |name| format!(" (\"{name}\")"))
            ),
            "Open Spotify.app and play anything once so it registers with Spotify; AppleScript handles playback meanwhile.",
        )
        .with_details(json!({
            "devices": listed.iter().map(|(d, _)| json!({"name": d.name, "type": d.kind})).collect::<Vec<_>>(),
        }))
    })
}

/// Where a start goes.
#[derive(Clone, Debug, PartialEq)]
pub struct Chosen {
    /// The device the start is sent to. Its id is empty for an active device the Web API lists
    /// without one; the start then names no device and Spotify plays it on the active one.
    pub device: Device,
    /// Another device is active and Spotify.app on this Mac is its remote (a speaker, a phone,
    /// a TV): the start plays there, as Spotify.app's own play button would, instead of moving
    /// playback to this Mac.
    pub remote: bool,
}

/// Picks where a start goes from a `GET /v1/me/player/devices` answer: the active device when it
/// is another one than Spotify.app on this Mac (never spotify_player's), so playback stays on the
/// speaker or phone Spotify.app controls; when nothing else is active, Spotify.app on this Mac
/// ([`pick_device`]). Spotify.app shows what plays on the device it controls, so a start there
/// is verified in Spotify.app all the same.
///
/// `app_playing` is whether Spotify.app on this Mac plays right now. When it does and no listed
/// device is active, it plays on a device the Web API does not list (Spotify leaves some device
/// models out of that list): the start then names no device, so Spotify plays it on the active
/// one instead of moving playback to this Mac. Should nothing be active after all, Spotify
/// answers `no_active_device` and the start is sent again to this Mac by id.
///
/// # Errors
/// `device_restricted` when the active device takes no Web API commands (AppleScript starts it
/// through Spotify.app instead); `device_not_found` when nothing else is active and Spotify.app
/// on this Mac is not listed.
pub fn choose_device(
    devices: &Value,
    computer_name: Option<&str>,
    app_playing: bool,
) -> Result<Chosen> {
    let mac = pick_device(devices, computer_name);
    let listed: Vec<(Device, &Value)> = devices
        .get("devices")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|raw| Device::from_json(raw).map(|device| (device, raw)))
        .collect();
    // spotify_player's device counts here: when it is the active one, a start naming no device
    // would play there.
    let any_active = listed.iter().any(|(device, _)| device.is_active);
    let active = listed.into_iter().find(|(device, _)| {
        device.is_active
            && !same_name(&device.name, SPOTIFY_PLAYER_DEVICE)
            && !mac
                .as_ref()
                .is_ok_and(|mac| !device.id.is_empty() && mac.id == device.id)
    });
    if !any_active && app_playing {
        return Ok(Chosen {
            device: Device::default(),
            remote: false,
        });
    }
    match active {
        Some((device, raw)) if restricted(raw) => Err(Error::new(
            "device_restricted",
            format!(
                "Spotify plays on \"{}\" ({}), which takes no commands through the Spotify Web API.",
                device.name, device.kind
            ),
            "Nothing to do: AppleScript starts it through Spotify.app, which controls that device.",
        )
        .with_details(json!({"device": {"name": device.name, "type": device.kind}}))),
        Some((device, _)) => Ok(Chosen {
            device,
            remote: true,
        }),
        None => mac.map(|device| Chosen {
            device,
            remote: false,
        }),
    }
}

/// Where a start goes now ([`choose_device`] over `GET /v1/me/player/devices`, with this Mac's
/// name and whether Spotify.app plays). The devices are listed at every start: the active one
/// changes whenever the user picks a speaker in Spotify.app.
///
/// # Errors
/// Classified Web API errors, `device_restricted`, `device_not_found`.
pub fn target(web: &dyn WebApi, app_playing: bool) -> Result<Chosen> {
    let devices = call(web, Method::Get, "/me/player/devices", &[], None)?;
    choose_device(&devices, computer_name().as_deref(), app_playing)
}

/// The `device_id` query of a request to `device` (none for an active device listed without an
/// id: Spotify then acts on the active device).
fn device_query(device: &Device) -> Vec<(&str, &str)> {
    if device.id.is_empty() {
        Vec::new()
    } else {
        vec![("device_id", device.id.as_str())]
    }
}

// ---------------------------------------------------------------------------------------------
// Starting

/// How a start named the item in its list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Offset {
    /// By uri.
    Uri(String),
    /// By index (an album's tracks across discs, Liked Songs' songs).
    Position(u32),
}

/// A start the Web API accepted.
#[derive(Clone, Debug, PartialEq)]
pub struct Started {
    /// The device it was sent to.
    pub device: Device,
    /// It went to another device Spotify.app controls ([`Chosen::remote`]).
    pub remote: bool,
    /// The list it plays in.
    pub context: String,
    /// How the item was named in it.
    pub offset: Offset,
    /// The item.
    pub item: ItemFacts,
    /// The context was looked up (the item's album or show), not given by the caller.
    pub own_context: bool,
}

impl Started {
    /// The start again by the item's position in its album, when that is another way to name
    /// it: after a start by uri that landed on another track. `None` when there is no such way.
    ///
    /// # Errors
    /// Classified Web API errors; one whose effect is unknown is [`unanswered`].
    pub fn by_position(&self, web: &dyn WebApi) -> Result<Option<Self>> {
        if !self.own_context || !matches!(self.offset, Offset::Uri(_)) {
            return Ok(None);
        }
        let Some(position) = album_position(web, &self.item)? else {
            return Ok(None);
        };
        let offset = Offset::Position(position);
        put_play(web, &self.device, &play_body(&self.context, Some(&offset)))?;
        Ok(Some(Self {
            offset,
            ..self.clone()
        }))
    }
}

/// The body of `PUT /v1/me/player/play` for `context`: from `offset`'s item at its beginning, or
/// (without one) wherever Spotify starts that list.
fn play_body(context: &str, offset: Option<&Offset>) -> Value {
    match offset {
        Some(Offset::Uri(uri)) => {
            json!({"context_uri": context, "offset": {"uri": uri}, "position_ms": 0})
        }
        Some(Offset::Position(position)) => {
            json!({"context_uri": context, "offset": {"position": position}, "position_ms": 0})
        }
        None => json!({"context_uri": context}),
    }
}

/// `PUT /v1/me/player/play` to `device`. An error whose effect is unknown is marked
/// [`unanswered`].
fn put_play(web: &dyn WebApi, device: &Device, body: &Value) -> Result<()> {
    call(
        web,
        Method::Put,
        "/me/player/play",
        &device_query(device),
        Some(body),
    )
    .map(drop)
    .map_err(mark_unanswered)
}

/// Starts `uri` (a track or episode) on [`choose_device`]'s device (Spotify.app on this Mac, or
/// the speaker it controls; `app_playing`: whether Spotify.app plays now): in `context` when
/// given, else in the track's album or the episode's show, from its beginning.
///
/// In its own album a track is named as the album lists it ([`ItemFacts::listed_as`]: a relinked
/// song by the id it links from), so one request starts it. A start by uri that the Web API
/// refuses is retried once by the track's position in the album (as is one that lands on another
/// track: [`Started::by_position`]). A device that the Web API no longer knows by the time the
/// start arrives (or no active device, for a start that named none) is looked up again once, and
/// the start then goes to this Mac unless another device is active.
///
/// # Errors
/// [`not_playable`] when Spotify lists the item as not playable in the account's market (nothing
/// is sent). Classified Web API errors: `rate_limited` (also while a rate limit's pause runs),
/// `premium_required`, `device_not_found`, `device_restricted`, `no_active_device`, `not_found`,
/// `web_api_failed`, `transport`. One whose effect is unknown (5xx, no answer) is [`unanswered`]:
/// the start may have gone through.
pub fn start(
    web: &dyn WebApi,
    uri: &SpotifyUri,
    context: Option<&SpotifyUri>,
    app_playing: bool,
) -> Result<Started> {
    if !matches!(uri.kind, Kind::Track | Kind::Episode) {
        return Err(Error::unsupported(
            format!("{} is not a track or episode.", uri.uri()),
            "Pass a track or episode uri.",
        ));
    }
    let item = item_facts(web, uri)?;
    // No release of it plays here: Spotify.app would skip it (or empty itself, for AppleScript's
    // start). Nothing is sent, and nothing else should try.
    if item.playable == Some(false) {
        return Err(not_playable(&item));
    }
    let (context, own_context, offset) = match context {
        // The caller's list holds the item under the id it was given.
        Some(context) => (context.uri(), false, Offset::Uri(item.requested.clone())),
        None => {
            let Some(context) = item.context.clone() else {
                return Err(Error::new(
                    "web_api_failed",
                    format!(
                        "The Spotify Web API names no {} for {}.",
                        if uri.kind == Kind::Episode {
                            "show"
                        } else {
                            "album"
                        },
                        item.requested
                    ),
                    "AppleScript plays it instead.",
                ));
            };
            // Named as its album lists it (for a relinked song, the id it links from), so the
            // one start lands on it; by position only when Spotify refuses or ignores that.
            (context, true, Offset::Uri(item.listed_as.clone()))
        }
    };
    let chosen = target(web, app_playing)?;
    let mut started = Started {
        device: chosen.device,
        remote: chosen.remote,
        context,
        offset,
        item,
        own_context,
    };
    let mut retried_device = false;
    loop {
        let body = play_body(&started.context, Some(&started.offset));
        match put_play(web, &started.device, &body) {
            Ok(()) => return Ok(started),
            // The device went away between the listing and the start (Spotify.app restarted),
            // or a start that named none found nothing active: this Mac, unless another plays.
            Err(error) if error.code == "no_active_device" && !retried_device => {
                retried_device = true;
                let chosen = target(web, false)?;
                started.device = chosen.device;
                started.remote = chosen.remote;
            }
            // The list does not hold the item under that uri: name it by position instead.
            Err(error)
                if matches!(error.code.as_str(), "not_found" | "web_api_failed")
                    && matches!(
                        error.details.as_ref().and_then(|d| d.get("status")),
                        Some(status) if status == 400 || status == 404
                    ) =>
            {
                return match started.by_position(web)? {
                    Some(again) => Ok(again),
                    None => Err(error),
                };
            }
            Err(error) => return Err(error),
        }
    }
}

/// `not_playable`: Spotify lists `item` as not playable for the account (`is_playable: false`):
/// in its market, with no release of it that plays there (a relinked song is playable), or, when
/// the lookup's `restrictions.reason` says so, not on the account's plan (`product`) or explicit
/// while the account does not play explicit content (`explicit`); `details.reason` gives that
/// reason. Nothing can start it: Spotify.app skips it, and AppleScript's `play track` of it
/// leaves Spotify.app empty. Not retryable; the hint searches for another version (a clean one,
/// for `explicit`).
#[must_use]
pub fn not_playable(item: &ItemFacts) -> Error {
    let kind = if item.requested.starts_with("spotify:episode:") {
        "episode"
    } else {
        "track"
    };
    let title = match (item.name.is_empty(), item.by.is_empty()) {
        (true, _) => item.requested.clone(),
        (false, true) => format!("'{}' ({})", item.name, item.requested),
        (false, false) => format!(
            "'{}' by {} ({})",
            item.name,
            item.by.join(", "),
            item.requested
        ),
    };
    // Words for the search, without quotes that would end the shell string.
    let words: String = std::iter::once(item.name.as_str())
        .chain(item.by.first().map(String::as_str))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .map(|c| {
            if matches!(c, '\'' | '"' | '`' | '\\') {
                ' '
            } else {
                c
            }
        })
        .collect();
    let words = words.split_whitespace().collect::<Vec<_>>().join(" ");
    let search = if words.is_empty() {
        format!("`spotify search '<title>' --type {kind}`")
    } else {
        format!("`spotify search '{words}' --type {kind}`")
    };
    let another = format!("Find another version with {search}, then `spotify play <uri>`.");
    // Spotify's reason when it names one other than the market: the account's settings or plan.
    let (why, hint) = match item.restriction.as_deref() {
        Some("explicit") => (
            "is explicit, and this Spotify account is set not to play explicit content".to_owned(),
            format!(
                "Allow explicit content in Spotify's settings (on a Family plan its manager sets it), or find a clean version with {search}, then `spotify play <uri>`."
            ),
        ),
        Some("product") => (
            "is not available on this account's Spotify plan".to_owned(),
            another,
        ),
        Some(reason) if reason != "market" => (
            format!("is not playable for this account (Spotify's restriction: {reason})"),
            another,
        ),
        _ => (
            "is not playable in your country/market: Spotify has no release of it that plays here"
                .to_owned(),
            another,
        ),
    };
    let mut details = json!({"uri": item.requested, "name": item.name, "market": "from_token"});
    if let Some(reason) = &item.restriction {
        details["reason"] = json!(reason);
    }
    Error::new(
        "not_playable",
        format!("{title} {why}, so nothing was started."),
        hint,
    )
    .with_details(details)
}

/// A list start (Liked Songs, a show) the Web API accepted.
#[derive(Clone, Debug, PartialEq)]
pub struct ListStarted {
    /// The device it was sent to.
    pub device: Device,
    /// It went to another device Spotify.app controls ([`Chosen::remote`]).
    pub remote: bool,
    /// The list.
    pub context: String,
    /// Where in the list it started (`None`: where Spotify starts it).
    pub position: Option<u32>,
}

/// Starts the list `context` (Liked Songs as `spotify:user:<id>:collection`, a show as
/// `spotify:show:<id>`) on [`choose_device`]'s device (`app_playing`: whether Spotify.app plays
/// now): at `position` from its beginning when given, else where Spotify starts it.
///
/// Spotify keeps a shuffle setting per list and switches to the list's own when it starts (seen
/// on Spotify.app in 2026-09: Liked Songs started shuffled right after shuffle was turned off for
/// the album playing before, and that album kept the shuffle set for it). So shuffle is set once
/// the list plays ([`set_shuffle`]), never before the start: that would only change the list
/// playing before. With shuffle on, `position` counts in the list's shuffled order.
///
/// # Errors
/// As [`start`]; one whose effect is unknown is [`unanswered`].
pub fn start_list(
    web: &dyn WebApi,
    context: &str,
    position: Option<u32>,
    app_playing: bool,
) -> Result<ListStarted> {
    let mut chosen = target(web, app_playing)?;
    let offset = position.map(Offset::Position);
    let body = play_body(context, offset.as_ref());
    let mut retried_device = false;
    loop {
        match put_play(web, &chosen.device, &body) {
            Ok(()) => {
                return Ok(ListStarted {
                    device: chosen.device,
                    remote: chosen.remote,
                    context: context.to_owned(),
                    position,
                });
            }
            Err(error) if error.code == "no_active_device" && !retried_device => {
                retried_device = true;
                chosen = target(web, false)?;
            }
            Err(error) => return Err(error),
        }
    }
}

/// Turns shuffle on or off for what plays on `device` (`PUT /v1/me/player/shuffle`).
///
/// # Errors
/// Classified Web API errors.
pub fn set_shuffle(web: &dyn WebApi, device: &Device, on: bool) -> Result<()> {
    let state = if on { "true" } else { "false" };
    let mut query = vec![("state", state)];
    query.extend(device_query(device));
    call(web, Method::Put, "/me/player/shuffle", &query, None).map(drop)
}

/// The size of Liked Songs and its first song.
#[derive(Clone, Debug, PartialEq)]
pub struct LikedSongs {
    /// How many songs it holds.
    pub total: u64,
    /// Its first song (the one liked last), to recognise it in Spotify.app; `None` when it is
    /// empty or Spotify cannot play that song in the account's market.
    pub first: Option<ItemFacts>,
}

/// Liked Songs' size and first song (`GET /v1/me/tracks?limit=1`, in the account's market).
///
/// # Errors
/// Classified Web API errors.
pub fn liked_songs(web: &dyn WebApi) -> Result<LikedSongs> {
    let value = call(
        web,
        Method::Get,
        "/me/tracks",
        &[("limit", "1"), ("market", "from_token")],
        None,
    )?;
    let first = value
        .pointer("/items/0/track")
        .filter(|track| track.is_object())
        .and_then(|track| {
            let uri = track
                .pointer("/linked_from/uri")
                .or_else(|| track.get("uri"))
                .and_then(Value::as_str)?;
            let requested = SpotifyUri::parse(uri, None).ok()?;
            ItemFacts::from_web(&requested, track)
        })
        // Spotify.app skips a song it cannot play here: no first song to expect then.
        .filter(|first| first.playable != Some(false));
    // An answer without its size is not an empty Liked Songs.
    let total = value.get("total").and_then(Value::as_u64).ok_or_else(|| {
        Error::new(
            "web_api_failed",
            "The Spotify Web API's answer for Liked Songs does not say how many songs it holds.",
            "Retry; AppleScript handles playback meanwhile.",
        )
        .retryable()
    })?;
    Ok(LikedSongs { total, first })
}

// ---------------------------------------------------------------------------------------------
// Search

/// The Web API's search (`GET /v1/search`) for `types` (`track`, `album`, `artist`,
/// `playlist`, `show`, `episode`), at most `limit` per type: the raw answer, one paging object
/// per type (`tracks.items`, …; Spotify leaves `null` holes in some lists). It answers when
/// `spotify_player search` cannot (its own parser refuses some answers). No `market`, as
/// spotify_player asks: `market=from_token` is refused for its token ("Insufficient client
/// scope", seen 2026-09) while the same search without it answers.
///
/// # Errors
/// Classified Web API errors (`rate_limited` also while a rate limit's pause runs).
pub fn search(web: &dyn WebApi, query: &str, types: &[&str], limit: usize) -> Result<Value> {
    let types = types.join(",");
    let limit = limit.to_string();
    call(
        web,
        Method::Get,
        "/search",
        &[
            ("q", query),
            ("type", types.as_str()),
            ("limit", limit.as_str()),
        ],
        None,
    )
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::time::Instant;

    /// A scripted Web API: answers by `METHOD path` prefix, records every request, keeps its
    /// own rate-limit pause (the process-wide one stays untouched).
    #[derive(Default)]
    pub(crate) struct FakeWeb {
        pub(crate) answers: Mutex<Vec<(String, Reply)>>,
        pub(crate) requests: Mutex<Vec<(String, Value)>>,
        pub(crate) paused_until: Mutex<Option<Instant>>,
    }

    pub(crate) fn ok(body: Value) -> Reply {
        Reply {
            status: 200,
            body,
            retry_after: None,
        }
    }

    pub(crate) fn status(status: u16, reason: &str, message: &str) -> Reply {
        Reply {
            status,
            body: json!({"error": {"status": status, "message": message, "reason": reason}}),
            retry_after: (status == 429).then(|| "7".to_owned()),
        }
    }

    impl FakeWeb {
        /// Answers `request` (a `METHOD path` prefix) with `reply`, once; the last answer for a
        /// request is kept for every later one.
        pub(crate) fn on(self, request: &str, reply: Reply) -> Self {
            self.answers
                .lock()
                .expect("lock")
                .push((request.to_owned(), reply));
            self
        }

        pub(crate) fn sent(&self) -> Vec<String> {
            self.requests
                .lock()
                .expect("lock")
                .iter()
                .map(|(r, _)| r.clone())
                .collect()
        }

        pub(crate) fn bodies(&self, prefix: &str) -> Vec<Value> {
            self.requests
                .lock()
                .expect("lock")
                .iter()
                .filter(|(r, _)| r.starts_with(prefix))
                .map(|(_, b)| b.clone())
                .collect()
        }
    }

    impl WebApi for FakeWeb {
        fn send(
            &self,
            method: Method,
            path: &str,
            query: &[(&str, &str)],
            body: Option<&Value>,
        ) -> Result<Reply> {
            let query = query
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>()
                .join("&");
            let request = format!("{} {path}?{query}", method.as_str());
            self.requests
                .lock()
                .expect("lock")
                .push((request.clone(), body.cloned().unwrap_or(Value::Null)));
            let mut answers = self.answers.lock().expect("lock");
            let matching: Vec<usize> = answers
                .iter()
                .enumerate()
                .filter(|(_, (prefix, _))| request.starts_with(prefix.as_str()))
                .map(|(i, _)| i)
                .collect();
            match matching.as_slice() {
                [] => Ok(status(500, "", "no fake answer")),
                [only] => Ok(answers[*only].1.clone()),
                [first, ..] => Ok(answers.remove(*first).1),
            }
        }

        fn pause_left(&self) -> Duration {
            self.paused_until
                .lock()
                .expect("lock")
                .map_or(Duration::ZERO, |until| {
                    until.saturating_duration_since(Instant::now())
                })
        }

        fn rate_limited(&self, retry_after: Option<&str>) {
            *self.paused_until.lock().expect("lock") =
                Some(Instant::now() + rate_limit_pause(retry_after));
        }
    }

    pub(crate) const MAC: &str = "mac-device";

    /// The devices answer: this Mac's Spotify.app (named as the Mac running the tests is, when
    /// it has a name), an idle phone and spotify_player's device.
    pub(crate) fn devices() -> Value {
        devices_with(None)
    }

    /// [`devices`] with the device named `active` (`phone`, `sp`, `speaker`, [`MAC`]) active; a
    /// kitchen speaker is listed too.
    pub(crate) fn devices_with(active: Option<&str>) -> Value {
        let mac = computer_name().unwrap_or_else(|| "Sam’s MacBook Pro".to_owned());
        let on = |id: &str| active == Some(id);
        json!({"devices": [
            {"id": "phone", "name": "Pixel", "type": "Smartphone", "is_active": on("phone")},
            {"id": MAC, "name": mac, "type": "Computer", "is_active": on(MAC)},
            {"id": "sp", "name": "spotify-player", "type": "Speaker", "is_active": on("sp")},
            {"id": "speaker", "name": "Kitchen", "type": "Speaker", "is_active": on("speaker")},
        ]})
    }

    pub(crate) fn track_json(id: &str, album: &str, disc: u32, number: u32) -> Value {
        json!({
            "id": id, "uri": format!("spotify:track:{id}"), "type": "track",
            "name": "Song", "duration_ms": 200_000,
            "disc_number": disc, "track_number": number,
            "album": {"uri": format!("spotify:album:{album}"), "name": "Album"},
        })
    }

    fn uri(text: &str) -> SpotifyUri {
        SpotifyUri::parse(text, None).expect("uri")
    }

    #[test]
    fn picks_spotify_app_on_this_mac_and_never_spotify_player() {
        let listed = json!({"devices": [
            {"id": "phone", "name": "Pixel", "type": "Smartphone", "is_active": true},
            {"id": MAC, "name": "Sam’s MacBook Pro", "type": "Computer", "is_active": false},
        ]});
        let device = pick_device(&listed, Some("Sam's MacBook Pro")).expect("found");
        assert_eq!(device.id, MAC);
        // This Mac's name unknown: the only computer.
        assert_eq!(pick_device(&listed, None).expect("found").id, MAC);
        // This Mac's name known and not listed: the lone computer is another one (Spotify.app
        // here has not registered yet), never started on.
        let elsewhere = json!({"devices": [
            {"id": "web", "name": "Web Player (Chrome)", "type": "Computer", "is_active": true},
        ]});
        let error = pick_device(&elsewhere, Some("Sam's MacBook Pro")).expect_err("not this Mac");
        assert_eq!(error.code, "device_not_found");
        // Two computers and neither is named like this Mac: none.
        let two = json!({"devices": [
            {"id": "a", "name": "Office iMac", "type": "Computer", "is_active": true},
            {"id": "b", "name": "Studio", "type": "Computer", "is_active": false},
        ]});
        let error = pick_device(&two, Some("Sam's MacBook Pro")).expect_err("ambiguous");
        assert_eq!(error.code, "device_not_found");
        // spotify_player's device, even if it called itself a computer.
        let player = json!({"devices": [
            {"id": "sp", "name": "spotify-player", "type": "Computer", "is_active": true},
        ]});
        assert!(pick_device(&player, None).is_err());
    }

    #[test]
    fn a_start_stays_on_the_active_device_spotify_app_controls() {
        let name = Some("Sam's MacBook Pro");
        let listed = |active: Option<&str>| {
            json!({"devices": [
                {"id": "phone", "name": "Pixel", "type": "Smartphone", "is_active": active == Some("phone")},
                {"id": MAC, "name": "Sam’s MacBook Pro", "type": "Computer", "is_active": active == Some(MAC)},
                {"id": "sp", "name": "spotify-player", "type": "Speaker", "is_active": active == Some("sp")},
                {"id": "tv", "name": "Living Room TV", "type": "TV", "is_active": active == Some("tv"), "is_restricted": true},
            ]})
        };
        // Spotify.app is the remote for the phone: the start plays there.
        let chosen = choose_device(&listed(Some("phone")), name, false).expect("chosen");
        assert_eq!((chosen.device.id.as_str(), chosen.remote), ("phone", true));
        // Nothing active, this Mac active, or only spotify_player's device active: this Mac.
        for active in [None, Some(MAC), Some("sp")] {
            let chosen = choose_device(&listed(active), name, false).expect("chosen");
            assert_eq!(
                (chosen.device.id.as_str(), chosen.remote),
                (MAC, false),
                "{active:?}"
            );
        }
        // A device that takes no Web API commands: AppleScript starts it through Spotify.app.
        let error = choose_device(&listed(Some("tv")), name, false).expect_err("restricted");
        assert_eq!(error.code, "device_restricted");
        // Spotify.app here not registered (yet): the active speaker is still reachable.
        let alone = json!({"devices": [
            {"id": "speaker", "name": "Kitchen", "type": "Speaker", "is_active": true},
        ]});
        let chosen = choose_device(&alone, name, false).expect("chosen");
        assert_eq!(
            (chosen.device.id.as_str(), chosen.remote),
            ("speaker", true)
        );
        // An active device listed without an id: the start names none (Spotify uses it).
        let no_id = json!({"devices": [
            {"id": null, "name": "Car", "type": "Automobile", "is_active": true},
            {"id": MAC, "name": "Sam’s MacBook Pro", "type": "Computer", "is_active": false},
        ]});
        let chosen = choose_device(&no_id, name, false).expect("chosen");
        assert!(chosen.remote && chosen.device.id.is_empty());
        assert!(device_query(&chosen.device).is_empty());
        // Nothing active and this Mac not listed: nowhere to start.
        let nowhere = json!({"devices": [
            {"id": "phone", "name": "Pixel", "type": "Smartphone", "is_active": false},
        ]});
        assert_eq!(
            choose_device(&nowhere, name, false).expect_err("none").code,
            "device_not_found"
        );
    }

    #[test]
    fn a_start_stays_on_a_device_the_web_api_does_not_list() {
        // As [`devices`] names this Mac.
        let mac = computer_name().unwrap_or_else(|| "Sam's MacBook Pro".to_owned());
        let name = Some(mac.as_str());
        let idle = devices();
        // Spotify.app plays, yet nothing listed is active: it plays on an unlisted device, and
        // the start names none so it plays there rather than moving to this Mac.
        let chosen = choose_device(&idle, name, true).expect("chosen");
        assert!(chosen.device.id.is_empty() && !chosen.remote, "{chosen:?}");
        // Spotify.app paused: this Mac, by id.
        let chosen = choose_device(&idle, name, false).expect("chosen");
        assert_eq!(chosen.device.id, MAC);
        // spotify_player's device plays (Spotify.app shows it playing): never started there,
        // not even by naming no device.
        let player = devices_with(Some("sp"));
        let chosen = choose_device(&player, name, true).expect("chosen");
        assert_eq!((chosen.device.id.as_str(), chosen.remote), (MAC, false));
        // Playing on this Mac or a listed speaker: named as before.
        let listed = devices_with(Some("speaker"));
        assert_eq!(
            choose_device(&listed, None, true)
                .expect("chosen")
                .device
                .id,
            "speaker"
        );
        // Spotify said no device is active after all: the start goes to this Mac by id.
        let id = "2400000000000000000000";
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(track_json(id, "ALB24", 1, 1)))
            .on("GET /me/player/devices", ok(devices()))
            .on(
                "PUT /me/player/play",
                status(
                    404,
                    "NO_ACTIVE_DEVICE",
                    "Player command failed: No active device found",
                ),
            )
            .on("PUT /me/player/play", ok(Value::Null));
        let started =
            start(&web, &uri(&format!("spotify:track:{id}")), None, true).expect("started");
        assert_eq!(started.device.id, MAC);
        let plays: Vec<String> = web
            .sent()
            .into_iter()
            .filter(|r| r.starts_with("PUT /me/player/play"))
            .collect();
        assert_eq!(
            plays,
            [
                "PUT /me/player/play?".to_owned(),
                format!("PUT /me/player/play?device_id={MAC}")
            ]
        );
    }

    #[test]
    fn a_track_starts_on_the_speaker_spotify_app_controls() {
        let id = "2100000000000000000000";
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(track_json(id, "ALB21", 1, 2)))
            .on("GET /me/player/devices", ok(devices_with(Some("speaker"))))
            .on("PUT /me/player/play", ok(Value::Null));
        let started =
            start(&web, &uri(&format!("spotify:track:{id}")), None, false).expect("started");
        assert_eq!(started.device.id, "speaker");
        assert!(started.remote);
        assert!(
            web.sent()
                .iter()
                .any(|r| r.starts_with("PUT /me/player/play?device_id=speaker")),
            "{:?}",
            web.sent()
        );
        // The devices are listed again at the next start: the speaker was switched off.
        let web = FakeWeb::default()
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", ok(Value::Null));
        let started =
            start(&web, &uri(&format!("spotify:track:{id}")), None, false).expect("started");
        assert_eq!((started.device.id.as_str(), started.remote), (MAC, false));
    }

    #[test]
    fn a_start_answered_5xx_or_not_at_all_is_marked_unanswered() {
        let id = "2200000000000000000000";
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(track_json(id, "ALB22", 1, 1)))
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", status(502, "", "Bad gateway"));
        let error =
            start(&web, &uri(&format!("spotify:track:{id}")), None, false).expect_err("502");
        assert_eq!(error.code, "web_api_failed");
        assert!(unanswered(&error), "{error:?}");
        // Refusals and failed lookups are answers: nothing started.
        let web = FakeWeb::default()
            .on("GET /me/player/devices", ok(devices()))
            .on(
                "PUT /me/player/play",
                status(403, "PREMIUM_REQUIRED", "Premium required"),
            );
        let error =
            start(&web, &uri(&format!("spotify:track:{id}")), None, false).expect_err("403");
        assert!(!unanswered(&error));
        let lookup = FakeWeb::default().on("GET /tracks/", status(503, "", "Unavailable"));
        let error = start(
            &lookup,
            &uri("spotify:track:2300000000000000000000"),
            None,
            false,
        )
        .expect_err("503 on the lookup");
        assert!(!unanswered(&error), "no start was sent");
        // No answer: unknown, unless the connection never opened.
        let timeout = Error::new("transport", "timed out", "")
            .with_details(json!({"connect": false, "timeout": true}));
        assert!(unanswered(&mark_unanswered(timeout)));
        let refused = Error::new("transport", "connection refused", "")
            .with_details(json!({"connect": true, "timeout": false}));
        assert!(!unanswered(&mark_unanswered(refused)));
    }

    #[test]
    fn lists_start_on_the_chosen_device_and_shuffle_is_set_on_it() {
        let web = FakeWeb::default()
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/shuffle", ok(Value::Null))
            .on("PUT /me/player/play", ok(Value::Null));
        let liked = "spotify:user:user1:collection";
        let started = start_list(&web, liked, Some(41), false).expect("started");
        assert_eq!(
            (started.device.id.as_str(), started.position),
            (MAC, Some(41))
        );
        set_shuffle(&web, &started.device, true).expect("shuffled");
        assert_eq!(
            web.sent(),
            [
                "GET /me/player/devices?".to_owned(),
                format!("PUT /me/player/play?device_id={MAC}"),
                format!("PUT /me/player/shuffle?state=true&device_id={MAC}"),
            ]
        );
        assert_eq!(
            web.bodies("PUT /me/player/play"),
            [json!({"context_uri": liked, "offset": {"position": 41}, "position_ms": 0})]
        );
        // A show: just the list, where Spotify starts it.
        let web = FakeWeb::default()
            .on("GET /me/player/devices", ok(devices_with(Some("phone"))))
            .on("PUT /me/player/play", ok(Value::Null));
        let started = start_list(&web, "spotify:show:SHOW", None, false).expect("started");
        assert!(started.remote);
        assert_eq!(
            web.bodies("PUT /me/player/play"),
            [json!({"context_uri": "spotify:show:SHOW"})]
        );
        assert!(!web.sent().iter().any(|r| r.contains("shuffle")));
    }

    #[test]
    fn liked_songs_count_and_first_song_come_from_one_request() {
        let mut first = track_json("4PxK4VkD4naS8TzBt5CUfx", "ALBL", 1, 1);
        first["linked_from"] = json!({"id": "5PxK4VkD4naS8TzBt5CUfx", "uri": "spotify:track:5PxK4VkD4naS8TzBt5CUfx", "type": "track"});
        let web = FakeWeb::default().on(
            "GET /me/tracks",
            ok(json!({"total": 527, "items": [{"added_at": "2026-09-01T00:00:00Z", "track": first}]})),
        );
        let liked = liked_songs(&web).expect("liked");
        assert_eq!(liked.total, 527);
        let first = liked.first.expect("first");
        // Saved under the id it links from.
        assert_eq!(first.requested, "spotify:track:5PxK4VkD4naS8TzBt5CUfx");
        assert_eq!(first.uri, "spotify:track:4PxK4VkD4naS8TzBt5CUfx");
        assert_eq!(web.sent(), ["GET /me/tracks?limit=1&market=from_token"]);
        let empty = FakeWeb::default().on("GET /me/tracks", ok(json!({"total": 0, "items": []})));
        assert_eq!(
            liked_songs(&empty).expect("empty"),
            LikedSongs {
                total: 0,
                first: None
            }
        );
        // An answer that does not say is no empty Liked Songs.
        let blank = FakeWeb::default().on("GET /me/tracks", ok(Value::Null));
        assert_eq!(
            liked_songs(&blank).expect_err("no size").code,
            "web_api_failed"
        );
    }

    #[test]
    fn search_asks_for_the_wanted_types() {
        let web = FakeWeb::default().on("GET /search", ok(json!({"tracks": {"items": []}})));
        search(&web, "radiohead reckoner", &["track", "album"], 5).expect("answer");
        assert_eq!(
            web.sent(),
            ["GET /search?q=radiohead reckoner&type=track,album&limit=5"]
        );
        // Nothing is sent while a rate limit's pause runs.
        let limited = FakeWeb::default();
        limited.rate_limited(Some("30"));
        assert_eq!(
            search(&limited, "x", &["track"], 1)
                .expect_err("paused")
                .code,
            "rate_limited"
        );
        assert!(limited.sent().is_empty());
    }

    #[test]
    fn facts_name_the_album_position_and_relinked_release() {
        let requested = uri("spotify:track:AAAAAAAAAAAAAAAAAAAAAA");
        let mut value = track_json("BBBBBBBBBBBBBBBBBBBBBB", "ALB", 1, 3);
        value["linked_from"] = json!({"id": "AAAAAAAAAAAAAAAAAAAAAA", "uri": "spotify:track:AAAAAAAAAAAAAAAAAAAAAA", "type": "track"});
        let facts = ItemFacts::from_web(&requested, &value).expect("facts");
        assert_eq!(facts.uri, "spotify:track:BBBBBBBBBBBBBBBBBBBBBB");
        // Its album lists it under the id it links from.
        assert_eq!(facts.listed_as, "spotify:track:AAAAAAAAAAAAAAAAAAAAAA");
        assert_eq!(facts.context.as_deref(), Some("spotify:album:ALB"));
        assert_eq!(facts.position, Some(2));
        // Relinked, the id is the release that plays, whatever `uri` says; `linked_from` may
        // come without a uri.
        let mut odd = value.clone();
        odd["uri"] = json!("spotify:track:AAAAAAAAAAAAAAAAAAAAAA");
        odd["linked_from"] = json!({"id": "AAAAAAAAAAAAAAAAAAAAAA", "type": "track"});
        let facts_odd = ItemFacts::from_web(&requested, &odd).expect("facts");
        assert_eq!(facts_odd.uri, "spotify:track:BBBBBBBBBBBBBBBBBBBBBB");
        assert_eq!(facts_odd.listed_as, "spotify:track:AAAAAAAAAAAAAAAAAAAAAA");
        // Not relinked: listed as it plays.
        let plain = ItemFacts::from_web(
            &requested,
            &track_json("AAAAAAAAAAAAAAAAAAAAAA", "ALB", 1, 3),
        )
        .expect("facts");
        assert_eq!(plain.listed_as, plain.uri);
        assert_eq!(plain.playable(), None);
        // Spotify.app may show either id.
        let mut shown = Track {
            uri: "spotify:track:AAAAAAAAAAAAAAAAAAAAAA".into(),
            name: "Song".into(),
            album: "Album".into(),
            duration_ms: 200_400,
            ..Track::default()
        };
        shown.finish();
        assert!(facts.is(&shown));
        shown.uri = "spotify:track:BBBBBBBBBBBBBBBBBBBBBB".into();
        shown.finish();
        assert!(facts.is(&shown));
        // Same title and album, another length: another song.
        shown.uri = "spotify:track:CCCCCCCCCCCCCCCCCCCCCC".into();
        shown.duration_ms = 150_000;
        shown.finish();
        assert!(!facts.is(&shown));
        // Later discs are found in the album's list.
        let later = ItemFacts::from_web(&requested, &track_json("B", "ALB", 2, 1)).expect("facts");
        assert_eq!(later.position, None);
    }

    #[test]
    fn a_429_pauses_every_later_request() {
        let web = FakeWeb::default().on("GET /tracks/", status(429, "", "API rate limit exceeded"));
        let error = start(
            &web,
            &uri("spotify:track:0000000000000000000429"),
            None,
            false,
        )
        .expect_err("limited");
        assert_eq!(error.code, "rate_limited");
        assert!(web.pause_left() > Duration::from_secs(5));
        // Nothing is sent while the pause runs.
        let error = start(
            &web,
            &uri("spotify:track:1000000000000000000429"),
            None,
            false,
        )
        .expect_err("paused");
        assert_eq!(error.code, "rate_limited");
        assert_eq!(web.sent().len(), 1);
    }

    #[test]
    fn the_process_wide_pause_follows_retry_after() {
        assert_eq!(rate_limit_pause(Some("7")), Duration::from_secs(7));
        assert_eq!(rate_limit_pause(Some(" 12 ")), Duration::from_secs(12));
        assert_eq!(rate_limit_pause(None), Duration::from_secs(30));
        assert_eq!(
            rate_limit_pause(Some("Wed, 21 Oct 2026 07:28:00 GMT")),
            Duration::from_secs(30)
        );
        assert_eq!(rate_limit_pause(Some("86400")), Duration::from_secs(600));
        assert_eq!(rate_limit_pause(Some("0")), Duration::from_secs(1));
    }

    #[test]
    fn premium_and_unknown_answers_are_classified() {
        let web = FakeWeb::default();
        let premium = answer(
            &web,
            "PUT /me/player/play",
            status(
                403,
                "PREMIUM_REQUIRED",
                "Player command failed: Premium required",
            ),
        )
        .expect_err("refused");
        assert_eq!(premium.code, "premium_required");
        let device = answer(
            &web,
            "PUT /me/player/play",
            status(404, "", "Device not found"),
        )
        .expect_err("refused");
        assert_eq!(device.code, "no_active_device");
        let other = answer(&web, "PUT /me/player/play", status(502, "", "Bad gateway"))
            .expect_err("failed");
        assert_eq!(other.code, "web_api_failed");
        assert!(other.retryable);
        assert_eq!(
            other.details.as_ref().map(|d| d["status"].clone()),
            Some(json!(502))
        );
    }

    #[test]
    fn starts_a_track_in_its_album_on_this_mac() {
        let id = "2000000000000000000000";
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(track_json(id, "ALB", 1, 4)))
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", ok(Value::Null));
        let started =
            start(&web, &uri(&format!("spotify:track:{id}")), None, false).expect("started");
        assert_eq!(started.context, "spotify:album:ALB");
        assert_eq!(started.device.id, MAC);
        assert_eq!(
            web.bodies("PUT"),
            [
                json!({"context_uri": "spotify:album:ALB", "offset": {"uri": format!("spotify:track:{id}")}, "position_ms": 0})
            ]
        );
        assert!(
            web.sent()
                .iter()
                .any(|r| r.contains("device_id=mac-device"))
        );
        // The lookup is remembered.
        let again = FakeWeb::default()
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", ok(Value::Null));
        start(&again, &uri(&format!("spotify:track:{id}")), None, false).expect("started");
        assert!(!again.sent().iter().any(|r| r.starts_with("GET /tracks/")));
    }

    #[test]
    fn a_refused_offset_is_retried_by_position_across_discs() {
        let id = "3000000000000000000000";
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(track_json(id, "ALB3", 2, 2)))
            .on("GET /me/player/devices", ok(devices()))
            .on(
                "GET /albums/ALB3/tracks",
                ok(json!({"items": [
                {"uri": "spotify:track:d1t1", "disc_number": 1, "track_number": 1},
                {"uri": "spotify:track:d1t2", "disc_number": 1, "track_number": 2},
                {"uri": "spotify:track:d2t1", "disc_number": 2, "track_number": 1},
                {"uri": "spotify:track:other", "disc_number": 2, "track_number": 2},
            ], "next": null})),
            )
            .on("PUT /me/player/play", status(404, "", "Not found."))
            .on("PUT /me/player/play", ok(Value::Null));
        let started =
            start(&web, &uri(&format!("spotify:track:{id}")), None, false).expect("started");
        assert_eq!(started.offset, Offset::Position(3));
        let bodies = web.bodies("PUT");
        assert_eq!(bodies.len(), 2);
        assert_eq!(bodies[1]["offset"], json!({"position": 3}));
    }

    #[test]
    fn an_item_not_playable_here_is_not_started() {
        let id = "3100000000000000000000";
        let mut lookup = track_json(id, "ALB31", 1, 2);
        lookup["is_playable"] = json!(false);
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(lookup))
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", ok(Value::Null));
        // Spotify.app would play the album's next track instead.
        let error =
            start(&web, &uri(&format!("spotify:track:{id}")), None, false).expect_err("refused");
        assert_eq!(error.code, "not_playable");
        assert_eq!(error.exit_code(), 1);
        assert!(!error.retryable);
        assert!(
            error
                .message
                .contains("not playable in your country/market"),
            "{}",
            error.message
        );
        assert!(web.bodies("PUT").is_empty());
        // Not even the devices: nothing will be sent.
        assert_eq!(web.sent(), [format!("GET /tracks/{id}?market=from_token")]);
        // Named by title and artists, and the hint searches for another version.
        let mut lookup = track_json("3400000000000000000000", "ALB34", 1, 1);
        lookup["name"] = json!("Fall (Acoustic Version)");
        lookup["artists"] = json!([{"name": "Ana Rey"}, {"name": "The Tides"}]);
        lookup["is_playable"] = json!(false);
        let web = FakeWeb::default().on("GET /tracks/", ok(lookup));
        let error = start(
            &web,
            &uri("spotify:track:3400000000000000000000"),
            None,
            false,
        )
        .expect_err("refused");
        assert_eq!(
            error.message,
            "'Fall (Acoustic Version)' by Ana Rey, The Tides (spotify:track:3400000000000000000000) is not playable in your country/market: Spotify has no release of it that plays here, so nothing was started."
        );
        assert_eq!(
            error.hint,
            "Find another version with `spotify search 'Fall (Acoustic Version) Ana Rey' --type track`, then `spotify play <uri>`."
        );
        assert_eq!(
            error.details,
            Some(
                json!({"uri": "spotify:track:3400000000000000000000", "name": "Fall (Acoustic Version)", "market": "from_token"})
            )
        );
        // Asked again next time: it may become playable.
        assert!(cached_facts(&uri("spotify:track:3400000000000000000000")).is_none());
        // A title with quotes stays one shell word; an unknown title is a placeholder.
        let mut item = ItemFacts::from_web(
            &uri("spotify:track:3100000000000000000001"),
            &track_json("3100000000000000000001", "ALB", 1, 1),
        )
        .expect("facts");
        item.name = "Don't 'Stop' Me".into();
        assert!(
            not_playable(&item)
                .hint
                .contains("`spotify search 'Don t Stop Me' --type track`"),
            "{}",
            not_playable(&item).hint
        );
        item.name.clear();
        assert!(not_playable(&item).hint.contains("'<title>'"));
        assert!(
            not_playable(&item)
                .message
                .starts_with("spotify:track:3100000000000000000001 is not playable")
        );
        // Spotify's restriction, when it names one, says why: only `market` is the country.
        let restricted = |reason: &str| {
            let mut lookup = track_json("3500000000000000000000", "ALB35", 1, 1);
            lookup["name"] = json!("Loud Song");
            lookup["is_playable"] = json!(false);
            lookup["restrictions"] = json!({"reason": reason});
            let web = FakeWeb::default().on("GET /tracks/", ok(lookup));
            let error = start(
                &web,
                &uri("spotify:track:3500000000000000000000"),
                None,
                false,
            )
            .expect_err("refused");
            assert_eq!(error.code, "not_playable");
            assert!(!error.retryable);
            assert_eq!(web.sent().len(), 1, "only the lookup: {:?}", web.sent());
            assert_eq!(error.details.as_ref().expect("details")["reason"], reason);
            error
        };
        let market = restricted("market");
        assert!(
            market
                .message
                .contains("not playable in your country/market")
        );
        let explicit = restricted("explicit");
        assert_eq!(
            explicit.message,
            "'Loud Song' (spotify:track:3500000000000000000000) is explicit, and this Spotify account is set not to play explicit content, so nothing was started."
        );
        assert!(
            explicit.hint.starts_with("Allow explicit content")
                && explicit
                    .hint
                    .contains("a clean version with `spotify search 'Loud Song' --type track`"),
            "{}",
            explicit.hint
        );
        let product = restricted("product");
        assert!(
            product
                .message
                .contains("not available on this account's Spotify plan")
                && !product.message.contains("country"),
            "{}",
            product.message
        );
        assert!(
            restricted("something_new")
                .message
                .contains("(Spotify's restriction: something_new)")
        );
    }

    #[test]
    fn a_relinked_song_starts_in_its_album_by_the_id_the_album_lists() {
        // Seen live (2026-09): `GET /v1/tracks/<asked>?market=from_token` of a relinked song
        // answers the release that plays here (its id, `linked_from` the id asked for) with the
        // album of the release asked for, which lists the song under the id asked for. Named by
        // the id that plays there, Spotify plays the album's first track instead; named by the
        // id asked for, the song.
        let requested = "3200000000000000000000";
        let plays = "3300000000000000000000";
        let mut lookup = track_json(plays, "ALB32", 1, 4);
        lookup["linked_from"] =
            json!({"id": requested, "type": "track", "uri": format!("spotify:track:{requested}")});
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(lookup))
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", ok(Value::Null));
        let started = start(
            &web,
            &uri(&format!("spotify:track:{requested}")),
            None,
            false,
        )
        .expect("started");
        assert_eq!(started.item.uri, format!("spotify:track:{plays}"));
        assert_eq!(started.item.listed_as, format!("spotify:track:{requested}"));
        assert_eq!(
            started.offset,
            Offset::Uri(format!("spotify:track:{requested}"))
        );
        assert_eq!(
            web.bodies("PUT"),
            [
                json!({"context_uri": "spotify:album:ALB32", "offset": {"uri": format!("spotify:track:{requested}")}, "position_ms": 0})
            ]
        );
        // In the caller's list it is named as asked, relinked or not.
        let playlist = uri("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M");
        let web = FakeWeb::default()
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", ok(Value::Null));
        let started = start(
            &web,
            &uri(&format!("spotify:track:{requested}")),
            Some(&playlist),
            false,
        )
        .expect("started");
        assert_eq!(
            started.offset,
            Offset::Uri(format!("spotify:track:{requested}"))
        );
    }

    #[test]
    fn a_callers_context_is_used_as_given() {
        let id = "4000000000000000000000";
        let web = FakeWeb::default()
            .on("GET /tracks/", ok(track_json(id, "ALB4", 1, 1)))
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", status(404, "", "Not found."));
        let playlist = uri("spotify:playlist:37i9dQZF1DXcBWIGoYBM5M");
        let error = start(
            &web,
            &uri(&format!("spotify:track:{id}")),
            Some(&playlist),
            false,
        )
        .expect_err("refused");
        // No position retry: the playlist's order is not the album's.
        assert_eq!(error.code, "not_found");
        assert_eq!(
            web.bodies("PUT"),
            [
                json!({"context_uri": playlist.uri(), "offset": {"uri": format!("spotify:track:{id}")}, "position_ms": 0})
            ]
        );
    }

    #[test]
    fn episodes_start_in_their_show() {
        let id = "5000000000000000000000";
        let web = FakeWeb::default()
            .on(
                "GET /episodes/",
                ok(
                    json!({"id": id, "type": "episode", "name": "Ep", "duration_ms": 1000,
                "show": {"uri": "spotify:show:SHOW", "name": "Show"}}),
                ),
            )
            .on("GET /me/player/devices", ok(devices()))
            .on("PUT /me/player/play", ok(Value::Null));
        let started =
            start(&web, &uri(&format!("spotify:episode:{id}")), None, false).expect("started");
        assert_eq!(started.context, "spotify:show:SHOW");
        assert_eq!(started.offset, Offset::Uri(format!("spotify:episode:{id}")));
        // Premium absent: refused at once.
        let web = FakeWeb::default()
            .on("GET /me/player/devices", ok(devices()))
            .on(
                "PUT /me/player/play",
                status(403, "PREMIUM_REQUIRED", "Premium required"),
            );
        let error =
            start(&web, &uri(&format!("spotify:episode:{id}")), None, false).expect_err("refused");
        assert_eq!(error.code, "premium_required");
        assert_eq!(web.bodies("PUT").len(), 1);
    }
}
