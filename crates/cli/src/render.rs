//! Human renderings. JSON output never goes through here.

use std::fmt::Write as _;

use serde_json::Value;

fn s<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or("")
}

/// `text` on one line: every run of whitespace and control characters (newlines, tabs, the
/// Unicode line and paragraph separators) becomes one space. Spotify names can contain line
/// breaks, which would break the list layout; --json keeps them as they are.
pub(crate) fn one_line(text: &str) -> String {
    text.split(|c: char| c.is_whitespace() || c.is_control())
        .filter(|word| !word.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// A name (or other Spotify-provided text) at `pointer`, on one line.
fn name(v: &Value, pointer: &str) -> String {
    one_line(s(v, pointer))
}

/// `name — by`, without a dangling separator when `by` is empty (podcast episodes have no artist).
fn titled(title: &str, by: &str) -> String {
    if by.is_empty() {
        title.to_owned()
    } else {
        format!("{title} — {by}")
    }
}

/// `liked: yes|no` when the value says whether the song is in Liked Songs.
fn liked(v: &Value) -> Option<&'static str> {
    ["/liked", "/track/liked", "/item/liked"]
        .iter()
        .find_map(|p| v.pointer(p).and_then(Value::as_bool))
        .map(|liked| if liked { "yes" } else { "no" })
}

fn state_icon(state: &str) -> &'static str {
    match state {
        "playing" => "▶",
        "paused" => "⏸",
        "stopped" => "■",
        _ => "○",
    }
}

fn bar(progress: f64, width: usize) -> String {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss
    )]
    let filled = ((progress.clamp(0.0, 1.0)) * width as f64).round() as usize;
    format!(
        "{}{}",
        "━".repeat(filled),
        "─".repeat(width.saturating_sub(filled))
    )
}

/// A playback snapshot.
#[must_use]
pub fn playback(p: &Value) -> String {
    let state = s(p, "/state");
    if state == "not_running" {
        return "Spotify.app is not running. Start it with `spotify launch`.".into();
    }
    if p.get("track").is_none_or(Value::is_null) {
        return format!("{} Nothing loaded in Spotify.", state_icon(state));
    }
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} {}",
        state_icon(state),
        titled(&name(p, "/track/name"), &name(p, "/track/artist"))
    );
    let album = name(p, "/track/album");
    if !album.is_empty() {
        let _ = writeln!(out, "  {album}");
    }
    let progress = p.get("progress").and_then(Value::as_f64).unwrap_or(0.0);
    let remaining = p.get("remaining_ms").and_then(Value::as_u64).unwrap_or(0);
    let _ = writeln!(
        out,
        "  {}  {} / {}  (-{})",
        bar(progress, 24),
        s(p, "/position"),
        s(p, "/track/duration"),
        silicon_spotify_client::timing::clock_rounded(remaining)
    );
    let mut flags = Vec::new();
    if let Some(v) = p.get("volume").and_then(Value::as_u64) {
        flags.push(format!("volume {v}%"));
    }
    if p.get("shuffling").and_then(Value::as_bool) == Some(true) {
        flags.push("shuffle".into());
    }
    // Web API facts that could not be matched to what Spotify.app plays now (`web.stale`).
    let stale = p.pointer("/web/stale").and_then(Value::as_bool) == Some(true);
    // Spotify.app's own flag is current; the Web API's mode tells repeat-one apart when fresh.
    let web_repeat = if stale {
        None
    } else {
        p.pointer("/web/repeat_state").and_then(Value::as_str)
    };
    match web_repeat {
        Some("track") => flags.push("repeat one".into()),
        Some("context") => flags.push("repeat".into()),
        _ if p.get("repeating").and_then(Value::as_bool) == Some(true) => {
            flags.push("repeat".into())
        }
        _ => {}
    }
    if let Some(ctx) = p.pointer("/web/context_uri").and_then(Value::as_str) {
        flags.push(format!("from {ctx}"));
    }
    if let Some(device) = p.pointer("/web/device/name").and_then(Value::as_str) {
        flags.push(format!("on {}", one_line(device)));
    }
    if stale {
        flags.push("web data out of date".into());
    } else if p.pointer("/web/relinked").and_then(Value::as_bool) == Some(true) {
        flags.push("relinked".into());
    }
    if !flags.is_empty() {
        let _ = writeln!(out, "  {}", flags.join(" · "));
    }
    let _ = write!(out, "  {}", s(p, "/track/uri"));
    out
}

/// `spotify status`.
#[must_use]
pub fn status(v: &Value) -> String {
    let mut out = playback(&v["playback"]);
    if let Some(queued) = v
        .get("managed_queue")
        .and_then(Value::as_u64)
        .filter(|q| *q > 0)
    {
        let _ = write!(out, "\n  {queued} managed item(s) queued (spotify queue)");
    }
    for warning in v
        .get("warnings")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let _ = write!(
            out,
            "\n  note: {} ({})",
            s(warning, "/message"),
            s(warning, "/code")
        );
    }
    out
}

/// A control outcome (`via`, `fallback`, resulting playback).
#[must_use]
pub fn outcome(v: &Value) -> String {
    // A `next` that skipped a managed item whose hand-off was still in flight.
    let skipped = v
        .get("skipped")
        .and_then(Value::as_str)
        .map(|uri| format!("\n  skipped {uri} (it was still being switched to)"))
        .unwrap_or_default();
    if v.get("source").and_then(Value::as_str) == Some("managed_queue") {
        return format!(
            "▶ Next from the managed queue: {} ({} left in queue){skipped}",
            queue_item_label(&v["playing"]),
            v["queue_remaining"]
        );
    }
    // What `previous` did: it restarts the item from 3 s in, else goes to the one before.
    let mut out = match v.get("result").and_then(Value::as_str) {
        Some("restarted") => "Back to the start of this item.\n".to_owned(),
        Some("previous_item") => "Back to the previous item.\n".to_owned(),
        Some(other) => format!("{}.\n", other.replace('_', " ")),
        None => String::new(),
    };
    out.push_str(&playback(&v["playback"]));
    let via = s(v, "/via");
    let _ = write!(out, "\n  via {via}");
    if let Some(reason) = v.pointer("/fallback/reason") {
        let _ = write!(
            out,
            " (spotify_player: {} — {})",
            s(reason, "/code"),
            s(reason, "/message")
        );
    }
    out.push_str(&skipped);
    out
}

/// `spotify launch`.
#[must_use]
pub fn launched(v: &Value) -> String {
    match v.get("already_running").and_then(Value::as_bool) {
        Some(true) => "Spotify.app was already running.".into(),
        Some(false) if v.get("launched").and_then(Value::as_bool) == Some(true) => {
            "Started Spotify.app (hidden).".into()
        }
        // A daemon from before `already_running` (0.1.2 and older) always answers
        // `launched: true`, also when Spotify.app was running, so it cannot tell.
        _ => "Spotify.app is running.".into(),
    }
}

/// `3 tracks`, `1 track`.
fn count(n: u64, noun: &str) -> String {
    if n == 1 {
        format!("1 {noun}")
    } else {
        format!("{n} {noun}s")
    }
}

fn names(item: &Value) -> String {
    item["by"]
        .as_array()
        .map(|b| {
            b.iter()
                .filter_map(Value::as_str)
                .map(one_line)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default()
}

/// Numbered item lines, at most `max` (then `… N more`). Albums show release date and type.
fn numbered(out: &mut String, items: &[Value], max: usize) {
    for (index, item) in items.iter().take(max).enumerate() {
        let mut line = item_line(item);
        if let Some(date) = item.get("release_date").and_then(Value::as_str)
            && item.get("kind").and_then(Value::as_str) == Some("album")
        {
            let facts = match item.get("album_type").and_then(Value::as_str) {
                Some(kind) => format!(" ({date}, {kind})"),
                None => format!(" ({date})"),
            };
            // After the name and artists, before the URI line.
            match line.find('\n') {
                Some(at) => line.insert_str(at, &facts),
                None => line.push_str(&facts),
            }
        }
        let _ = writeln!(out, "  {:>3}. {line}", index + 1);
    }
    if items.len() > max {
        let _ = writeln!(out, "  … {} more (--json lists all)", items.len() - max);
    }
}

/// `spotify track spotify:album:…`.
fn album(v: &Value) -> String {
    let item = &v["item"];
    let mut out = titled(&name(item, "/name"), &names(item));
    let mut facts = Vec::new();
    if let Some(kind) = item.get("album_type").and_then(Value::as_str) {
        facts.push(kind.to_owned());
    }
    if let Some(date) = v.get("release_date").and_then(Value::as_str) {
        facts.push(format!("released {date}"));
    }
    facts.push(count(v["track_count"].as_u64().unwrap_or(0), "track"));
    if let Some(duration) = v.get("duration").and_then(Value::as_str) {
        facts.push(duration.to_owned());
    }
    let _ = writeln!(out, "\n  {}\n  {}", facts.join(" · "), s(item, "/uri"));
    let tracks = v["tracks"].as_array().cloned().unwrap_or_default();
    numbered(&mut out, &tracks, usize::MAX);
    out
}

/// `spotify track spotify:artist:…`.
fn artist(v: &Value) -> String {
    let item = &v["item"];
    let mut out = format!("{}\n  {}", name(item, "/name"), s(item, "/uri"));
    let genres: Vec<String> = v["genres"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(one_line)
        .collect();
    if !genres.is_empty() {
        let _ = write!(out, "\n  genres: {}", genres.join(", "));
    }
    let mut facts = Vec::new();
    if let Some(followers) = v.get("followers").and_then(Value::as_u64) {
        facts.push(format!("{followers} followers"));
    }
    if let Some(popularity) = v.get("popularity").and_then(Value::as_u64) {
        facts.push(format!("popularity {popularity}/100"));
    }
    if !facts.is_empty() {
        let _ = write!(out, "\n  {}", facts.join(" · "));
    }
    out.push('\n');
    for (key, title, max) in [
        ("top_tracks", "Top tracks", 10),
        ("albums", "Albums and singles", 10),
    ] {
        let items = v[key].as_array().cloned().unwrap_or_default();
        if !items.is_empty() {
            let _ = writeln!(out, "{title} ({}):", items.len());
            numbered(&mut out, &items, max);
        }
    }
    let related: Vec<String> = v["related_artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .map(one_line)
        .take(10)
        .collect();
    if !related.is_empty() {
        let _ = writeln!(out, "Related: {}", related.join(", "));
    }
    out
}

/// A song's album URI: the `album_uri` the daemon gives, else the album object spotify_player
/// reported (`album` of the current song, `raw.album` of a looked-up one).
fn album_uri(song: &Value, album: Option<&Value>) -> Option<String> {
    let valid_id = |id: &str| id.len() == 22 && id.chars().all(|c| c.is_ascii_alphanumeric());
    let given = |uri: Option<&Value>| {
        uri.and_then(Value::as_str)
            .filter(|u| u.strip_prefix("spotify:album:").is_some_and(valid_id))
            .map(str::to_owned)
    };
    given(song.get("album_uri"))
        .or_else(|| given(album.and_then(|a| a.get("uri"))))
        .or_else(|| {
            album
                .and_then(|a| a.get("id"))
                .and_then(Value::as_str)
                .filter(|id| valid_id(id))
                .map(|id| format!("spotify:album:{id}"))
        })
}

/// `spotify track`.
#[must_use]
pub fn track(v: &Value) -> String {
    if let Some(playlist) = v.get("playlist").filter(|p| !p.is_null()) {
        return playlist_show(v, playlist);
    }
    match v.get("kind").and_then(Value::as_str) {
        Some("album") if !v["item"].is_null() => return album(v),
        Some("artist") if !v["item"].is_null() => return artist(v),
        _ => {}
    }
    if let Some(item) = v.get("item").filter(|i| !i.is_null()) {
        let mut out = format!(
            "{}\n  {}",
            titled(&name(item, "/name"), &names(item)),
            s(item, "/uri")
        );
        if let Some(album) = item.get("album").and_then(Value::as_str) {
            let _ = write!(out, "\n  album: {}", one_line(album));
            // Songs only: an episode's `album` is its show.
            if s(item, "/kind") != "episode"
                && let Some(uri) = album_uri(item, v.pointer("/raw/album"))
            {
                let _ = write!(out, " · {uri}");
            }
        }
        if let Some(d) = item.get("duration").and_then(Value::as_str) {
            let _ = write!(out, "\n  length: {d}");
        }
        if let Some(r) = item.get("release_date").and_then(Value::as_str) {
            let _ = write!(out, "\n  released: {r}");
        }
        if let Some(liked) = liked(v) {
            let _ = write!(out, "\n  liked: {liked}");
        }
        return out;
    }
    let t = &v["track"];
    let mut out = format!(
        "{}\n  {}: {}",
        titled(&name(t, "/name"), &name(t, "/artist")),
        // An episode's "album" is its show.
        if s(t, "/kind") == "episode" {
            "show"
        } else {
            "album"
        },
        name(t, "/album")
    );
    if let Some(date) = v.pointer("/album/release_date").and_then(Value::as_str) {
        let _ = write!(out, " ({date})");
    }
    if s(t, "/kind") != "episode"
        && let Some(uri) = album_uri(t, v.get("album"))
    {
        let _ = write!(out, " · {uri}");
    }
    let _ = write!(
        out,
        "\n  length: {}  ·  at {} (-{})",
        s(t, "/duration"),
        s(v, "/playback/position"),
        silicon_spotify_client::timing::clock_rounded(
            v.pointer("/playback/remaining_ms")
                .and_then(Value::as_u64)
                .unwrap_or(0)
        )
    );
    if let Some(p) = t.get("popularity").and_then(Value::as_u64) {
        let _ = write!(out, "\n  popularity: {p}/100");
    }
    if v.get("explicit").and_then(Value::as_bool) == Some(true) {
        let _ = write!(out, "\n  explicit");
    }
    if let Some(liked) = liked(v) {
        let _ = write!(out, "\n  liked: {liked}");
    }
    let _ = write!(out, "\n  {}\n  {}", s(t, "/uri"), s(t, "/url"));
    out
}

fn item_line(item: &Value) -> String {
    let mut line = titled(&name(item, "/name"), &names(item));
    if let Some(d) = item.get("duration").and_then(Value::as_str) {
        let _ = write!(line, " ({d})");
    }
    format!("{line}\n      {}", s(item, "/uri"))
}

/// `spotify search`.
#[must_use]
pub fn search(v: &Value) -> String {
    let mut out = String::new();
    if let Some(results) = v.get("results").and_then(Value::as_object) {
        for (kind, items) in results {
            let Some(items) = items.as_array().filter(|i| !i.is_empty()) else {
                continue;
            };
            let _ = writeln!(out, "{kind}:");
            for (index, item) in items.iter().enumerate() {
                let _ = writeln!(out, "  {:>2}. {}", index + 1, item_line(item));
            }
        }
    }
    if out.is_empty() {
        return format!("No results for `{}`.", s(v, "/query"));
    }
    // What to do with a hit follows on stderr (`Next:`, with the first hit's URI).
    out
}

/// Library sections and saved shows.
#[must_use]
pub fn items_list(v: &Value) -> String {
    let items = v["items"].as_array().cloned().unwrap_or_default();
    if items.is_empty() {
        return "Nothing here.".into();
    }
    let mut out = String::new();
    if let Some(total) = v.get("total").and_then(Value::as_u64) {
        let _ = writeln!(out, "{} ({total})", s(v, "/section"));
    }
    for (index, item) in items.iter().enumerate() {
        let _ = writeln!(out, "  {:>3}. {}", index + 1, item_line(item));
    }
    out
}

/// `spotify devices`.
#[must_use]
pub fn devices(v: &Value) -> String {
    let list = v["devices"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No Spotify Connect devices are visible. Open Spotify on a device first.".into();
    }
    list.iter()
        .map(|d| {
            format!(
                "{} {} ({}){}  id: {}",
                if d["is_active"] == Value::Bool(true) {
                    "●"
                } else {
                    "○"
                },
                name(d, "/name"),
                s(d, "/type"),
                d.get("volume_percent")
                    .and_then(Value::as_u64)
                    .map(|v| format!(" {v}%"))
                    .unwrap_or_default(),
                s(d, "/id")
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// `spotify:episode:<id>` as `episode <id>`: what a queued item without a known name shows.
fn kind_and_id(uri: &str) -> String {
    match uri
        .strip_prefix("spotify:")
        .and_then(|rest| rest.split_once(':'))
    {
        Some((kind, id)) if !kind.is_empty() && !id.is_empty() => format!("{kind} {id}"),
        _ => uri.to_owned(),
    }
}

/// A queued item: `name — artists` (or its show), else `track <id>` / `episode <id>`. `by` is
/// one string for managed items and a list for Spotify's upcoming items.
#[must_use]
pub fn queue_item_label(item: &Value) -> String {
    let title = name(item, "/name");
    if title.is_empty() {
        return kind_and_id(s(item, "/uri"));
    }
    let by = match item.get("by") {
        Some(Value::Array(_)) => names(item),
        _ => name(item, "/by"),
    };
    let episode = s(item, "/kind") == "episode" || s(item, "/uri").starts_with("spotify:episode:");
    let by = if by.is_empty() && episode {
        // An episode's show (a track's album is not who it is by).
        name(item, "/album")
    } else {
        by
    };
    titled(&title, &by)
}

/// `spotify queue add`.
#[must_use]
pub fn queue_added(v: &Value) -> String {
    let added = v["added"].as_array().cloned().unwrap_or_default();
    let mut out = format!(
        "Queued {}. Managed queue now has {}.",
        count(added.len() as u64, "item"),
        v["queue"].as_array().map_or(0, Vec::len)
    );
    for item in &added {
        let _ = write!(out, "\n  + {}", queue_item_label(item));
    }
    if let Some(note) = v.get("note").and_then(Value::as_str) {
        let _ = write!(out, "\n{}", one_line(note));
    }
    out
}

/// `spotify queue`.
#[must_use]
pub fn queue(v: &Value) -> String {
    let mut out = String::new();
    let managed = v["managed"].as_array().cloned().unwrap_or_default();
    if managed.is_empty() {
        out.push_str("Managed queue: empty (add with `spotify queue add <uri>`)\n");
    } else {
        out.push_str("Managed queue (plays next, editable):\n");
        for (index, item) in managed.iter().enumerate() {
            let _ = writeln!(
                out,
                "  {:>2}. {}  [{}]",
                index + 1,
                queue_item_label(item),
                s(item, "/id")
            );
        }
    }
    let upcoming = v
        .pointer("/spotify_upcoming/items")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !upcoming.is_empty() {
        out.push_str("Then Spotify's upcoming (read-only):\n");
        for (index, item) in upcoming.iter().take(10).enumerate() {
            let _ = writeln!(out, "  {:>2}. {}", index + 1, queue_item_label(item));
        }
        if upcoming.len() > 10 {
            let _ = writeln!(out, "  … {} more", upcoming.len() - 10);
        }
    } else if let Some(error) = v.pointer("/spotify_upcoming/error") {
        let _ = writeln!(
            out,
            "Spotify's upcoming list is unavailable: {}",
            s(error, "/message")
        );
    }
    // E.g. the item playing now, which Spotify repeats there, was left out.
    if let Some(note) = v.pointer("/spotify_upcoming/note").and_then(Value::as_str) {
        let _ = writeln!(out, "{note}");
    }
    // E.g. no_active_device: Spotify's list may be empty because no device plays.
    let mut seen: Vec<String> = Vec::new();
    for warning in ["/warnings", "/spotify_upcoming/warnings"]
        .iter()
        .filter_map(|at| v.pointer(at).and_then(Value::as_array))
        .flatten()
    {
        let code = s(warning, "/code").to_owned();
        // The same warning in both places once; two without a code are told apart by their text.
        let key = if code.is_empty() {
            s(warning, "/message").to_owned()
        } else {
            code.clone()
        };
        if seen.contains(&key) {
            continue;
        }
        let _ = write!(out, "note: {}", s(warning, "/message"));
        if !code.is_empty() {
            let _ = write!(out, " ({code})");
        }
        if let Some(hint) = warning.get("hint").and_then(Value::as_str) {
            let _ = write!(out, "\n  {hint}");
        }
        out.push('\n');
        seen.push(key);
    }
    out
}

fn playlist_show(v: &Value, playlist: &Value) -> String {
    let mut out = format!(
        "{} — {}, {}\n  {}\n",
        name(playlist, "/name"),
        count(v["track_count"].as_u64().unwrap_or(0), "track"),
        s(v, "/duration"),
        s(playlist, "/uri")
    );
    for (index, track) in v["tracks"].as_array().into_iter().flatten().enumerate() {
        let _ = writeln!(out, "  {:>3}. {}", index + 1, item_line(track));
    }
    out
}

/// Playlist ops.
#[must_use]
pub fn playlist(op: &str, v: &Value) -> String {
    match op {
        "playlist.list" => items_list(v),
        "playlist.show" => playlist_show(v, &v["playlist"]),
        "playlist.create" => format!("Created playlist {} ({}).", name(v, "/name"), s(v, "/uri")),
        "playlist.delete" => format!("{}\n{}", s(v, "/message"), s(v, "/note")),
        "playlist.fork" => {
            let mut out = match v.get("uri").and_then(Value::as_str) {
                Some(uri) => {
                    let name = v
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| format!("'{}' ", one_line(n)))
                        .unwrap_or_default();
                    format!(
                        "Forked spotify:playlist:{} into {name}({uri}).",
                        s(v, "/from")
                    )
                }
                None => s(v, "/message").to_owned(),
            };
            if let Some(note) = v.get("note").and_then(Value::as_str) {
                let _ = write!(out, "\n{note}");
            }
            out
        }
        "playlist.add" | "playlist.remove" => v["results"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|r| s(r, "/message").to_owned())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => s(v, "/message").to_owned(),
    }
}

/// One trigger line.
#[must_use]
pub fn trigger_line(t: &Value) -> String {
    let mut line = format!(
        "{}  {}  [{}]",
        s(t, "/id"),
        s(t, "/description"),
        t.pointer("/scope/kind")
            .and_then(Value::as_str)
            .unwrap_or("")
    );
    if let Some(label) = t.get("label").and_then(Value::as_str) {
        let _ = write!(line, "  {label}");
    }
    let _ = write!(line, "  {}", s(t, "/status"));
    if let Some(fired) = t.get("fired").and_then(Value::as_u64).filter(|f| *f > 0) {
        let _ = write!(line, " (fired {fired}×)");
    }
    if t.pointer("/delivery/ting") == Some(&Value::Bool(false)) {
        line.push_str("  local");
    }
    if let Some(note) = t.get("note").and_then(Value::as_str) {
        let _ = write!(line, "\n      note: {note}");
    }
    line
}

/// `trigger list`.
#[must_use]
pub fn triggers(v: &Value) -> String {
    let list = v["triggers"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No triggers. Add one: spotify trigger add --remaining 30s --note 'wrap up'".into();
    }
    list.iter().map(trigger_line).collect::<Vec<_>>().join("\n")
}

/// `trigger add`.
#[must_use]
pub fn trigger_added(v: &Value) -> String {
    let t = &v["trigger"];
    let mut out = format!("Trigger {} set: {}", s(t, "/id"), s(t, "/description"));
    match t.pointer("/scope/kind").and_then(Value::as_str) {
        Some("current") => {
            let _ = write!(
                out,
                " on {}",
                one_line(
                    v.pointer("/now_playing/name")
                        .and_then(Value::as_str)
                        .unwrap_or("the current song")
                )
            );
        }
        Some("every") => out.push_str(" on every song"),
        Some("track") => {
            let _ = write!(out, " on every play of {}", s(t, "/scope/uri"));
        }
        _ => {}
    }
    if let Some(recipient) = t
        .pointer("/delivery/recipient")
        .and_then(Value::as_str)
        .filter(|_| t.pointer("/delivery/ting") == Some(&Value::Bool(true)))
    {
        let _ = write!(out, "\n  notifies {recipient} through Ting");
    }
    out
}

/// One firing.
#[must_use]
pub fn firing_line(f: &Value) -> String {
    let mut line = format!(
        "{}  {} {}  {}  {}",
        s(f, "/created_at"),
        s(f, "/trigger_id"),
        s(f, "/outcome"),
        s(f, "/data/trigger/description"),
        s(f, "/state")
    );
    if let Some(track) = f.pointer("/data/track/name").and_then(Value::as_str) {
        let _ = write!(
            line,
            "\n      {} at {}",
            one_line(track),
            s(f, "/data/playback/position")
        );
    }
    if let Some(error) = f.get("last_error").filter(|e| !e.is_null()) {
        let _ = write!(
            line,
            "\n      delivery: {} — {}",
            s(error, "/code"),
            s(error, "/message")
        );
    }
    if let Some(id) = f.pointer("/ting/id").and_then(Value::as_str) {
        let _ = write!(line, "\n      ting: {id}");
    }
    line
}

/// `trigger history`.
#[must_use]
pub fn history(v: &Value) -> String {
    let list = v["firings"].as_array().cloned().unwrap_or_default();
    if list.is_empty() {
        return "No firings yet.".into();
    }
    list.iter().map(firing_line).collect::<Vec<_>>().join("\n")
}

/// `trigger show`.
#[must_use]
pub fn trigger_detail(v: &Value) -> String {
    let mut out = trigger_line(&v["trigger"]);
    let firings = history(&serde_json::json!({"firings": v["firings"]}));
    let _ = write!(out, "\n\n{firings}");
    out
}

/// `3 s`, `500 ms`.
fn millis(ms: u64) -> String {
    if ms >= 1000 && ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{ms} ms")
    }
}

/// Who holds spotify_player's client port, as the daemon describes it: `null` (nobody),
/// `"unknown"`, or `{pid, parent_pid, kind, command}`.
fn port_owner(owner: &Value) -> String {
    let Some(pid) = owner.get("pid").and_then(Value::as_u64) else {
        return match owner.as_str() {
            Some("unknown") => "an unknown process".into(),
            Some(other) => other.to_owned(),
            None => "nobody".into(),
        };
    };
    let what = match s(owner, "/kind") {
        "your_spotify_player" => "your own spotify_player",
        "stale_warm_copy" => "a warm copy left by an earlier daemon",
        "another_daemons_warm_copy" => "another daemon's warm copy",
        "spotify_player_command" => "a one-off spotify_player command",
        "other_process" => "another program",
        _ => "a process",
    };
    match owner.get("command").and_then(Value::as_str).map(one_line) {
        Some(command) if !command.is_empty() && s(owner, "/kind") == "other_process" => {
            format!("pid {pid}, {what}: {command}")
        }
        _ => format!("pid {pid}, {what}"),
    }
}

/// An error the daemon reports as a string or as `{code, message}`.
fn error_text(error: &Value) -> String {
    error.as_str().map_or_else(
        || {
            let message = name(error, "/message");
            match error.get("code").and_then(Value::as_str) {
                Some(code) if !message.is_empty() => format!("{message} ({code})"),
                Some(code) => code.to_owned(),
                None => message,
            }
        },
        one_line,
    )
}

/// The daemon's warm spotify_player: `starting`, `running`, `not_serving`, `deferred`,
/// `restarting`, `failed`, `unavailable`, `waiting_for_spotify_auth` or `disabled`.
#[must_use]
pub fn warm_player(w: &Value) -> String {
    let state = s(w, "/state");
    let port = w
        .get("port")
        .and_then(Value::as_u64)
        .map(|port| format!("127.0.0.1:{port}"));
    let port_text = port.clone().unwrap_or_else(|| "its client port".into());
    let pid = w.get("pid").and_then(Value::as_u64);
    let retry = w
        .get("retry_in_s")
        .and_then(Value::as_u64)
        .map(|secs| format!("; retrying in {secs} s"))
        .unwrap_or_default();
    let mut line = match state {
        "running" => {
            let mut facts: Vec<String> = pid.map(|pid| format!("pid {pid}")).into_iter().collect();
            facts.extend(port.clone());
            if let Some(refresh) = w.get("refresh_ms").and_then(Value::as_u64) {
                facts.push(format!("playback refresh every {}", millis(refresh)));
            }
            match w.get("serves_cli").and_then(Value::as_bool) {
                Some(true) => facts.push("serves spotify-cli".into()),
                Some(false) => facts.push("does not serve spotify-cli".into()),
                None => {}
            }
            if facts.is_empty() {
                "running".to_owned()
            } else {
                format!("running ({})", facts.join(", "))
            }
        }
        "starting" => match pid {
            Some(pid) => format!("starting (pid {pid}, waiting for it to take {port_text})"),
            None => "starting".to_owned(),
        },
        "not_serving" => format!(
            "not serving: the daemon's copy never got {port_text} (held by {}); it was stopped{retry}",
            port_owner(&w["port_owner"])
        ),
        "deferred" => {
            let since = w
                .get("since")
                .and_then(Value::as_str)
                .map(|since| format!(" since {since}"))
                .unwrap_or_default();
            format!(
                "deferred{since}: {port_text} is held by {}, which answers spotify-cli's commands; the daemon starts its own copy once it exits",
                port_owner(&w["port_owner"])
            )
        }
        "restarting" => {
            let exit = w
                .get("last_exit")
                .and_then(Value::as_str)
                .map(|exit| format!(" after it exited ({exit})"))
                .unwrap_or_default();
            format!("restarting{exit}{retry}")
        }
        "failed" | "unavailable" => match error_text(&w["error"]) {
            error if error.is_empty() => state.to_owned(),
            error => format!("{state}: {error}"),
        },
        "waiting_for_spotify_auth" => {
            "waiting for Spotify sign-in (run `spotify auth login`)".to_owned()
        }
        "disabled" => match w.get("reason").and_then(Value::as_str) {
            Some(reason) => format!("disabled ({reason})"),
            None => "disabled".to_owned(),
        },
        "" => "unknown".to_owned(),
        other => other.replace('_', " "),
    };
    // Notes that say more than the line: why a copy is not serving, where its log is.
    if !matches!(state, "deferred" | "not_serving")
        && let Some(note) = w.get("note").and_then(Value::as_str)
    {
        let _ = write!(line, "\n    {}", one_line(note));
    }
    line
}

/// `daemon status`.
#[must_use]
pub fn daemon_status(v: &Value) -> String {
    if v["running"] != Value::Bool(true) {
        let mut out = format!(
            "spotify-daemon is not running. Start it: spotify daemon start (launch agent installed: {})",
            v["launch_agent_installed"]
        );
        for line in v["log_tail"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            let _ = write!(out, "\n  log: {line}");
        }
        return out;
    }
    let mut out = format!(
        "spotify-daemon {} · pid {} · up {} s · {}",
        s(v, "/version"),
        v["pid"],
        v["uptime_s"],
        if v["launch_agent_installed"] == Value::Bool(true) {
            "launchd agent installed"
        } else {
            "not installed at login (spotify daemon install)"
        }
    );
    if let Some(spotify) = v.get("spotify").filter(|x| !x.is_null()) {
        let _ = write!(
            out,
            "\n  Spotify: {} {}",
            s(spotify, "/state"),
            one_line(spotify.get("track").and_then(Value::as_str).unwrap_or(""))
        );
    }
    let c = &v["counts"];
    let _ = write!(
        out,
        "\n  triggers: {} active · deliveries: {} pending, {} failed · managed queue: {}",
        c["active_triggers"], c["pending_deliveries"], c["failed_deliveries"], v["managed_queue"]
    );
    let _ = write!(
        out,
        "\n  warm spotify_player: {}",
        warm_player(&v["warm_spotify_player"])
    );
    match v.get("automation").and_then(Value::as_str) {
        Some("not_answering") => out.push_str(
            "\n  automation: Spotify is not answering (click Allow if macOS asks whether spotify-daemon may control Spotify)",
        ),
        Some("denied") => out.push_str(
            "\n  automation: denied (System Settings → Privacy & Security → Automation → spotify-daemon → Spotify)",
        ),
        Some(state) => {
            let _ = write!(out, "\n  automation: {state}");
        }
        None => {}
    }
    let _ = write!(
        out,
        "\n  readings: {} · Spotify notifications: {}",
        v["readings"], v["notifications"]
    );
    if let Some(error) = v.get("watch_error").filter(|e| !e.is_null()) {
        let _ = write!(
            out,
            "\n  watcher error: {} ({})",
            s(error, "/message"),
            s(error, "/code")
        );
    }
    let _ = write!(out, "\n  log: {}", s(v, "/log"));
    out
}
