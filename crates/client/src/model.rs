//! Shapes printed by every surface. Field names are stable JSON contracts.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::timing::clock;

/// What Spotify.app is doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlayerState {
    /// Audio is playing.
    Playing,
    /// A track is loaded and paused.
    Paused,
    /// Nothing is loaded.
    Stopped,
    /// Spotify.app is not running.
    NotRunning,
}

impl PlayerState {
    /// Parses AppleScript's `player state as string`.
    #[must_use]
    pub fn from_applescript(value: &str) -> Self {
        match value.trim() {
            "playing" | "kPSP" => Self::Playing,
            "paused" | "kPSp" => Self::Paused,
            "not_running" => Self::NotRunning,
            _ => Self::Stopped,
        }
    }

    /// Lowercase name.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Playing => "playing",
            Self::Paused => "paused",
            Self::Stopped => "stopped",
            Self::NotRunning => "not_running",
        }
    }
}

/// The item Spotify.app has loaded.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Track {
    /// `spotify:track:<id>`, `spotify:episode:<id>`, `spotify:ad:<id>` or `spotify:local:…`.
    pub uri: String,
    /// The id part of the URI.
    pub id: String,
    /// `track`, `episode`, `ad` or `local`.
    pub kind: String,
    /// Title.
    pub name: String,
    /// Artist(s) as Spotify.app reports them (joined with ", " when several).
    pub artist: String,
    /// Album (or show, for episodes).
    pub album: String,
    /// Album artist.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub album_artist: String,
    /// Length in milliseconds.
    pub duration_ms: u64,
    /// Length as `m:ss`.
    pub duration: String,
    /// Position on the album.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub track_number: Option<u32>,
    /// Disc on the album.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disc_number: Option<u32>,
    /// Spotify popularity 0–100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub popularity: Option<u32>,
    /// Cover art.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub artwork_url: String,
    /// Share link.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub url: String,
}

impl Track {
    /// Fills derived fields (`id`, `kind`, `duration`, `url`) from `uri` and `duration_ms`.
    pub fn finish(&mut self) {
        let mut parts = self.uri.splitn(3, ':');
        let (_, kind, id) = (parts.next(), parts.next(), parts.next());
        self.kind = kind.unwrap_or_default().to_owned();
        self.id = id.unwrap_or_default().to_owned();
        self.duration = clock(self.duration_ms);
        if self.url.is_empty() && matches!(self.kind.as_str(), "track" | "episode") {
            self.url = format!("https://open.spotify.com/{}/{}", self.kind, self.id);
        }
    }

    /// `Name — Artist`.
    #[must_use]
    pub fn label(&self) -> String {
        if self.artist.is_empty() {
            self.name.clone()
        } else {
            format!("{} — {}", self.name, self.artist)
        }
    }
}

/// A snapshot of Spotify.app.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Playback {
    /// Player state.
    pub state: PlayerState,
    /// The loaded item, if any.
    pub track: Option<Track>,
    /// Position in milliseconds.
    pub position_ms: u64,
    /// Position as `m:ss`.
    pub position: String,
    /// Time left in milliseconds.
    pub remaining_ms: u64,
    /// Share of the track played, 0–1.
    pub progress: f64,
    /// Spotify.app volume 0–100.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    /// Shuffle on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffling: Option<bool>,
    /// Repeat on (AppleScript cannot tell context repeat from track repeat).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeating: Option<bool>,
    /// Whether Spotify allows shuffle in the current context (singles and some contexts do not).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle_allowed: Option<bool>,
    /// Whether Spotify allows repeat in the current context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_allowed: Option<bool>,
    /// Extra fields only the Spotify Web API knows (via spotify_player): context, device, repeat mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub web: Option<WebPlayback>,
    /// When this snapshot was taken (RFC 3339, UTC).
    pub observed_at: String,
}

impl Playback {
    /// A snapshot for a state with no track.
    #[must_use]
    pub fn empty(state: PlayerState) -> Self {
        Self {
            state,
            track: None,
            position_ms: 0,
            position: clock(0),
            remaining_ms: 0,
            progress: 0.0,
            volume: None,
            shuffling: None,
            repeating: None,
            shuffle_allowed: None,
            repeat_allowed: None,
            web: None,
            observed_at: now_rfc3339(),
        }
    }

    /// Recomputes `position`, `remaining_ms` and `progress`.
    pub fn finish(&mut self) {
        let duration = self.track.as_ref().map_or(0, |t| t.duration_ms);
        self.position = clock(self.position_ms);
        self.remaining_ms = duration.saturating_sub(self.position_ms);
        #[allow(clippy::cast_precision_loss)]
        {
            self.progress = if duration == 0 {
                0.0
            } else {
                ((self.position_ms as f64 / duration as f64) * 1000.0).round() / 1000.0
            }
            .clamp(0.0, 1.0);
        }
    }
}

/// Playback facts from the Spotify Web API (through spotify_player).
///
/// The running spotify_player instance answers from memory that refreshes only after commands it
/// ran itself, so these facts can describe an earlier moment. [`crate::control::Controller::status_full`]
/// compares them with Spotify.app and asks the Web API directly when they disagree; `source`
/// says which answer this is and `stale` marks one that still does not match Spotify.app.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebPlayback {
    /// The item the Web API reports (`spotify:track:<id>`, `spotify:episode:<id>`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item_uri: Option<String>,
    /// Whether the Web API reports playback as running.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub is_playing: Option<bool>,
    /// The list being played (`spotify:playlist:…`, `spotify:album:…`, …).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_uri: Option<String>,
    /// `playlist`, `album`, `artist`, `show`, `collection`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_type: Option<String>,
    /// `off`, `context` or `track`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_state: Option<String>,
    /// Shuffle according to the Web API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shuffle_state: Option<bool>,
    /// The Spotify Connect device playing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<Device>,
    /// `spotify_player` (the running instance's memory, consistent with Spotify.app when read) or
    /// `web_api` (a fresh Web API read, made because the instance's memory was out of date).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// These facts could not be matched to what Spotify.app plays now (the Web API reports a
    /// different item, or could not be asked), so they are out of date; repeat and shuffle that
    /// contradict Spotify.app are left out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub stale: bool,
    /// The Web API plays the song Spotify.app shows under another id (track relinking: Spotify
    /// substitutes the same recording from another release), so `item_uri` is not Spotify.app's
    /// `track.uri`. Recognised by the item's `linked_from`, or by the same title, a length
    /// within a second and the same album name. The facts are current, not `stale`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub relinked: bool,
}

impl WebPlayback {
    /// Extracts the fields from `spotify_player get key playback` JSON.
    #[must_use]
    pub fn from_player_json(value: &Value) -> Option<Self> {
        if value.is_null() {
            return None;
        }
        let context = value.get("context").filter(|c| !c.is_null());
        Some(Self {
            item_uri: playing_item_uri(value),
            is_playing: value.get("is_playing").and_then(Value::as_bool),
            context_uri: context
                .and_then(|c| c.get("uri"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            context_type: context
                .and_then(|c| c.get("type"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            repeat_state: value
                .get("repeat_state")
                .and_then(Value::as_str)
                .map(str::to_owned),
            shuffle_state: value.get("shuffle_state").and_then(Value::as_bool),
            device: value.get("device").and_then(Device::from_json),
            source: None,
            stale: false,
            relinked: false,
        })
    }
}

/// How the Web API's current item was recognised as the song Spotify.app plays.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SameSong {
    /// The same id.
    Id,
    /// A relinked copy whose `linked_from` names Spotify.app's id.
    LinkedFrom,
    /// Another id with the same title, a length within a second and the same album name.
    Metadata,
}

impl SameSong {
    /// The name error details use.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Id => "id",
            Self::LinkedFrom => "linked_from",
            Self::Metadata => "title_length_album",
        }
    }
}

/// Longest difference in length between Spotify.app's song and the Web API's item for them to
/// count as one recording under two ids.
const SAME_LENGTH_MS: u64 = 1000;

/// The Web API's current item in `spotify_player get key playback` JSON: enough to recognise the
/// song Spotify.app plays when the two name it with different ids.
///
/// When the release a song was saved or started from cannot play in the account's market,
/// Spotify plays the same recording from another release (track relinking). Spotify.app then
/// reports the id it was asked for with the substitute's title, album and length, while the Web
/// API reports the substitute's id; it names the requested id in `linked_from` only when asked
/// with a market, which spotify_player does not do. So the title, length and album name are what
/// connect the two. The album name is required: the same recording on another release (a single
/// and its album, a deluxe edition) has the same title, length and artist, and a view still on
/// that release after a switch in Spotify.app is behind, not relinked.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct WebItem {
    uri: Option<String>,
    linked_from: Option<String>,
    name: Option<String>,
    duration_ms: Option<u64>,
    album: Option<String>,
}

impl WebItem {
    /// Reads the item of a `get key playback` answer (empty when it has none).
    pub(crate) fn from_player_json(value: &Value) -> Self {
        let Some(item) = value.get("item").filter(|i| i.is_object()) else {
            return Self::default();
        };
        let text = |value: Option<&Value>| {
            value
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let linked_from = item
            .get("linked_from")
            .filter(|l| l.is_object())
            .and_then(|link| {
                text(link.get("uri"))
                    .filter(|uri| uri.starts_with("spotify:"))
                    .or_else(|| {
                        let kind = text(link.get("type")).unwrap_or_else(|| "track".into());
                        text(link.get("id")).map(|id| format!("spotify:{kind}:{id}"))
                    })
            });
        // The Web API's `duration_ms`, or spotify_player's own `{secs, nanos}`.
        let duration_ms = item.get("duration_ms").and_then(Value::as_u64).or_else(|| {
            let duration = item.get("duration")?;
            let secs = duration.get("secs")?.as_u64()?;
            let nanos = duration.get("nanos").and_then(Value::as_u64).unwrap_or(0);
            Some(secs * 1000 + nanos / 1_000_000)
        });
        Self {
            uri: playing_item_uri(value),
            linked_from,
            name: text(item.get("name")),
            duration_ms,
            album: text(item.get("album").and_then(|a| a.get("name"))),
        }
    }

    /// Whether this item is `track` (the song Spotify.app plays), and how that shows. Only songs
    /// are relinked: another episode, ad or local file under a different id is another item.
    pub(crate) fn same_song(&self, track: &Track) -> Option<SameSong> {
        let uri = self.uri.as_deref()?;
        if uri == track.uri {
            return Some(SameSong::Id);
        }
        if track.kind != "track" || !uri.starts_with("spotify:track:") {
            return None;
        }
        if self.linked_from.as_deref() == Some(track.uri.as_str()) {
            return Some(SameSong::LinkedFrom);
        }
        let name = self
            .name
            .as_deref()
            .is_some_and(|name| same_text(name, &track.name));
        let length = self.duration_ms.is_some_and(|ms| {
            track.duration_ms > 0 && ms.abs_diff(track.duration_ms) <= SAME_LENGTH_MS
        });
        // Spotify.app shows the substitute's album, so a relinked song has the same one.
        let album = self
            .album
            .as_deref()
            .is_some_and(|album| same_text(album, &track.album));
        (name && length && album).then_some(SameSong::Metadata)
    }
}

/// Equal ignoring case and surrounding space; empty never matches.
fn same_text(a: &str, b: &str) -> bool {
    let (a, b) = (a.trim(), b.trim());
    !a.is_empty() && a.to_lowercase() == b.to_lowercase()
}

/// The item of `spotify_player get key playback` JSON as a URI. The Web API's item objects carry
/// `type` and a bare `id` but no `uri`; local files have no id and give `None`.
#[must_use]
pub fn playing_item_uri(value: &Value) -> Option<String> {
    let item = value.get("item").filter(|i| !i.is_null())?;
    let id = item.get("id")?.as_str().filter(|id| !id.is_empty())?;
    let kind = item
        .get("type")
        .and_then(Value::as_str)
        .or_else(|| value.get("currently_playing_type").and_then(Value::as_str))
        .unwrap_or("track");
    Some(format!("spotify:{kind}:{id}"))
}

/// A Spotify Connect device.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Device {
    /// Device id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// `Computer`, `Smartphone`, `Speaker`, …
    #[serde(rename = "type")]
    pub kind: String,
    /// Currently the active device.
    pub is_active: bool,
    /// Volume 0–100 when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume_percent: Option<u8>,
}

impl Device {
    /// Parses one device object from spotify_player JSON.
    #[must_use]
    pub fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            id: value.get("id")?.as_str().unwrap_or_default().to_owned(),
            name: value.get("name")?.as_str().unwrap_or_default().to_owned(),
            kind: value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            is_active: value
                .get("is_active")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            volume_percent: value
                .get("volume_percent")
                .and_then(Value::as_u64)
                .and_then(|v| u8::try_from(v).ok()),
        })
    }
}

/// One search hit or library item, normalized across kinds.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Item {
    /// `track`, `album`, `artist`, `playlist`, `show` or `episode`.
    pub kind: String,
    /// Spotify id.
    pub id: String,
    /// `spotify:<kind>:<id>`; pass it to `spotify play`.
    pub uri: String,
    /// Title or name.
    pub name: String,
    /// Artists (tracks, albums) or owner (playlists).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub by: Vec<String>,
    /// Album (tracks) or show (episodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// The album's uri (tracks) or the show's (episodes), when known: `spotify track` it for its
    /// tracks, or `spotify play` it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_uri: Option<String>,
    /// Length in milliseconds (tracks, episodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Length as `m:ss`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// Release date (albums, episodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
    /// `album`, `single`, `compilation` or `appears_on` (albums).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album_type: Option<String>,
    /// Explicit content flag (tracks).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub explicit: Option<bool>,
    /// Description (playlists, episodes), truncated to 280 characters.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Item {
    /// Normalizes a spotify_player JSON object of the given kind.
    #[must_use]
    pub fn from_player_json(kind: &str, value: &Value) -> Option<Self> {
        let id = value.get("id")?.as_str()?.to_owned();
        let name = value
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let names = |key: &str| -> Vec<String> {
            value
                .get(key)
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|a| a.get("name").and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default()
        };
        let by = match kind {
            "playlist" => value
                .get("owner")
                .and_then(Value::as_array)
                .and_then(|owner| owner.first())
                .and_then(Value::as_str)
                .map(|owner| vec![owner.to_owned()])
                .unwrap_or_default(),
            _ => names("artists"),
        };
        let duration_ms = value.get("duration").and_then(|d| {
            let secs = d.get("secs")?.as_u64()?;
            let nanos = d.get("nanos").and_then(Value::as_u64).unwrap_or(0);
            Some(secs * 1000 + nanos / 1_000_000)
        });
        let album = match kind {
            "track" => value
                .get("album")
                .and_then(|a| a.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            "episode" => value
                .get("show")
                .and_then(|s| s.get("name"))
                .and_then(Value::as_str)
                .map(str::to_owned),
            _ => None,
        };
        let album_uri = container_uri(kind, value);
        let description = value
            .get("description")
            .or_else(|| value.get("desc"))
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(|d| truncate(d, 280));
        Some(Self {
            kind: kind.to_owned(),
            uri: format!("spotify:{kind}:{id}"),
            id,
            name,
            by,
            album,
            album_uri,
            duration: duration_ms.map(clock),
            duration_ms,
            release_date: value
                .get("release_date")
                .and_then(Value::as_str)
                .map(str::to_owned),
            album_type: (kind == "album")
                .then(|| value.get("album_type").or_else(|| value.get("typ")))
                .flatten()
                .and_then(Value::as_str)
                .map(|t| t.to_ascii_lowercase()),
            explicit: value.get("explicit").and_then(Value::as_bool),
            description,
        })
    }

    /// Normalizes a Spotify Web API object of the given kind (a search hit from `GET
    /// /v1/search`) into the same shape as [`Self::from_player_json`]: lengths come as
    /// `duration_ms`, a playlist's owner as `{display_name, id}`, a show's `by` is its publisher.
    /// `None` for a `null` entry (Spotify leaves those in some lists) or one without an id.
    #[must_use]
    pub fn from_web_json(kind: &str, value: &Value) -> Option<Self> {
        let id = value
            .get("id")?
            .as_str()
            .filter(|id| !id.is_empty())?
            .to_owned();
        let text = |pointer: &str| {
            value
                .pointer(pointer)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        };
        let by = match kind {
            "playlist" => text("/owner/display_name")
                .or_else(|| text("/owner/id"))
                .into_iter()
                .collect(),
            "show" => text("/publisher").into_iter().collect(),
            _ => value
                .get("artists")
                .and_then(Value::as_array)
                .map(|artists| {
                    artists
                        .iter()
                        .filter_map(|a| a.get("name").and_then(Value::as_str).map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        };
        let album = match kind {
            "track" => text("/album/name"),
            "episode" => text("/show/name"),
            _ => None,
        };
        let duration_ms = matches!(kind, "track" | "episode")
            .then(|| value.get("duration_ms").and_then(Value::as_u64))
            .flatten();
        Some(Self {
            kind: kind.to_owned(),
            uri: format!("spotify:{kind}:{id}"),
            id,
            name: text("/name").unwrap_or_default(),
            by,
            album,
            album_uri: container_uri(kind, value),
            duration: duration_ms.map(clock),
            duration_ms,
            release_date: text("/release_date"),
            album_type: (kind == "album")
                .then(|| text("/album_type"))
                .flatten()
                .map(|t| t.to_ascii_lowercase()),
            explicit: value.get("explicit").and_then(Value::as_bool),
            description: text("/description").map(|d| truncate(&d, 280)),
        })
    }

    /// `name — by`.
    #[must_use]
    pub fn label(&self) -> String {
        if self.by.is_empty() {
            self.name.clone()
        } else {
            format!("{} — {}", self.name, self.by.join(", "))
        }
    }
}

/// The uri of a track's album or an episode's show in a spotify_player or Web API object: its
/// `uri`, else built from its `id` (spotify_player gives bare ids). `None` for other kinds, or
/// when neither is usable.
fn container_uri(kind: &str, value: &Value) -> Option<String> {
    let of = match kind {
        "track" => "album",
        "episode" => "show",
        _ => return None,
    };
    let object = value.get(of).filter(|o| o.is_object())?;
    let prefix = format!("spotify:{of}:");
    let text = |key: &str| {
        object
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    if let Some(uri) = text("uri").filter(|uri| uri.starts_with(&prefix)) {
        return Some(uri.to_owned());
    }
    let id = text("id")?;
    let id = id.strip_prefix(&prefix).unwrap_or(id);
    id.chars()
        .all(|c| c.is_ascii_alphanumeric())
        .then(|| format!("{prefix}{id}"))
}

/// Details of one album, artist or track from `spotify_player get item --id <id> <kind>`, as
/// `spotify track <uri>` prints them.
///
/// spotify_player wraps albums as `{"album", "tracks"}` and artists as `{"artist", "top_tracks",
/// "albums", "related_artists"}`; a track is the bare object. The result always has `kind` and
/// `item` (the normalized [`Item`], `null` only when the output has no id; a track's `item` has
/// `album` (its name) and `album_uri`), plus per kind:
/// - track: `album` as `{name, uri}`, so its album can be opened (`spotify track <uri>`) or
///   played;
/// - album: `release_date`, `track_count`, `duration_ms`, `duration`, `tracks`;
/// - artist: `top_tracks`, `albums`, `related_artists`, and `genres`, `followers`, `popularity`
///   when spotify_player reports them.
#[must_use]
pub fn item_view(kind: &str, value: &Value) -> Value {
    let list = |key: &str, of: &str| -> Vec<Item> {
        value
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| Item::from_player_json(of, v))
                    .collect()
            })
            .unwrap_or_default()
    };
    // The object itself sits under its kind's key, or is the whole value.
    let object = value
        .get(kind)
        .filter(|inner| inner.is_object())
        .unwrap_or(value);
    let item = Item::from_player_json(kind, object);
    let mut view = serde_json::json!({"kind": kind, "item": item});
    match kind {
        "track" => {
            if let Some(item) = item
                .as_ref()
                .filter(|i| i.album.is_some() || i.album_uri.is_some())
            {
                view["album"] = serde_json::json!({"name": item.album, "uri": item.album_uri});
            }
        }
        "album" => {
            let tracks = list("tracks", "track");
            let total_ms: u64 = tracks.iter().filter_map(|t| t.duration_ms).sum();
            view["release_date"] =
                serde_json::json!(item.as_ref().and_then(|i| i.release_date.clone()));
            view["track_count"] = serde_json::json!(tracks.len());
            view["duration_ms"] = serde_json::json!(total_ms);
            view["duration"] = serde_json::json!(clock(total_ms));
            view["tracks"] = serde_json::json!(tracks);
        }
        "artist" => {
            view["top_tracks"] = serde_json::json!(list("top_tracks", "track"));
            view["albums"] = serde_json::json!(list("albums", "album"));
            view["related_artists"] = serde_json::json!(list("related_artists", "artist"));
            if let Some(genres) = object.get("genres").and_then(Value::as_array) {
                view["genres"] = Value::Array(genres.clone());
            }
            // The Web API nests followers as {"total": n}; accept a bare number too.
            if let Some(followers) = object
                .get("followers")
                .and_then(|f| f.get("total").unwrap_or(f).as_u64())
            {
                view["followers"] = serde_json::json!(followers);
            }
            if let Some(popularity) = object.get("popularity").and_then(Value::as_u64) {
                view["popularity"] = serde_json::json!(popularity);
            }
        }
        _ => {}
    }
    view
}

/// Truncates on a character boundary, adding `…`.
#[must_use]
pub fn truncate(value: &str, max_chars: usize) -> String {
    if value.chars().count() <= max_chars {
        return value.to_owned();
    }
    let mut out: String = value.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Current UTC time as RFC 3339 with milliseconds.
#[must_use]
pub fn now_rfc3339() -> String {
    rfc3339(time::OffsetDateTime::now_utc())
}

/// Formats a timestamp as RFC 3339 (UTC, milliseconds, `Z`).
#[must_use]
pub fn rfc3339(at: time::OffsetDateTime) -> String {
    let at = at.to_offset(time::UtcOffset::UTC);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}Z",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute(),
        at.second(),
        at.millisecond()
    )
}

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> u64 {
    u64::try_from(time::OffsetDateTime::now_utc().unix_timestamp_nanos() / 1_000_000).unwrap_or(0)
}
