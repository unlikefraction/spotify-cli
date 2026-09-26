//! A thin, stateless wrapper around the `spotify_player` CLI.
//!
//! `spotify_player` talks to the Spotify Web API. Its CLI forwards each command over UDP to a
//! running `spotify_player` instance (the daemon keeps one alive headless) or, when none runs,
//! starts a temporary client, which takes about 1.5 s instead of ~20 ms.
//!
//! Important: with a running instance, playback commands are handled asynchronously and exit 0
//! before (or even when) nothing happened. Never treat exit 0 as proof; [`crate::control`]
//! verifies every playback effect against Spotify.app.

use std::borrow::Cow;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::{Error, Result};

/// Where `spotify_player` lives and which config/cache folders it uses.
#[derive(Clone, Debug)]
pub struct SpotifyPlayer {
    /// The `spotify_player` executable.
    pub binary: PathBuf,
    /// `-c`: config folder (default `~/.config/spotify-player`).
    pub config_dir: Option<PathBuf>,
    /// `-C`: cache folder (default `~/.cache/spotify-player`); holds the Web API tokens.
    pub cache_dir: Option<PathBuf>,
    /// Kill a command after this long.
    pub timeout: Duration,
}

/// Captured output of one invocation.
#[derive(Clone, Debug)]
pub struct Output {
    /// Trimmed stdout.
    pub stdout: String,
    /// Trimmed stderr.
    pub stderr: String,
    /// Wall time.
    pub elapsed: Duration,
}

/// `spotify_player search` returns at most this many results per kind and has no option for
/// more, so larger search limits are rejected rather than silently returning fewer.
pub const SEARCH_MAX_PER_KIND: u32 = 10;

/// Where `spotify_player` is searched for when it is not on PATH (launchd and Stemcell run with a
/// minimal PATH).
pub const SEARCH_PATHS: &[&str] = &[
    "/opt/homebrew/bin/spotify_player",
    "/usr/local/bin/spotify_player",
    "/home/linuxbrew/.linuxbrew/bin/spotify_player",
];

impl SpotifyPlayer {
    /// Finds the binary: explicit path, then PATH, then [`SEARCH_PATHS`] and `~/.cargo/bin`.
    ///
    /// # Errors
    /// Returns `spotify_player_missing` with the install command.
    pub fn locate(explicit: Option<&Path>) -> Result<Self> {
        let found = match explicit {
            Some(path) if path.is_file() => Some(path.to_path_buf()),
            Some(path) => {
                return Err(Error::new(
                    "spotify_player_missing",
                    format!(
                        "The configured spotify_player binary {} does not exist.",
                        path.display()
                    ),
                    "Fix it with `spotify config set '{\"spotify_player_binary\": \"/path/to/spotify_player\"}'`, or remove the key to auto-detect.",
                ));
            }
            None => find_on_path("spotify_player")
                .or_else(|| SEARCH_PATHS.iter().map(PathBuf::from).find(|p| p.is_file()))
                .or_else(|| {
                    std::env::var_os("HOME")
                        .map(|home| PathBuf::from(home).join(".cargo/bin/spotify_player"))
                        .filter(|p| p.is_file())
                }),
        };
        let binary = found.ok_or_else(missing)?;
        Ok(Self {
            binary,
            config_dir: None,
            cache_dir: None,
            timeout: Duration::from_secs(20),
        })
    }

    /// The cache folder in use.
    #[must_use]
    pub fn cache_folder(&self) -> Option<PathBuf> {
        self.cache_dir.clone().or_else(|| {
            std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/spotify-player"))
        })
    }

    /// Whether Spotify Web API tokens are cached (`*_token.json` in the cache folder).
    #[must_use]
    pub fn has_cached_token(&self) -> bool {
        self.cache_folder()
            .and_then(|dir| std::fs::read_dir(dir).ok())
            .is_some_and(|entries| {
                entries
                    .filter_map(std::result::Result::ok)
                    .any(|entry| entry.file_name().to_string_lossy().ends_with("_token.json"))
            })
    }

    /// Fails fast with `spotify_auth_required` instead of letting spotify_player open a browser
    /// and block.
    ///
    /// # Errors
    /// `spotify_auth_required` when no cached token exists.
    pub fn require_auth(&self) -> Result<()> {
        if self.has_cached_token() {
            Ok(())
        } else {
            Err(auth_required(None))
        }
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = Command::new(&self.binary);
        if let Some(dir) = &self.config_dir {
            command.arg("-c").arg(dir);
        }
        if let Some(dir) = &self.cache_dir {
            command.arg("-C").arg(dir);
        }
        command.args(args);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    }

    /// Runs `spotify_player <args>` and classifies failures.
    ///
    /// # Errors
    /// See [`classify`].
    pub fn run(&self, args: &[&str]) -> Result<Output> {
        self.require_auth()?;
        match run_with_timeout(self.command(args), self.timeout, args) {
            // A spotify_player instance was starting and took the client port between the probe
            // and the bind (the daemon's warm instance restarting): retry once through it.
            Err(error) if error.code == "spotify_player_busy" => {
                std::thread::sleep(Duration::from_millis(700));
                run_with_timeout(self.command(args), self.timeout, args)
            }
            other => other,
        }
    }

    /// Runs and parses stdout as JSON (`null` becomes `Value::Null`). See [`parse_json`].
    ///
    /// # Errors
    /// Classified failures, or `spotify_player_failed` when stdout is not JSON.
    pub fn json(&self, args: &[&str]) -> Result<Value> {
        let output = self.run(args)?;
        parse_json(&output.stdout, args)
    }

    /// `spotify_player --version`, without requiring auth.
    ///
    /// # Errors
    /// When the binary cannot run.
    pub fn version(&self) -> Result<String> {
        let output = run_with_timeout(
            self.command(&["--version"]),
            Duration::from_secs(5),
            &["--version"],
        )?;
        Ok(output
            .stdout
            .trim_start_matches("spotify_player")
            .trim()
            .to_owned())
    }

    /// Starts `spotify_player authenticate` detached. It opens the Spotify consent page in the
    /// default browser and waits for the redirect to `login_redirect_uri` (default
    /// `http://127.0.0.1:8989/login`).
    ///
    /// # Errors
    /// When the process cannot be spawned.
    pub fn spawn_authenticate(&self, log: &Path) -> Result<u32> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
            .map_err(|error| Error::internal(format!("cannot open {}: {error}", log.display())))?;
        let err = file
            .try_clone()
            .map_err(|error| Error::internal(format!("cannot clone log handle: {error}")))?;
        let mut command = Command::new(&self.binary);
        if let Some(dir) = &self.config_dir {
            command.arg("-c").arg(dir);
        }
        if let Some(dir) = &self.cache_dir {
            command.arg("-C").arg(dir);
        }
        let child = command
            .arg("authenticate")
            .stdin(Stdio::null())
            .stdout(file)
            .stderr(err)
            .spawn()
            .map_err(|error| {
                Error::new(
                    "spotify_player_failed",
                    format!("Could not start `spotify_player authenticate`: {error}."),
                    "Run `spotify_player authenticate` yourself in a terminal.",
                )
            })?;
        Ok(child.id())
    }
}

/// `spotify_player_missing`.
#[must_use]
pub fn missing() -> Error {
    Error::new(
        "spotify_player_missing",
        "spotify_player is not installed (not on PATH, /opt/homebrew/bin, /usr/local/bin or ~/.cargo/bin).",
        "Install it with `brew install spotify_player` (or `cargo install spotify_player`), then run `spotify setup`.",
    )
}

/// `spotify_auth_required`.
#[must_use]
pub fn auth_required(detail: Option<&str>) -> Error {
    let mut error = Error::new(
        "spotify_auth_required",
        "spotify_player is not signed in to Spotify, so Web API features (search, lyrics, playlists, queue, devices, likes) cannot run.",
        "Run `spotify auth login`: it opens Spotify's consent page in the browser on this Mac. A Carbon must click Agree once; the token is then cached and refreshed automatically. Check with `spotify auth status`.",
    );
    if let Some(detail) = detail {
        error = error.with_details(serde_json::json!({"spotify_player": detail}));
    }
    error
}

fn run_with_timeout(mut command: Command, timeout: Duration, args: &[&str]) -> Result<Output> {
    let started = Instant::now();
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing()
        } else {
            Error::new(
                "spotify_player_failed",
                format!("spotify_player could not start: {error}."),
                "Check the binary with `spotify doctor`.",
            )
        }
    })?;
    // Drain pipes on threads so large outputs (playlists) never deadlock the child.
    let mut stdout_pipe = child.stdout.take();
    let mut stderr_pipe = child.stderr.take();
    let out_thread = std::thread::spawn(move || {
        let mut buffer = String::new();
        if let Some(pipe) = stdout_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buffer);
        }
        buffer
    });
    let err_thread = std::thread::spawn(move || {
        let mut buffer = String::new();
        if let Some(pipe) = stderr_pipe.as_mut() {
            let _ = pipe.read_to_string(&mut buffer);
        }
        buffer
    });
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Error::new(
                    "timeout",
                    format!("`spotify_player {}` did not finish within {timeout:?}.", args.join(" ")),
                    "The Spotify Web API or network may be slow. Retry; `spotify doctor` checks connectivity. If spotify_player is waiting for a browser login, run `spotify auth login`.",
                )
                .retryable());
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(5)),
            Err(error) => {
                return Err(Error::new(
                    "spotify_player_failed",
                    format!("Waiting for spotify_player failed: {error}."),
                    "Retry.",
                ));
            }
        }
    };
    let stdout = out_thread.join().unwrap_or_default().trim().to_owned();
    let stderr = err_thread.join().unwrap_or_default().trim().to_owned();
    if status.success() {
        return Ok(Output {
            stdout,
            stderr,
            elapsed: started.elapsed(),
        });
    }
    Err(classify(&stderr, &stdout, args))
}

/// Maps spotify_player's stderr (`Bad request: …`) to a structured error.
#[must_use]
pub fn classify(stderr: &str, stdout: &str, args: &[&str]) -> Error {
    let text = if stderr.is_empty() { stdout } else { stderr };
    let lower = text.to_ascii_lowercase();
    let details = serde_json::json!({
        "command": format!("spotify_player {}", args.join(" ")),
        "stderr": crate::model::truncate(text, 600),
    });
    let error = if lower.contains("address already in use")
        || lower.contains("try to connect to a client")
    {
        Error::new(
            "spotify_player_busy",
            "Another spotify_player instance was starting and holds its client port.",
            "Retry in a moment (the daemon's warm instance may be restarting).",
        )
        .retryable()
    } else if lower.contains("no active playback") || lower.contains("no track currently playing") {
        Error::new(
            "no_active_device",
            "Spotify reports no active playback device, so the Web API has nothing to control.",
            "Open Spotify.app and play anything once (or run `spotify play`), or pick a device with `spotify devices` and `spotify devices connect <name>`.",
        )
    } else if lower.contains("premium") {
        Error::new(
            "premium_required",
            "Spotify refused this Web API playback command because the account is not Premium.",
            "Playback control through the Web API needs Spotify Premium. AppleScript-based commands (play, pause, next, seek, volume) still work: set `spotify config set '{\"strategy\": \"applescript\"}'`.",
        )
    } else if lower.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("invalid access token")
        || lower.contains("token") && lower.contains("expired")
    {
        return auth_required(Some(text)).with_details(details);
    } else if lower.contains("429") || lower.contains("too many requests") {
        Error::new(
            "rate_limited",
            "The Spotify Web API is rate limiting this account.",
            "Wait a minute and retry.",
        )
        .retryable()
    } else if lower.contains("status code 400") || lower.contains("400 bad request") {
        // Spotify answers 400 for ids it cannot parse; the cause is the input, not a bug.
        Error::invalid(
            "Spotify rejected the request as malformed (HTTP 400), usually because an id or URI is not valid.",
            "Spotify ids are 22 letters and digits: pass spotify:<kind>:<id>, https://open.spotify.com/<kind>/<id> or a bare id (find one with `spotify search '<query>'`). If the input is right, report it with `spotify report`.",
        )
    } else if lower.contains("404")
        || lower.contains("cannot find")
        || lower.contains("no device with name")
        || lower.contains("not found")
    {
        Error::not_found(
            "Spotify could not find that item (check the id, kind or name).",
            "Search for the right id with `spotify search '<query>'` and pass its uri.",
        )
    } else if lower.contains("403") || lower.contains("forbidden") {
        Error::new(
            "permission_denied",
            "Spotify refused the request (403). You may not own the playlist, or the account lacks the needed scope.",
            "Only playlists you own or collaborate on can be edited. If scopes changed, re-run `spotify auth login`.",
        )
    } else if lower.contains("error sending request")
        || lower.contains("dns")
        || lower.contains("connection")
        || lower.contains("timed out")
    {
        Error::new(
            "transport",
            "spotify_player could not reach the Spotify Web API.",
            "Check the network connection and retry.",
        )
        .retryable()
    } else {
        Error::new(
            "spotify_player_failed",
            format!(
                "spotify_player failed: {}",
                crate::model::truncate(text, 300)
            ),
            "Retry once. If it persists, run `spotify doctor` and report it with `spotify report` (include the details).",
        )
    };
    error.with_details(details)
}

/// Parses spotify_player's JSON stdout (empty becomes `Value::Null`).
///
/// spotify_player copies Spotify's strings into its output verbatim, so a playlist name or
/// description can carry raw control characters (a name typed across two lines), which strict
/// JSON forbids inside strings. Those are escaped first; nothing outside string literals changes.
///
/// # Errors
/// `spotify_player_failed` when stdout is still not JSON; the hint only suspects the
/// spotify_player version when the output is not JSON at all.
pub fn parse_json(stdout: &str, args: &[&str]) -> Result<Value> {
    let stdout = stdout.trim();
    if stdout.is_empty() {
        return Ok(Value::Null);
    }
    serde_json::from_str(&escape_controls_in_strings(stdout)).map_err(|error| {
        let command = format!("spotify_player {}", args.join(" "));
        let details = serde_json::json!({
            "command": command,
            "stdout": crate::model::truncate(stdout, 400),
            "parse_error": error.to_string(),
        });
        if stdout.starts_with(['{', '[']) {
            Error::new(
                "spotify_player_failed",
                format!("`{command}` printed malformed JSON ({error})."),
                "Retry once. If it persists, report it with `spotify report` (include the details).",
            )
        } else {
            Error::new(
                "spotify_player_failed",
                format!("`{command}` printed text instead of JSON."),
                "This spotify_player version may print a different format (tested with 0.25). Run `spotify doctor`.",
            )
        }
        .with_details(details)
    })
}

/// Escapes raw control characters (U+0000–U+001F) inside JSON string literals. Borrows the input
/// when there are none.
fn escape_controls_in_strings(text: &str) -> Cow<'_, str> {
    let bytes = text.as_bytes();
    let mut out: Vec<u8> = Vec::new();
    // Bytes of `text` before this index are already in `out`.
    let mut copied = 0;
    let mut in_string = false;
    let mut after_backslash = false;
    for (index, &byte) in bytes.iter().enumerate() {
        if !in_string {
            in_string = byte == b'"';
        } else if after_backslash {
            after_backslash = false;
        } else if byte == b'\\' {
            after_backslash = true;
        } else if byte == b'"' {
            in_string = false;
        } else if byte < 0x20 {
            out.extend_from_slice(&bytes[copied..index]);
            match byte {
                b'\n' => out.extend_from_slice(b"\\n"),
                b'\r' => out.extend_from_slice(b"\\r"),
                b'\t' => out.extend_from_slice(b"\\t"),
                other => out.extend_from_slice(format!("\\u{other:04x}").as_bytes()),
            }
            copied = index + 1;
        }
    }
    if copied == 0 {
        return Cow::Borrowed(text);
    }
    out.extend_from_slice(&bytes[copied..]);
    // Only ASCII bytes were replaced, so the UTF-8 sequences around them are intact.
    String::from_utf8(out).map_or(Cow::Borrowed(text), Cow::Owned)
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(name))
            .find(|candidate| candidate.is_file())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_known_failures() {
        let c = |s: &str| classify(s, "", &["x"]).code;
        assert_eq!(
            c("Bad request: no active playback found!"),
            "no_active_device"
        );
        assert_eq!(
            c("Bad request: http error: status code 404 Not Found"),
            "not_found"
        );
        assert_eq!(
            c("Bad request: http error: status code 401 Unauthorized"),
            "spotify_auth_required"
        );
        assert_eq!(
            c("Bad request: Player command failed: Premium required"),
            "premium_required"
        );
        assert_eq!(
            c("Bad request: http error: status code 429 Too Many Requests"),
            "rate_limited"
        );
        assert_eq!(
            c("Bad request: http error: status code 400 Bad Request"),
            "invalid_input"
        );
        assert_eq!(c("Bad request: something odd"), "spotify_player_failed");
        assert_eq!(
            c(
                "Error: try to connect to a client\n\nCaused by:\n    Address already in use (os error 48)"
            ),
            "spotify_player_busy"
        );
    }

    #[test]
    fn parses_json_with_raw_control_characters_in_strings() {
        // The shape `spotify_player search queen` (0.25.1) printed: a playlist name typed across
        // lines arrives with a raw newline inside the string.
        let stdout = "{\"tracks\":[],\"playlists\":[{\"id\":\"6S5eKpEJcVEzXdb8TkO3Ud\",\"collaborative\":false,\"name\":\"Mai teri queen aave\nDil di clean aave\",\"owner\":[\"Owner\",\"owner_id\"],\"desc\":\"tab\there \\\"quoted\\\" \\\\ bell\u{7}\"}]}\n";
        assert!(
            serde_json::from_str::<Value>(stdout).is_err(),
            "strict JSON rejects it"
        );
        let value = parse_json(stdout, &["search", "queen"]).expect("parses");
        let playlist = &value["playlists"][0];
        assert_eq!(playlist["name"], "Mai teri queen aave\nDil di clean aave");
        assert_eq!(playlist["desc"], "tab\there \"quoted\" \\ bell\u{7}");
        assert_eq!(playlist["owner"][0], "Owner");
        // Valid JSON and whitespace between tokens are untouched.
        assert_eq!(
            escape_controls_in_strings("{\n\t\"a\": \"b\\n\"\n}"),
            "{\n\t\"a\": \"b\\n\"\n}"
        );
        assert_eq!(parse_json("  ", &["x"]).expect("empty"), Value::Null);
    }

    #[test]
    fn unparseable_output_says_why() {
        let malformed = parse_json("{\"tracks\": [", &["search", "x"]).expect_err("malformed");
        assert_eq!(malformed.code, "spotify_player_failed");
        assert!(malformed.message.contains("malformed JSON"));
        assert!(!malformed.hint.contains("version"), "{}", malformed.hint);
        let text = parse_json("Usage: spotify_player search", &["search", "x"]).expect_err("text");
        assert!(text.message.contains("instead of JSON"));
        assert!(text.hint.contains("version"));
    }
}
