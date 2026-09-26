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

/// `spotify track`.
#[must_use]
pub fn track(v: &Value) -> String {
    if let Some(playlist) = v.get("playlist").filter(|p| !p.is_null()) {
        return playlist_show(v, playlist);
    }
    if let Some(item) = v.get("item").filter(|i| !i.is_null()) {
        let mut out = format!(
            "{} — {}\n  {}",
            s(item, "/name"),
            item["by"]
                .as_array()
                .map(|b| b
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(", "))
                .unwrap_or_default(),
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
    let by = item["by"]
        .as_array()
        .map(|b| {
            b.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
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
        "{} — {} tracks, {}\n  {}\n",
        s(playlist, "/name"),
        v["track_count"],
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
        Some("needs_consent" | "stalled") => out.push_str(
            "\n  automation: waiting for Allow in the macOS dialog (… wants access to control \"Spotify\")",
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
