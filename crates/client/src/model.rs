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
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WebPlayback {
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
        })
    }
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
    /// Length in milliseconds (tracks, episodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// Length as `m:ss`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration: Option<String>,
    /// Release date (albums, episodes).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_date: Option<String>,
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
            duration: duration_ms.map(clock),
            duration_ms,
            release_date: value
                .get("release_date")
                .and_then(Value::as_str)
                .map(str::to_owned),
            explicit: value.get("explicit").and_then(Value::as_bool),
            description,
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalizes_spotify_player_items() {
        let track = json!({"id":"0BxE4FqsDD1Ot4YuBXwAPp","name":"505","artists":[{"id":"x","name":"Arctic Monkeys"}],
            "album":{"id":"a","name":"Favourite Worst Nightmare"},"duration":{"secs":253,"nanos":586000000},"explicit":false});
        let item = Item::from_player_json("track", &track).expect("item");
        assert_eq!(item.uri, "spotify:track:0BxE4FqsDD1Ot4YuBXwAPp");
        assert_eq!(item.duration_ms, Some(253_586));
        assert_eq!(item.duration.as_deref(), Some("4:13"));
        assert_eq!(item.label(), "505 — Arctic Monkeys");
        let playlist =
            json!({"id":"p1","name":"Mix","owner":["Playlist Owner","owner_id"],"desc":""});
        let item = Item::from_player_json("playlist", &playlist).expect("playlist");
        assert_eq!(item.by, vec!["Playlist Owner".to_owned()]);
        assert_eq!(item.description, None);
    }

    #[test]
    fn playback_derives_progress() {
        let mut playback = Playback::empty(PlayerState::Playing);
        let mut track = Track {
            uri: "spotify:track:abc".into(),
            duration_ms: 200_000,
            ..Track::default()
        };
        track.finish();
        playback.track = Some(track);
        playback.position_ms = 50_000;
        playback.finish();
        assert!((playback.progress - 0.25).abs() < f64::EPSILON);
        assert_eq!(playback.remaining_ms, 150_000);
        assert_eq!(
            playback.track.as_ref().map(|t| t.kind.as_str()),
            Some("track")
        );
    }

    #[test]
    fn rfc3339_is_utc_millis() {
        let at = time::OffsetDateTime::from_unix_timestamp(0).expect("epoch");
        assert_eq!(rfc3339(at), "1970-01-01T00:00:00.000Z");
    }
}
