//! `Next:` suggestions after human output: the commands that usually follow, filled in with the
//! real ids and URIs from what was just printed. `Ctx::next` prints them on stderr, only when
//! stdout is a terminal (so they never land ahead of what a pipe prints), never in `--json` mode
//! and never with `SPOTIFY_HINTS=0`. Each function here is pure, so tests pin them down.

use serde_json::Value;

fn s<'a>(v: &'a Value, pointer: &str) -> Option<&'a str> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
}

/// The first hit of a search, in the order `search --play` picks: its kind and URI.
fn first_hit(v: &Value) -> Option<(&'static str, &str)> {
    [
        "tracks",
        "episodes",
        "albums",
        "playlists",
        "artists",
        "shows",
    ]
    .into_iter()
    .find_map(|kind| s(v, &format!("/results/{kind}/0/uri")).map(|uri| (kind, uri)))
}

/// What to do with an item, by kind.
fn with_item(kind: &str, uri: &str) -> Vec<String> {
    match kind {
        "tracks" | "track" => vec![
            format!("spotify play {uri}"),
            format!("spotify lyrics {uri}"),
            format!("spotify queue add {uri}"),
            format!("spotify track {uri}"),
        ],
        "episodes" | "episode" => vec![
            format!("spotify podcast play {uri}"),
            format!("spotify queue add {uri}"),
        ],
        "albums" | "album" => vec![
            format!("spotify play {uri}"),
            format!("spotify track {uri}"),
        ],
        "playlists" | "playlist" => vec![
            format!("spotify playlist play {uri} --shuffle"),
            format!("spotify playlist show {uri}"),
        ],
        "artists" | "artist" => vec![
            format!("spotify play {uri}"),
            format!("spotify track {uri}"),
            format!("spotify play --radio {uri}"),
        ],
        "shows" | "show" => vec![format!("spotify podcast play {uri}")],
        _ => Vec::new(),
    }
}

/// After `spotify search` and `spotify podcast search`.
#[must_use]
pub fn after_search(v: &Value) -> Vec<String> {
    first_hit(v).map_or_else(Vec::new, |(kind, uri)| with_item(kind, uri))
}

/// After `spotify status` (and `spotify podcast now`).
#[must_use]
pub fn after_status(v: &Value) -> Vec<String> {
    let playback = &v["playback"];
    match s(playback, "/state") {
        Some("not_running") => return vec!["spotify launch".into()],
        _ if playback.get("track").is_none_or(Value::is_null) => {
            return vec!["spotify play --search '<song or artist>'".into()];
        }
        _ => {}
    }
    let paused = s(playback, "/state") == Some("paused");
    let remaining = playback
        .get("remaining_ms")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let checkpoint = if remaining > 35_000 {
        "spotify trigger add --remaining 30s --note '<what to do>'"
    } else {
        "spotify trigger add --end --note '<what to do>'"
    };
    let mut next = Vec::new();
    if paused {
        next.push("spotify resume".to_owned());
    }
    if s(playback, "/track/kind") == Some("episode") {
        next.push("spotify seek +30s".into());
    } else {
        next.push("spotify lyrics".into());
        next.push("spotify track".into());
    }
    if !paused {
        next.push(checkpoint.into());
    }
    next
}

/// After something started playing (`play`, `playlist play`, `podcast play`, `search --play`).
#[must_use]
pub fn after_play(v: &Value) -> Vec<String> {
    let playback = &v["playback"];
    if s(playback, "/state") != Some("playing") || playback.get("track").is_none_or(Value::is_null)
    {
        return Vec::new();
    }
    if s(playback, "/track/kind") == Some("episode") {
        return vec![
            "spotify seek +30s".into(),
            "spotify trigger add --end --note '<what to do>'".into(),
        ];
    }
    vec![
        "spotify lyrics".into(),
        "spotify queue add --search '<song>'".into(),
        "spotify trigger add --end --note '<what to do>'".into(),
    ]
}

/// After `spotify track`: for the current song, or for the item looked up.
#[must_use]
pub fn after_track(v: &Value) -> Vec<String> {
    if let Some(uri) = s(v, "/item/uri").or_else(|| s(v, "/playlist/uri")) {
        let kind = s(v, "/kind").unwrap_or_else(|| {
            if v.get("playlist").is_some_and(|p| !p.is_null()) {
                "playlist"
            } else {
                "track"
            }
        });
        let mut next = with_item(kind, uri);
        // It is described already.
        next.retain(|n| {
            !n.starts_with("spotify track ") && !n.starts_with("spotify playlist show ")
        });
        if kind == "track" || kind == "album" {
            next.push(format!("spotify playlist add <playlist-id> {uri}"));
        }
        next.truncate(3);
        return next;
    }
    match (s(v, "/track/kind"), s(v, "/track/uri")) {
        (Some("episode"), _) => vec!["spotify podcast now".into()],
        (_, Some(uri)) => {
            let mut next = vec!["spotify lyrics".to_owned()];
            if v.get("liked").and_then(Value::as_bool) == Some(false) {
                next.push("spotify like".into());
            }
            next.push(format!("spotify playlist add <playlist-id> {uri}"));
            next
        }
        _ => Vec::new(),
    }
}

/// After `spotify lyrics`: `target` is whether a song other than the current one was asked for.
#[must_use]
pub fn after_lyrics(v: &Value, target: bool) -> Vec<String> {
    match s(v, "/track") {
        Some(uri) if target => vec![
            format!("spotify play {uri}"),
            format!("spotify queue add {uri}"),
            format!("spotify track {uri}"),
        ],
        Some(_) => vec!["spotify track".into()],
        None => Vec::new(),
    }
}

/// After `spotify queue`.
#[must_use]
pub fn after_queue(v: &Value) -> Vec<String> {
    let managed = v["managed"].as_array().map_or(0, Vec::len);
    match managed {
        0 => vec!["spotify queue add --search '<song>'".into()],
        1 => vec!["spotify next".into(), "spotify queue remove 1".into()],
        n => vec![
            "spotify next".into(),
            format!("spotify queue move {n} 1"),
            "spotify queue remove 1".into(),
        ],
    }
}

/// After `spotify queue add`.
#[must_use]
pub fn after_queue_add(_: &Value) -> Vec<String> {
    vec!["spotify queue".into(), "spotify next".into()]
}

/// After `spotify library <section>`.
#[must_use]
pub fn after_library(v: &Value, section: &str) -> Vec<String> {
    let first = s(v, "/items/0/uri");
    let mut next = Vec::new();
    if section == "liked" {
        next.push("spotify play --liked --random".to_owned());
    }
    if let Some(uri) = first {
        let kind = uri.split(':').nth(1).unwrap_or("track");
        next.extend(with_item(kind, uri).into_iter().take(2));
    }
    next.truncate(3);
    next
}

/// After `spotify playlist list`.
#[must_use]
pub fn after_playlist_list(v: &Value) -> Vec<String> {
    s(v, "/items/0/uri").map_or_else(Vec::new, |uri| {
        vec![
            format!("spotify playlist show {uri}"),
            format!("spotify playlist play {uri} --shuffle"),
        ]
    })
}

/// After `spotify playlist show`.
#[must_use]
pub fn after_playlist_show(v: &Value) -> Vec<String> {
    s(v, "/playlist/uri").map_or_else(Vec::new, |uri| {
        vec![
            format!("spotify playlist play {uri} --shuffle"),
            format!("spotify playlist add {uri} spotify:track:<id>"),
        ]
    })
}

/// After `spotify playlist create` and `spotify playlist fork`.
#[must_use]
pub fn after_playlist_created(v: &Value) -> Vec<String> {
    s(v, "/uri").map_or_else(Vec::new, |uri| {
        vec![
            format!("spotify playlist add {uri} spotify:track:<id>"),
            format!("spotify playlist play {uri}"),
        ]
    })
}

/// After `spotify devices`.
#[must_use]
pub fn after_devices(v: &Value) -> Vec<String> {
    v["devices"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|d| d.get("is_active").and_then(Value::as_bool) != Some(true))
        .and_then(|d| s(d, "/id"))
        .map_or_else(Vec::new, |id| vec![format!("spotify devices connect {id}")])
}

/// After `spotify trigger add`.
#[must_use]
pub fn after_trigger_add(v: &Value, local: bool) -> Vec<String> {
    let id = s(v, "/trigger/id").unwrap_or("<id>");
    if local {
        vec![
            format!("spotify trigger wait {id}"),
            format!("spotify trigger history {id}"),
        ]
    } else {
        vec![
            format!("spotify trigger history {id}"),
            format!("spotify trigger remove {id}"),
            "spotify trigger test".into(),
        ]
    }
}

/// After `spotify trigger list`.
#[must_use]
pub fn after_trigger_list(v: &Value) -> Vec<String> {
    s(v, "/triggers/0/id").map_or_else(Vec::new, |id| {
        vec![
            format!("spotify trigger show {id}"),
            format!("spotify trigger remove {id}"),
        ]
    })
}

/// After `spotify login` and a positive `spotify login status`.
#[must_use]
pub fn after_login() -> Vec<String> {
    vec![
        "spotify ting authorize".into(),
        "spotify login contexts".into(),
    ]
}

/// The `Next:` text: one line when it fits in 100 columns, else one command per line.
#[must_use]
pub fn format(steps: &[String]) -> Option<String> {
    if steps.is_empty() {
        return None;
    }
    let line = format!("Next: {}", steps.join(" · "));
    if line.chars().count() <= 100 {
        return Some(line);
    }
    Some(format!("Next: {}", steps.join("\n      ")))
}

/// When `Next:` suggestions are printed, by `SPOTIFY_HINTS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hints {
    /// Only when stdout is a terminal (unset, or any other value): a pipe's reader prints its
    /// part later than stderr reaches the terminal, so a hint would come before the output.
    Terminal,
    /// `0`, `false`, `off` or `no` (any case): never.
    Off,
    /// `always`: also when stdout is a pipe or a file (the hint still goes to stderr).
    Always,
}

impl Hints {
    /// The mode a `SPOTIFY_HINTS` value asks for.
    #[must_use]
    pub fn from_env(value: Option<&str>) -> Self {
        match value.map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if silicon_spotify_client::is_off(&v) => Self::Off,
            Some(v) if v == "always" => Self::Always,
            _ => Self::Terminal,
        }
    }

    /// Whether to print them, given whether stdout is a terminal.
    #[must_use]
    pub fn show(self, stdout_is_terminal: bool) -> bool {
        match self {
            Self::Off => false,
            Self::Always => true,
            Self::Terminal => stdout_is_terminal,
        }
    }
}

/// Whether hints are printed now: `SPOTIFY_HINTS=0|false|off|no` turns them off,
/// `SPOTIFY_HINTS=always` prints them even when stdout is not a terminal; otherwise only when it
/// is one.
#[must_use]
pub fn enabled() -> bool {
    use std::io::IsTerminal as _;
    Hints::from_env(std::env::var("SPOTIFY_HINTS").ok().as_deref())
        .show(std::io::stdout().is_terminal())
}
