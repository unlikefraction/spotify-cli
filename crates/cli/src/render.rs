//! Human renderings. JSON output never goes through here.

use std::fmt::Write as _;

use serde_json::Value;

fn s<'a>(v: &'a Value, pointer: &str) -> &'a str {
    v.pointer(pointer).and_then(Value::as_str).unwrap_or("")
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
        "{} {} — {}",
        state_icon(state),
        s(p, "/track/name"),
        s(p, "/track/artist")
    );
    let album = s(p, "/track/album");
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
        silicon_spotify_client::timing::clock(remaining)
    );
    let mut flags = Vec::new();
    if let Some(v) = p.get("volume").and_then(Value::as_u64) {
        flags.push(format!("volume {v}%"));
    }
    if p.get("shuffling").and_then(Value::as_bool) == Some(true) {
        flags.push("shuffle".into());
    }
    match p.pointer("/web/repeat_state").and_then(Value::as_str) {
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
        flags.push(format!("on {device}"));
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
    if v.get("source").and_then(Value::as_str) == Some("managed_queue") {
        return format!(
            "▶ Next from the managed queue: {} ({} left in queue)",
            v.pointer("/playing/name")
                .and_then(Value::as_str)
                .unwrap_or_else(|| s(v, "/playing/uri")),
            v["queue_remaining"]
        );
    }
    let mut out = playback(&v["playback"]);
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
    out
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
    let mut out = s(item, "/name").to_owned();
    let by = names(item);
    if !by.is_empty() {
        let _ = write!(out, " — {by}");
    }
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
    let mut out = format!("{}\n  {}", s(item, "/name"), s(item, "/uri"));
    let genres: Vec<&str> = v["genres"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
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
    let related: Vec<&str> = v["related_artists"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| a.get("name").and_then(Value::as_str))
        .take(10)
        .collect();
    if !related.is_empty() {
        let _ = writeln!(out, "Related: {}", related.join(", "));
    }
    out
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
            "{} — {}\n  {}",
            s(item, "/name"),
            names(item),
            s(item, "/uri")
        );
        if let Some(album) = item.get("album").and_then(Value::as_str) {
            let _ = write!(out, "\n  album: {album}");
        }
        if let Some(d) = item.get("duration").and_then(Value::as_str) {
            let _ = write!(out, "\n  length: {d}");
        }
        if let Some(r) = item.get("release_date").and_then(Value::as_str) {
            let _ = write!(out, "\n  released: {r}");
        }
        return out;
    }
    let t = &v["track"];
    let mut out = format!(
        "{} — {}\n  album: {}",
        s(t, "/name"),
        s(t, "/artist"),
        s(t, "/album")
    );
    if let Some(date) = v.pointer("/album/release_date").and_then(Value::as_str) {
        let _ = write!(out, " ({date})");
    }
    let _ = write!(
        out,
        "\n  length: {}  ·  at {} (-{})",
        s(t, "/duration"),
        s(v, "/playback/position"),
        silicon_spotify_client::timing::clock(
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
    let _ = write!(out, "\n  {}\n  {}", s(t, "/uri"), s(t, "/url"));
    out
}

fn item_line(item: &Value) -> String {
    let by = names(item);
    let mut line = s(item, "/name").to_owned();
    if !by.is_empty() {
        let _ = write!(line, " — {by}");
    }
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
    out.push_str("Play one: spotify play <uri> · queue it: spotify queue add <uri>");
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
                s(d, "/name"),
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
            let label = item.get("name").and_then(Value::as_str).map_or_else(
                || s(item, "/uri").to_owned(),
                |n| {
                    format!(
                        "{n} — {}",
                        item.get("by").and_then(Value::as_str).unwrap_or("")
                    )
                },
            );
            let _ = writeln!(out, "  {:>2}. {label}  [{}]", index + 1, s(item, "/id"));
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
            let _ = writeln!(out, "  {:>2}. {}", index + 1, s(item, "/name"));
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
    out
}

fn playlist_show(v: &Value, playlist: &Value) -> String {
    let mut out = format!(
        "{} — {}, {}\n  {}\n",
        s(playlist, "/name"),
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
        "playlist.create" => format!("Created playlist {} ({}).", s(v, "/name"), s(v, "/uri")),
        "playlist.delete" => format!("{}\n{}", s(v, "/message"), s(v, "/note")),
        "playlist.fork" => {
            let mut out = match v.get("uri").and_then(Value::as_str) {
                Some(uri) => {
                    let name = v
                        .get("name")
                        .and_then(Value::as_str)
                        .map(|n| format!("'{n}' "))
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
                v.pointer("/now_playing/name")
                    .and_then(Value::as_str)
                    .unwrap_or("the current song")
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
            "\n      {track} at {}",
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
            spotify.get("track").and_then(Value::as_str).unwrap_or("")
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
        s(v, "/warm_spotify_player/state")
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

#[cfg(test)]
mod tests {
    use serde_json::json;
    use silicon_spotify_client::model::item_view;

    use super::*;

    fn song(id: &str, name: &str, secs: u64) -> Value {
        json!({"id": id, "name": name, "artists": [{"id": "7Ln80lUS6He07XvHI8qqHH", "name": "Arctic Monkeys"}],
            "album": {"id": "78bpIziExqiI9qztvNFlQu", "name": "AM"}, "duration": {"secs": secs, "nanos": 0}})
    }

    #[test]
    fn albums_and_artists_render_their_details() {
        // What `spotify track spotify:album:78bpIziExqiI9qztvNFlQu` gets: the daemon's `raw`
        // (spotify_player's output) normalized by `item_view`.
        let raw = json!({"album": {"id": "78bpIziExqiI9qztvNFlQu", "release_date": "2013-09-09", "name": "AM",
            "artists": [{"id": "7Ln80lUS6He07XvHI8qqHH", "name": "Arctic Monkeys"}], "typ": "album"},
            "tracks": [song("5FVd6KXrgO9B3JPmC8OPst", "Do I Wanna Know?", 272)]});
        let text = track_view("album", &raw);
        assert!(text.starts_with("AM — Arctic Monkeys\n  album · released 2013-09-09 · 1 track · 4:32\n  spotify:album:78bpIziExqiI9qztvNFlQu\n"), "{text}");
        assert!(
            text.contains("1. Do I Wanna Know? — Arctic Monkeys (4:32)"),
            "{text}"
        );
        let raw = json!({"artist": {"id": "7Ln80lUS6He07XvHI8qqHH", "name": "Arctic Monkeys"},
            "top_tracks": [song("5XeFesFbtLpXzIVDNQP22n", "I Wanna Be Yours", 184)],
            "albums": [{"id": "2rkuPRtC7rZlcsOCwTmdpF", "release_date": "2005-10-17", "name": "Dancefloor",
                "artists": [{"id": "7Ln80lUS6He07XvHI8qqHH", "name": "Arctic Monkeys"}], "typ": "single"}],
            "related_artists": [{"id": "77SW9BnxLY8rJ0RciFqkHh", "name": "The Neighbourhood"}]});
        let text = track_view("artist", &raw);
        assert!(
            text.starts_with("Arctic Monkeys\n  spotify:artist:7Ln80lUS6He07XvHI8qqHH\n"),
            "{text}"
        );
        assert!(
            text.contains("Top tracks (1):\n    1. I Wanna Be Yours"),
            "{text}"
        );
        assert!(
            text.contains("1. Dancefloor — Arctic Monkeys (2005-10-17, single)\n"),
            "{text}"
        );
        assert!(text.contains("Related: The Neighbourhood"), "{text}");
    }

    fn track_view(kind: &str, raw: &Value) -> String {
        let mut value = json!({"item": null, "raw": raw});
        if let (Some(object), Value::Object(view)) = (value.as_object_mut(), item_view(kind, raw)) {
            object.extend(view);
        }
        track(&value)
    }

    #[test]
    fn playlist_counts_are_pluralized() {
        let one = json!({"playlist": {"name": "Mix", "uri": "spotify:playlist:x"}, "track_count": 1, "duration": "3:00", "tracks": []});
        assert!(playlist("playlist.show", &one).starts_with("Mix — 1 track, 3:00"));
        let two = json!({"playlist": {"name": "Mix", "uri": "spotify:playlist:x"}, "track_count": 2, "duration": "6:00", "tracks": []});
        assert!(playlist("playlist.show", &two).starts_with("Mix — 2 tracks, 6:00"));
    }

    #[test]
    fn forks_show_the_new_playlist() {
        let forked = json!({"forked": true, "from": "37i9dQZF1DXcBWIGoYBM5M", "id": "3PgK3VZ2qzkM0B12w8hHnf",
            "uri": "spotify:playlist:3PgK3VZ2qzkM0B12w8hHnf", "name": "Focus (mine)", "message": "..."});
        assert_eq!(
            playlist("playlist.fork", &forked),
            "Forked spotify:playlist:37i9dQZF1DXcBWIGoYBM5M into 'Focus (mine)' (spotify:playlist:3PgK3VZ2qzkM0B12w8hHnf)."
        );
        let old = json!({"forked": true, "from": "x", "message": "Forked playlist."});
        assert_eq!(playlist("playlist.fork", &old), "Forked playlist.");
    }
}
