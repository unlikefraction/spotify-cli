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
