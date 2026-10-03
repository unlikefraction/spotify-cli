//! `spotify` with no command: what is set up, what is missing, and a few things to try.
//!
//! Only fast local checks: files on disk, spotify_player's cached tokens, this home's saved
//! session, and the daemon's status over its local socket when it already runs (it is never
//! started or replaced here). No network. `spotify doctor` does the thorough, live checks.

use std::path::Path;
use std::time::Duration;

use serde_json::{Value, json};
use silicon_spotify_client::player::SpotifyPlayer;
use silicon_spotify_client::{Result, VERSION};

use crate::Ctx;
use crate::daemon::Found;

/// How long to wait for a running daemon's status.
const DAEMON_WAIT: Duration = Duration::from_millis(800);

fn check(name: &str, ok: bool, detail: Value, fix: Option<&str>, optional: bool) -> Value {
    json!({"check": name, "ok": ok, "detail": detail, "fix": if ok { Value::Null } else { json!(fix) }, "optional": optional})
}

/// Spotify.app in /Applications or ~/Applications.
fn spotify_app_installed() -> bool {
    Path::new("/Applications/Spotify.app").exists()
        || silicon_spotify_client::ipc::real_home()
            .is_some_and(|h| h.join("Applications/Spotify.app").exists())
}

/// The facts, gathered without the network, given what the daemon's socket said.
fn facts(ctx: &Ctx, found: &Found) -> Vec<Value> {
    let daemon = match found {
        Found::Running(status) => Some(status),
        _ => None,
    };
    let mut checks = Vec::new();
    let macos = cfg!(target_os = "macos");
    checks.push(check(
        "macos",
        macos,
        json!(std::env::consts::OS),
        Some("Spotify control needs macOS; login, config, docs and how work anywhere."),
        false,
    ));
    let app = spotify_app_installed();
    checks.push(check(
        "spotify_app",
        app,
        json!(app),
        Some("brew install --cask spotify (or: spotify setup --install-apps)"),
        false,
    ));
    let player = SpotifyPlayer::locate(ctx.config.spotify_player_binary.as_deref().map(Path::new))
        .map(|mut p| {
            p.cache_dir = ctx
                .config
                .spotify_player_cache_dir
                .as_deref()
                .map(Into::into);
            p
        });
    let signed_in = player.as_ref().is_ok_and(SpotifyPlayer::has_cached_token);
    checks.push(check(
        "spotify_player",
        player.is_ok(),
        json!(player.as_ref().ok().map(|p| &p.binary)),
        Some("spotify setup (installs it with Homebrew)"),
        false,
    ));
    checks.push(check(
        "spotify_signed_in",
        signed_in,
        json!(signed_in),
        Some("spotify auth login (search, lyrics, playlists and the library need it)"),
        false,
    ));
    let agent = silicon_spotify_client::ipc::real_home().is_some_and(|h| {
        h.join("Library/LaunchAgents")
            .join(format!("{}.plist", crate::daemon::LABEL))
            .is_file()
    });
    let version = daemon
        .and_then(|d| d.get("version"))
        .and_then(Value::as_str);
    let (detail, fix) = match found {
        Found::NotAnswering(code) => (
            json!({"running": true, "answering": false, "error": code, "starts_at_login": agent}),
            "spotify daemon restart (it runs but did not answer; spotify daemon logs shows why)",
        ),
        _ => (
            json!({"running": daemon.is_some(), "version": version, "starts_at_login": agent}),
            if agent {
                "spotify daemon start (commands also start it on demand)"
            } else {
                "spotify daemon install (runs it at login; commands also start it on demand)"
            },
        ),
    };
    checks.push(check("daemon", daemon.is_some(), detail, Some(fix), true));
    if let Some(state) = daemon
        .and_then(|d| d.get("automation"))
        .and_then(Value::as_str)
    {
        let ok = !matches!(state, "denied" | "not_answering");
        checks.push(check(
            "automation_permission",
            ok,
            json!(state),
            Some(if state == "denied" {
                "System Settings → Privacy & Security → Automation → spotify-daemon → Spotify"
            } else {
                "click Allow if macOS asks whether spotify-daemon may control Spotify"
            }),
            false,
        ));
    }
    let session = ctx
        .home
        .sessions()
        .ok()
        .and_then(|s| ctx.slot().ok().and_then(|key| s.slots.get(&key).cloned()));
    let ting = session
        .as_ref()
        .and_then(|s| s.session.ting.as_ref())
        .is_some_and(|t| t.subscribed);
    checks.push(check(
        "iam_login",
        session.is_some(),
        json!(session.as_ref().map(|s| &s.session.actor.public_id)),
        Some("spotify login '<SLT>' (see spotify login --help)"),
        true,
    ));
    if session.is_some() {
        checks.push(check(
            "ting_recipient",
            ting,
            json!(ting),
            Some("spotify ting register"),
            true,
        ));
    }
    checks
}

fn ok(checks: &[Value], name: &str) -> bool {
    checks
        .iter()
        .find(|c| c["check"] == name)
        .is_none_or(|c| c["ok"] == Value::Bool(true))
}

/// What to try: fixes for what is missing first, then the usual first steps.
fn suggestions(checks: &[Value], playing: bool) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    if !ok(checks, "spotify_app") || !ok(checks, "spotify_player") {
        out.push((
            "spotify setup".into(),
            "Install what is missing and start the daemon".into(),
        ));
    } else if !ok(checks, "spotify_signed_in") {
        out.push((
            "spotify auth login".into(),
            "Sign spotify_player in to Spotify (browser, once)".into(),
        ));
    }
    if !ok(checks, "automation_permission") {
        out.push((
            "spotify doctor".into(),
            "Why Spotify cannot be controlled, and the fix".into(),
        ));
    }
    out.push(if playing {
        (
            "spotify status".into(),
            "What is playing, position, time left".into(),
        )
    } else {
        (
            "spotify play --search 'arctic monkeys 505'".into(),
            "Search and play".into(),
        )
    });
    out.push(if playing {
        (
            "spotify lyrics".into(),
            "Lyrics of the song playing now".into(),
        )
    } else {
        (
            "spotify lyrics spotify:track:0BxE4FqsDD1Ot4YuBXwAPp".into(),
            "Lyrics of any song by URI (spotify search finds it)".into(),
        )
    });
    if ok(checks, "iam_login") {
        out.push((
            "spotify trigger add --remaining 30s --note 'wrap up'".into(),
            "Ting me 30 s before this song ends".into(),
        ));
    } else {
        out.push((
            "spotify trigger add --end --local".into(),
            "Record when this song ends (Ting needs spotify login)".into(),
        ));
    }
    out.push((
        "spotify how \"play my liked songs shuffled\"".into(),
        "Ask which command does something".into(),
    ));
    out.truncate(5);
    out
}

/// The orientation in words.
fn render(v: &Value) -> String {
    let mut out = format!(
        "spotify-cli {} — Spotify on this Mac from the terminal: play, lyrics, queue, playlists, Ting triggers.\n",
        v["version"].as_str().unwrap_or("")
    );
    if let Some(now) = v["now_playing"].as_str() {
        out.push_str(&format!("Now: {now}\n"));
    }
    out.push('\n');
    for c in v["checks"].as_array().into_iter().flatten() {
        let good = c["ok"] == Value::Bool(true);
        let mark = if good {
            "✓"
        } else if c["optional"] == Value::Bool(true) {
            "·"
        } else {
            "✗"
        };
        let label = match c["check"].as_str().unwrap_or("") {
            "macos" => "macOS".to_owned(),
            "spotify_app" => "Spotify.app installed".to_owned(),
            "spotify_player" => "spotify_player installed".to_owned(),
            "spotify_signed_in" => "spotify_player signed in to Spotify".to_owned(),
            "daemon" => match (c.pointer("/detail/version").and_then(Value::as_str), good) {
                (Some(version), true) => format!("spotify-daemon {version} running"),
                _ if c.pointer("/detail/answering") == Some(&Value::Bool(false)) => {
                    "spotify-daemon not answering".to_owned()
                }
                _ => "spotify-daemon not running".to_owned(),
            },
            "automation_permission" => format!(
                "permission to control Spotify ({})",
                c["detail"].as_str().unwrap_or("?").replace('_', " ")
            ),
            "iam_login" => match c["detail"].as_str() {
                Some(who) => format!("logged in as {who}"),
                None => "not logged in (only triggers need it)".to_owned(),
            },
            "ting_recipient" => "registered as a Ting recipient".to_owned(),
            other => other.to_owned(),
        };
        out.push_str(&format!("  {mark} {label}"));
        if let Some(fix) = c["fix"].as_str() {
            out.push_str(&format!(" → {fix}"));
        }
        out.push('\n');
    }
    out.push_str("\nTry:\n");
    let tries = v["try"].as_array().cloned().unwrap_or_default();
    let width = tries
        .iter()
        .map(|t| t["command"].as_str().unwrap_or("").chars().count())
        .max()
        .unwrap_or(0);
    for t in &tries {
        out.push_str(&format!(
            "  {:<width$}   {}\n",
            t["command"].as_str().unwrap_or(""),
            t["description"].as_str().unwrap_or("")
        ));
    }
    out.push_str(
        "\nEverything: spotify --help (by goal) · spotify <command> --help · spotify docs · spotify commands --json",
    );
    out
}

/// `spotify` with no command.
///
/// # Errors
/// None in practice: every check degrades to "missing".
pub async fn orient(ctx: &Ctx) -> Result<()> {
    let found = if cfg!(target_os = "macos") {
        crate::daemon::status_if_running(DAEMON_WAIT).await
    } else {
        Found::NotRunning
    };
    let checks = facts(ctx, &found);
    let daemon = match &found {
        Found::Running(status) => Some(status),
        _ => None,
    };
    let now = daemon.and_then(|d| {
        let state = d.pointer("/spotify/state").and_then(Value::as_str)?;
        let track = d.pointer("/spotify/track").and_then(Value::as_str)?;
        (state == "playing" || state == "paused").then(|| {
            format!(
                "{} {}",
                if state == "playing" { "▶" } else { "⏸" },
                crate::render::one_line(track)
            )
        })
    });
    let ready = checks
        .iter()
        .all(|c| c["ok"] == Value::Bool(true) || c["optional"] == Value::Bool(true));
    let tries: Vec<Value> = suggestions(&checks, now.is_some())
        .into_iter()
        .map(|(command, description)| json!({"command": command, "description": description}))
        .collect();
    let value = json!({
        "version": VERSION,
        "ready": ready,
        "now_playing": now,
        "checks": checks,
        "try": tries,
        "help": "spotify --help lists every command by goal; spotify how \"<question>\" finds one; spotify doctor checks everything live.",
    });
    ctx.emit(&value, render);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, ok: bool, optional: bool) -> Value {
        check(name, ok, Value::Null, Some("fix it"), optional)
    }

    #[test]
    fn missing_pieces_come_first_in_what_to_try() {
        let checks = [
            c("spotify_app", true, false),
            c("spotify_player", true, false),
            c("spotify_signed_in", false, false),
            c("iam_login", false, true),
        ];
        let tries = suggestions(&checks, false);
        assert_eq!(tries[0].0, "spotify auth login");
        assert!(tries.iter().any(|(c, _)| c.contains("--local")));
        assert_eq!(tries.len(), 5);
        let all_good = [c("spotify_app", true, false), c("iam_login", true, true)];
        let tries = suggestions(&all_good, true);
        assert_eq!(tries[0].0, "spotify status");
        assert!(tries.iter().any(|(c, _)| c.contains("--remaining 30s")));
        // Every suggestion is a command line the grammar takes.
        for (command, _) in suggestions(&checks, false)
            .into_iter()
            .chain(suggestions(&all_good, true))
        {
            let words = crate::shell::spotify_commands(&command);
            assert_eq!(words.len(), 1, "{command}");
            crate::args::command()
                .try_get_matches_from(&words[0])
                .unwrap_or_else(|e| panic!("{command}: {e}"));
        }
    }

    #[test]
    fn rendering_marks_required_and_optional_checks() {
        let value = json!({"version": "0.1.5", "now_playing": "▶ 505 — Arctic Monkeys",
            "checks": [c("spotify_app", true, false), c("spotify_signed_in", false, false), c("iam_login", false, true)],
            "try": [{"command": "spotify status", "description": "What is playing"}]});
        let text = render(&value);
        assert!(text.contains("Now: ▶ 505 — Arctic Monkeys"), "{text}");
        assert!(text.contains("  ✓ Spotify.app installed\n"), "{text}");
        assert!(
            text.contains("  ✗ spotify_player signed in to Spotify → fix it\n"),
            "{text}"
        );
        assert!(
            text.contains("  · not logged in (only triggers need it) → fix it\n"),
            "{text}"
        );
        assert!(
            text.contains("Try:\n  spotify status   What is playing\n"),
            "{text}"
        );
    }
}
