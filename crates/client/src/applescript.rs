//! AppleScript for Spotify.app: script builders, output parsing and error classification.
//!
//! Every script checks `application "Spotify" is running` first, so reading state never launches
//! Spotify as a side effect. Numbers cross the boundary as integers (milliseconds) so the system
//! locale's decimal separator can never corrupt them.

use std::io::Read as _;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::model::{Playback, PlayerState, Track, now_rfc3339};
use crate::{Error, Result};

/// Field separator in script output (ASCII unit separator, never present in titles).
pub const SEP: char = '\u{1f}';

/// Executes AppleScript source and returns its result as text.
///
/// The daemon implements this with an in-process `NSAppleScript` on the main thread; everything
/// else can use [`Osascript`].
pub trait Runner: Send + Sync {
    /// Runs `source` and returns the script's result coerced to text.
    ///
    /// # Errors
    /// Returns a classified [`Error`] (see [`classify`]).
    fn run(&self, source: &str) -> Result<String>;
}

/// Runs AppleScript through `/usr/bin/osascript` with a timeout.
#[derive(Clone, Debug)]
pub struct Osascript {
    /// Kill the script after this long.
    pub timeout: Duration,
}

impl Default for Osascript {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(10),
        }
    }
}

impl Runner for Osascript {
    fn run(&self, source: &str) -> Result<String> {
        if !cfg!(target_os = "macos") {
            return Err(Error::platform_unsupported("Controlling Spotify.app"));
        }
        let mut child = Command::new("/usr/bin/osascript")
            .arg("-e")
            .arg(source)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                Error::new(
                    "applescript_failed",
                    format!("/usr/bin/osascript could not start: {error}."),
                    "osascript ships with macOS; check that /usr/bin/osascript exists and is executable.",
                )
            })?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    let mut stdout = String::new();
                    let mut stderr = String::new();
                    if let Some(mut out) = child.stdout.take() {
                        let _ = out.read_to_string(&mut stdout);
                    }
                    if let Some(mut err) = child.stderr.take() {
                        let _ = err.read_to_string(&mut stderr);
                    }
                    if status.success() {
                        return Ok(stdout.trim_end_matches('\n').to_owned());
                    }
                    return Err(classify(stderr.trim(), error_number(&stderr)));
                }
                Ok(None) if started.elapsed() > self.timeout => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Error::new(
                        "timeout",
                        format!("Spotify.app did not answer AppleScript within {:?}.", self.timeout),
                        "Spotify may be frozen or showing a modal dialog. Bring it to the front, or quit and reopen it, then retry.",
                    )
                    .retryable());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                Err(error) => {
                    return Err(Error::new(
                        "applescript_failed",
                        format!("Waiting for osascript failed: {error}."),
                        "Retry; if it persists run `spotify doctor`.",
                    ));
                }
            }
        }
    }
}

/// Extracts the `(-NNNN)` AppleScript error number from osascript's stderr.
#[must_use]
pub fn error_number(stderr: &str) -> Option<i64> {
    let start = stderr.rfind("(-")?;
    let end = stderr[start..].find(')')? + start;
    stderr[start + 1..end].parse().ok()
}

/// Maps an AppleScript failure to a structured error.
#[must_use]
pub fn classify(message: &str, number: Option<i64>) -> Error {
    let details = serde_json::json!({"applescript_error": number, "message": message});
    match number {
        Some(-1743) => Error::new(
            "automation_permission_denied",
            "macOS blocked this process from controlling Spotify (Automation permission is off or was never granted).",
            "Open System Settings → Privacy & Security → Automation, find spotify-daemon (or your terminal) and enable Spotify. If it is not listed, run `tccutil reset AppleEvents com.unlikefraction.spotify-daemon` (if macOS answers that there is no such bundle identifier, `tccutil reset AppleEvents` works but makes every app ask again) and retry so macOS asks. `spotify doctor` re-checks.",
        )
        .with_details(details),
        Some(-600 | -609) => Error::new(
            "spotify_not_running",
            "Spotify.app quit while the command was running.",
            "Start it with `spotify launch` (or open Spotify), then retry.",
        )
        .retryable()
        .with_details(details),
        Some(-1728) => Error::nothing_playing().with_details(details),
        Some(-1712) => Error::new(
            "timeout",
            "Spotify.app did not answer the Apple Event in time.",
            "Spotify may be busy or showing a dialog. Retry in a moment.",
        )
        .retryable()
        .with_details(details),
        Some(-2741 | -2740 | -2753) => Error::internal(format!(
            "An AppleScript generated by spotify-cli failed to compile: {message}"
        ))
        .with_details(details),
        Some(-10810 | -10814) => Error::new(
            "spotify_not_installed",
            "macOS could not find or launch Spotify.app.",
            "Install Spotify from https://www.spotify.com/download/mac/ (or `brew install --cask spotify`), open it once and sign in.",
        )
        .with_details(details),
        _ => Error::new(
            "applescript_failed",
            format!("Spotify.app rejected the AppleScript command: {message}"),
            "Retry once. If it keeps failing run `spotify doctor`, and report it with `spotify report`.",
        )
        .with_details(details),
    }
}

/// Quotes text as an AppleScript string literal.
#[must_use]
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' | '\r' => out.push(' '),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Wraps statements so they run only while Spotify is running; the script returns `ok` or
/// `not_running`.
#[must_use]
pub fn guarded(body: &str) -> String {
    format!(
        "if application \"Spotify\" is running then\n\twith timeout of 8 seconds\n\ttell application \"Spotify\"\n{body}\n\tend tell\n\tend timeout\n\treturn \"ok\"\nelse\n\treturn \"not_running\"\nend if"
    )
}

/// `play` (resume).
#[must_use]
pub fn play() -> String {
    guarded("\t\tplay")
}

/// `pause`.
#[must_use]
pub fn pause() -> String {
    guarded("\t\tpause")
}

/// `playpause`.
#[must_use]
pub fn toggle() -> String {
    guarded("\t\tplaypause")
}

/// `next track`.
#[must_use]
pub fn next() -> String {
    guarded("\t\tnext track")
}

/// `previous track`.
#[must_use]
pub fn previous() -> String {
    guarded("\t\tprevious track")
}

/// `play track <uri> [in context <context>]`.
#[must_use]
pub fn play_uri(uri: &str, context: Option<&str>) -> String {
    match context {
        Some(context) => guarded(&format!(
            "\t\tplay track {} in context {}",
            quote(uri),
            quote(context)
        )),
        None => guarded(&format!("\t\tplay track {}", quote(uri))),
    }
}

/// `set player position to <seconds>`.
#[must_use]
pub fn seek(position_ms: u64) -> String {
    let seconds = position_ms / 1000;
    let millis = position_ms % 1000;
    guarded(&format!("\t\tset player position to {seconds}.{millis:03}"))
}

/// `set sound volume to <0-100>`, compensating for Spotify.app's rounding.
///
/// Spotify.app keeps the level at 16-bit precision and reads it back rounded down, so after
/// `set sound volume to 64` it reports 63 (every level except the multiples of 20). Setting one
/// more then reads back the requested level. The script waits (up to 0.3 s) for the first set to
/// show before judging it, because Spotify.app can still report the old level right after a set.
/// 19, 39, 59, 79 and 99 cannot be reached this way; they land one above (see
/// [`volume_reached`]).
#[must_use]
pub fn volume(percent: u8) -> String {
    let percent = percent.min(100);
    if percent == 100 {
        return guarded("\t\tset sound volume to 100");
    }
    guarded(&format!(
        "\t\tset xWas to sound volume
\t\tset sound volume to {percent}
\t\trepeat 10 times
\t\t\tif sound volume is not xWas then exit repeat
\t\t\tdelay 0.03
\t\tend repeat
\t\tif sound volume < {percent} then set sound volume to {}",
        percent + 1
    ))
}

/// Whether Spotify.app reporting volume `read` means a request for `requested` took effect:
/// exactly, except for the levels [`volume`] cannot reach through AppleScript, which land one
/// above.
#[must_use]
pub fn volume_reached(requested: u8, read: u8) -> bool {
    read == requested || (requested % 20 == 19 && read == requested.saturating_add(1))
}

/// `set shuffling to <bool>`.
#[must_use]
pub fn shuffle(on: bool) -> String {
    guarded(&format!("\t\tset shuffling to {on}"))
}

/// `set repeating to <bool>`.
#[must_use]
pub fn repeat(on: bool) -> String {
    guarded(&format!("\t\tset repeating to {on}"))
}

/// Whether Spotify.app is running (never launches it).
#[must_use]
pub fn is_running() -> &'static str {
    "return (application \"Spotify\" is running) as string"
}

/// Reads the full player state in one Apple Event round.
///
/// Output: `not_running` | `no_track␟state␟volume␟shuffling␟repeating` |
/// `ok␟state␟volume␟shuffling␟repeating␟position_ms␟uri␟name␟artist␟album␟album_artist␟duration_ms␟track_number␟disc_number␟popularity␟artwork_url␟spotify_url␟shuffle_allowed␟repeat_allowed`.
#[must_use]
pub fn status() -> String {
    let fields = [
        ("xName", "name of t"),
        ("xArtist", "artist of t"),
        ("xAlbum", "album of t"),
        ("xAlbumArtist", "album artist of t"),
        ("xDuration", "duration of t"),
        ("xTrackNo", "track number of t"),
        ("xDiscNo", "disc number of t"),
        ("xPopularity", "popularity of t"),
        ("xArtwork", "artwork url of t"),
        ("xShareUrl", "spotify url of t"),
    ];
    let mut reads = String::new();
    let mut joined = String::new();
    for (var, expr) in fields {
        reads.push_str(&format!(
            "\t\tset {var} to \"\"\n\t\ttry\n\t\t\tset {var} to ({expr}) as string\n\t\tend try\n"
        ));
        joined.push_str(&format!(" & xSep & {var}"));
    }
    format!(
        "if application \"Spotify\" is not running then return \"not_running\"
with timeout of 5 seconds
tell application \"Spotify\"
\tset xSep to character id 31
\tset xState to (player state as string)
\tset xVolume to \"\"
\ttry
\t\tset xVolume to (sound volume) as string
\tend try
\tset xShuffle to \"\"
\ttry
\t\tset xShuffle to (shuffling) as string
\tend try
\tset xRepeat to \"\"
\ttry
\t\tset xRepeat to (repeating) as string
\tend try
\tset xShufOk to \"\"
\ttry
\t\tset xShufOk to (shuffling enabled) as string
\tend try
\tset xRepOk to \"\"
\ttry
\t\tset xRepOk to (repeating enabled) as string
\tend try
\tset xHead to xState & xSep & xVolume & xSep & xShuffle & xSep & xRepeat
\ttry
\t\tset t to current track
\t\tset xTrackId to (id of t) as string
\ton error
\t\treturn \"no_track\" & xSep & xHead
\tend try
\tif xTrackId is \"\" then return \"no_track\" & xSep & xHead
\tset xPosition to \"0\"
\ttry
\t\tset xPosition to (round ((player position) * 1000) rounding down) as string
\tend try
{reads}\treturn \"ok\" & xSep & xHead & xSep & xPosition & xSep & xTrackId{joined} & xSep & xShufOk & xSep & xRepOk
end tell
end timeout"
    )
}

/// Parses [`status`] output.
///
/// # Errors
/// Returns `applescript_failed` when the output is not in the expected shape.
pub fn parse_status(output: &str) -> Result<Playback> {
    let fields: Vec<&str> = output.trim_end().split(SEP).collect();
    let head = |fields: &[&str]| -> (PlayerState, Option<u8>, Option<bool>, Option<bool>) {
        (
            PlayerState::from_applescript(fields.first().copied().unwrap_or("stopped")),
            fields.get(1).and_then(|v| v.trim().parse().ok()),
            fields.get(2).and_then(|v| parse_bool(v)),
            fields.get(3).and_then(|v| parse_bool(v)),
        )
    };
    match fields.first().copied() {
        Some("not_running") => Ok(Playback::empty(PlayerState::NotRunning)),
        Some("no_track") => {
            let (state, volume, shuffling, repeating) = head(&fields[1..]);
            let mut playback = Playback::empty(if state == PlayerState::Playing {
                PlayerState::Playing
            } else {
                PlayerState::Stopped
            });
            playback.volume = volume;
            playback.shuffling = shuffling;
            playback.repeating = repeating;
            Ok(playback)
        }
        Some("ok") if fields.len() >= 17 => {
            let (state, volume, shuffling, repeating) = head(&fields[1..5]);
            let number = |i: usize| fields.get(i).and_then(|v| v.trim().parse::<u64>().ok());
            let text = |i: usize| fields.get(i).map(|v| (*v).to_owned()).unwrap_or_default();
            let mut track = Track {
                uri: text(6),
                name: text(7),
                artist: text(8),
                album: text(9),
                album_artist: text(10),
                duration_ms: number(11).unwrap_or(0),
                track_number: number(12).and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0),
                disc_number: number(13).and_then(|v| u32::try_from(v).ok()).filter(|v| *v > 0),
                popularity: number(14).and_then(|v| u32::try_from(v).ok()),
                artwork_url: text(15),
                ..Track::default()
            };
            let share = text(16);
            if share.starts_with("https://") {
                track.url = share;
            }
            track.finish();
            let shuffle_allowed = fields.get(17).and_then(|v| parse_bool(v));
            let repeat_allowed = fields.get(18).and_then(|v| parse_bool(v));
            let mut playback = Playback {
                state,
                position_ms: number(5).unwrap_or(0),
                track: Some(track),
                volume,
                shuffling,
                repeating,
                shuffle_allowed,
                repeat_allowed,
                observed_at: now_rfc3339(),
                ..Playback::empty(state)
            };
            playback.finish();
            Ok(playback)
        }
        _ => Err(Error::new(
            "applescript_failed",
            "Spotify.app returned player state in an unexpected shape.",
            "Your Spotify version may have changed its AppleScript dictionary. Run `spotify doctor` and report it with `spotify report`.",
        )
        .with_details(serde_json::json!({"output": crate::model::truncate(output, 400)}))),
    }
}

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Interprets a guarded command's result.
///
/// # Errors
/// Returns `spotify_not_running` when the guard reported Spotify is not running.
pub fn expect_ok(output: &str) -> Result<()> {
    match output.trim() {
        "ok" | "" => Ok(()),
        "not_running" => Err(not_running()),
        other => Err(Error::new(
            "applescript_failed",
            format!("Unexpected AppleScript result `{other}`."),
            "Report it with `spotify report`.",
        )),
    }
}

/// `spotify_not_running`.
#[must_use]
pub fn not_running() -> Error {
    Error::new(
        "spotify_not_running",
        "Spotify.app is not running, so there is nothing to control.",
        "Run `spotify launch` (starts Spotify in the background), then retry. Set `spotify config set '{\"launch_spotify\": true}'` to launch it automatically.",
    )
    .retryable()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quotes_literals() {
        assert_eq!(quote(r#"say "hi" \ now"#), r#""say \"hi\" \\ now""#);
    }

    #[test]
    fn parses_full_status() {
        let s = SEP;
        let output = format!(
            "ok{s}playing{s}66{s}false{s}true{s}12345{s}spotify:track:039NYmSMIO7g2HCeHFXwht{s}Make You Love Me{s}Akhil Sahni{s}Make You Love Me{s}Akhil Sahni{s}259826{s}1{s}1{s}30{s}https://i.scdn.co/image/x{s}spotify:track:039NYmSMIO7g2HCeHFXwht{s}false{s}true"
        );
        let playback = parse_status(&output).expect("parses");
        assert_eq!(playback.state, PlayerState::Playing);
        assert_eq!(playback.volume, Some(66));
        assert_eq!(playback.repeating, Some(true));
        assert_eq!(playback.position_ms, 12_345);
        let track = playback.track.expect("track");
        assert_eq!(track.id, "039NYmSMIO7g2HCeHFXwht");
        assert_eq!(track.duration, "4:19");
        assert_eq!(
            track.url,
            "https://open.spotify.com/track/039NYmSMIO7g2HCeHFXwht"
        );
        assert_eq!(playback.remaining_ms, 259_826 - 12_345);
        assert_eq!(playback.shuffle_allowed, Some(false));
        assert_eq!(playback.repeat_allowed, Some(true));
    }

    #[test]
    fn parses_edge_states() {
        assert_eq!(
            parse_status("not_running").expect("nr").state,
            PlayerState::NotRunning
        );
        let s = SEP;
        let stopped =
            parse_status(&format!("no_track{s}stopped{s}50{s}false{s}false")).expect("stopped");
        assert_eq!(stopped.state, PlayerState::Stopped);
        assert!(stopped.track.is_none());
        assert!(parse_status("garbage").is_err());
    }

    #[test]
    fn classifies_errors() {
        let stderr =
            "38:40: execution error: Not authorized to send Apple events to Spotify. (-1743)";
        assert_eq!(error_number(stderr), Some(-1743));
        assert_eq!(
            classify(stderr, error_number(stderr)).code,
            "automation_permission_denied"
        );
        assert_eq!(classify("x", Some(-1728)).code, "nothing_playing");
    }

    #[test]
    fn seek_script_uses_dot_decimal() {
        assert!(seek(90_250).contains("set player position to 90.250"));
    }

    #[test]
    fn volume_script_compensates_for_rounding_down() {
        // Spotify.app 1.2 reads `set sound volume to 64` back as 63; setting 65 reads back 64.
        let script = volume(64);
        assert!(script.contains("set sound volume to 64\n"), "{script}");
        assert!(
            script.contains("if sound volume < 64 then set sound volume to 65"),
            "{script}"
        );
        assert!(!volume(100).contains("101"));
        assert!(volume(0).contains("set sound volume to 0\n"));
        assert!(volume_reached(64, 64));
        assert!(!volume_reached(64, 63), "one below is the bug, not success");
        assert!(!volume_reached(64, 65));
        assert!(volume_reached(59, 60), "59 is unreachable and lands on 60");
        assert!(!volume_reached(59, 58));
    }
}
