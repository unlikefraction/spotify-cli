//! Per-home state shared by the CLI and daemon: config, IAM sessions and testing selection.
//!
//! Layout (directory 0700, files 0600, atomic writes, one exclusive lock around every
//! read-modify-write and every refresh):
//!
//! ```text
//! $SILICON_HOME/.spotify/        (falls back to $HOME/.spotify)
//!   config.json                  settings from `spotify config set '<json>'`
//!   session.json                 IAM app sessions, one slot per backend origin + environment
//!   session.lock                 exclusive lock for session.json (kept forever)
//!   testing.json                 selected testing application secret (optional)
//! ```
//!
//! Refresh rules (IAM): every refresh rotates the refresh token. The idempotency key is derived
//! from the refresh token (`spotify-refresh-<blake3>`), so an uncertain refresh retried by any
//! process (CLI or daemon) replays instead of revoking the family. IAM keeps the replay for 10
//! minutes.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write as _;
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::api::{Api, Session, Testing};
use crate::control::Strategy;
use crate::{Error, Result};

/// Refresh when the access token has less than this many seconds left.
pub const REFRESH_MARGIN_SECS: i64 = 90;

/// One Silicon's (or Carbon's) home.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Home {
    /// `$SILICON_HOME` or `$HOME`.
    pub root: PathBuf,
    /// `<root>/.spotify`.
    pub dir: PathBuf,
}

impl Home {
    /// Resolves `$SILICON_HOME`, else `$HOME`.
    ///
    /// # Errors
    /// `invalid_input` when `SILICON_HOME` is set but empty or relative, or no home is known.
    pub fn resolve() -> Result<Self> {
        if let Some(value) = std::env::var_os("SILICON_HOME") {
            let path = PathBuf::from(&value);
            if value.is_empty() || !path.is_absolute() {
                return Err(Error::invalid(
                    format!(
                        "SILICON_HOME is set to `{}`, which is not an absolute path.",
                        path.display()
                    ),
                    "Set SILICON_HOME to the Silicon's absolute home directory, or unset it to use $HOME.",
                ));
            }
            return Ok(Self::from_root(path));
        }
        let home = std::env::var_os("HOME")
            .filter(|h| !h.is_empty())
            .ok_or_else(|| {
                Error::invalid(
                    "Neither SILICON_HOME nor HOME is set.",
                    "Set SILICON_HOME (Silicons) or HOME.",
                )
            })?;
        Ok(Self::from_root(PathBuf::from(home)))
    }

    /// A home rooted at `root`.
    #[must_use]
    pub fn from_root(root: PathBuf) -> Self {
        let dir = root.join(".spotify");
        Self { root, dir }
    }

    /// Canonical root path (the daemon keys triggers by it).
    #[must_use]
    pub fn key(&self) -> String {
        fs::canonicalize(&self.root)
            .unwrap_or_else(|_| self.root.clone())
            .to_string_lossy()
            .into_owned()
    }

    /// Creates `.spotify` with mode 0700.
    ///
    /// # Errors
    /// Filesystem errors, explained.
    pub fn ensure(&self) -> Result<()> {
        if let Ok(meta) = fs::symlink_metadata(&self.dir)
            && meta.file_type().is_symlink()
        {
            return Err(Error::new(
                "unsafe_store",
                format!(
                    "{} is a symlink; spotify-cli refuses to store credentials through symlinks.",
                    self.dir.display()
                ),
                "Replace the symlink with a real directory.",
            ));
        }
        fs::create_dir_all(&self.dir).map_err(|error| io_error("create", &self.dir, &error))?;
        #[cfg(unix)]
        fs::set_permissions(&self.dir, fs::Permissions::from_mode(0o700))
            .map_err(|error| io_error("secure", &self.dir, &error))?;
        Ok(())
    }

    /// `config.json`.
    #[must_use]
    pub fn config_path(&self) -> PathBuf {
        self.dir.join("config.json")
    }

    /// `session.json`.
    #[must_use]
    pub fn session_path(&self) -> PathBuf {
        self.dir.join("session.json")
    }

    /// `testing.json`.
    #[must_use]
    pub fn testing_path(&self) -> PathBuf {
        self.dir.join("testing.json")
    }

    /// Loads config (defaults when absent).
    ///
    /// # Errors
    /// Corrupt file.
    pub fn config(&self) -> Result<Config> {
        read_json(&self.config_path()).map(Option::unwrap_or_default)
    }

    /// Saves config atomically.
    ///
    /// # Errors
    /// Filesystem errors.
    pub fn save_config(&self, config: &Config) -> Result<()> {
        self.ensure()?;
        write_json(&self.config_path(), config)
    }

    /// Takes the exclusive session lock (blocks until free).
    ///
    /// # Errors
    /// Filesystem errors.
    pub fn lock(&self) -> Result<Lock> {
        self.ensure()?;
        let path = self.dir.join("session.lock");
        let file = private(OpenOptions::new().create(true).truncate(false).write(true))
            .open(&path)
            .map_err(|error| io_error("open", &path, &error))?;
        file.lock()
            .map_err(|error| io_error("lock", &path, &error))?;
        Ok(Lock { _file: file })
    }

    /// Reads all session slots.
    ///
    /// # Errors
    /// Corrupt file.
    pub fn sessions(&self) -> Result<Sessions> {
        read_json(&self.session_path()).map(Option::unwrap_or_default)
    }

    /// Writes all session slots (caller holds the lock).
    ///
    /// # Errors
    /// Filesystem errors.
    pub fn save_sessions(&self, sessions: &Sessions, _lock: &Lock) -> Result<()> {
        self.ensure()?;
        write_json(&self.session_path(), sessions)
    }

    /// The selected testing plane: `SPOTIFY_TEST_APP_SECRET`, else `testing.json`.
    ///
    /// # Errors
    /// Corrupt file or malformed secret.
    pub fn testing(&self) -> Result<Option<Testing>> {
        if let Ok(secret) = std::env::var("SPOTIFY_TEST_APP_SECRET")
            && !secret.trim().is_empty()
        {
            return validate_test_secret(secret.trim()).map(Some);
        }
        self.testing_saved()
    }

    /// Only the saved selection (`testing.json`), ignoring the environment. Background work (the
    /// daemon) uses this so its own environment can never select a plane for a home.
    ///
    /// # Errors
    /// Corrupt file or malformed secret.
    pub fn testing_saved(&self) -> Result<Option<Testing>> {
        let saved: Option<TestingFile> = read_json(&self.testing_path())?;
        saved
            .map(|t| validate_test_secret(&t.app_secret))
            .transpose()
    }

    /// Saves (or clears) the testing selection.
    ///
    /// # Errors
    /// Filesystem errors.
    pub fn save_testing(&self, secret: Option<&str>) -> Result<()> {
        self.ensure()?;
        match secret {
            Some(secret) => {
                validate_test_secret(secret)?;
                write_json(
                    &self.testing_path(),
                    &TestingFile {
                        app_secret: secret.to_owned(),
                    },
                )
            }
            None => match fs::remove_file(self.testing_path()) {
                Ok(()) => Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(io_error("remove", &self.testing_path(), &error)),
            },
        }
    }
}

#[derive(Serialize, Deserialize)]
struct TestingFile {
    app_secret: String,
}

fn validate_test_secret(secret: &str) -> Result<Testing> {
    if !secret.starts_with("ask_")
        || secret.len() != 47
        || !secret.bytes().all(|b| b.is_ascii_graphic())
    {
        return Err(Error::invalid(
            "The testing application secret must be the 47-character `ask_…` secret of the Spotify app inside an IAM testing environment.",
            "Get it with `honeycomb --test <ENV> --json apps rotate-secret spotify --revision <R>` and pass it with `--app-secret-file -`.",
        ));
    }
    Ok(Testing {
        app_secret: secret.to_owned(),
    })
}

/// Holds `session.lock` until dropped.
#[derive(Debug)]
pub struct Lock {
    _file: File,
}

/// Saved sessions keyed by [`slot_key`].
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Sessions {
    /// Slots.
    #[serde(default)]
    pub slots: BTreeMap<String, Slot>,
}

/// One saved IAM application session.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Slot {
    /// Backend origin it belongs to.
    pub api_url: String,
    /// The session (tokens, actor, org).
    pub session: Session,
    /// Unix seconds when the access token expires.
    pub expires_at: i64,
    /// Unix seconds when first obtained via login.
    pub logged_in_at: i64,
    /// Key of an in-flight refresh (persisted before calling).
    #[serde(default)]
    pub pending_refresh_key: Option<String>,
    /// When that refresh started (unix seconds).
    #[serde(default)]
    pub refresh_started_at: Option<i64>,
}

/// `<origin>#production` or `<origin>#test:<sha-prefix>`.
#[must_use]
pub fn slot_key(api_url: &str, testing: Option<&Testing>) -> String {
    let origin = api_url.trim_end_matches('/');
    match testing {
        None => format!("{origin}#production"),
        Some(testing) => {
            let digest = blake3::hash(testing.app_secret.as_bytes()).to_hex();
            format!("{origin}#test:{}", &digest.as_str()[..16])
        }
    }
}

/// Deterministic idempotency key for exchanging this SLT.
#[must_use]
pub fn login_key(slt: &str) -> String {
    format!("spotify-login-{}", blake3::hash(slt.as_bytes()).to_hex())
}

/// Deterministic idempotency key for rotating this refresh token.
#[must_use]
pub fn refresh_key(refresh_token: &str) -> String {
    format!(
        "spotify-refresh-{}",
        blake3::hash(refresh_token.as_bytes()).to_hex()
    )
}

fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

/// Saves a fresh login.
///
/// # Errors
/// Filesystem errors.
pub fn save_login(home: &Home, key: &str, api_url: &str, session: Session) -> Result<Slot> {
    let lock = home.lock()?;
    let mut sessions = home.sessions()?;
    let slot = Slot {
        api_url: api_url.to_owned(),
        expires_at: now() + session.expires_in.max(0),
        logged_in_at: now(),
        session,
        pending_refresh_key: None,
        refresh_started_at: None,
    };
    sessions.slots.insert(key.to_owned(), slot.clone());
    home.save_sessions(&sessions, &lock)?;
    Ok(slot)
}

/// Removes a slot. Returns it when present.
///
/// # Errors
/// Filesystem errors.
pub fn remove_slot(home: &Home, key: &str) -> Result<Option<Slot>> {
    let lock = home.lock()?;
    let mut sessions = home.sessions()?;
    let removed = sessions.slots.remove(key);
    home.save_sessions(&sessions, &lock)?;
    Ok(removed)
}

/// Returns a session with at least [`REFRESH_MARGIN_SECS`] left, refreshing under the lock when
/// needed (`force` refreshes regardless, e.g. after a 401).
///
/// # Errors
/// `not_authenticated` when there is no session or IAM revoked it (the slot is then deleted);
/// retryable transport errors otherwise (the pending key is kept for a safe retry).
pub async fn fresh_session(home: &Home, api: &Api, key: &str, force: bool) -> Result<Slot> {
    let home_for_lock = home.clone();
    let lock = tokio::task::spawn_blocking(move || home_for_lock.lock())
        .await
        .map_err(|error| Error::internal(format!("lock task failed: {error}")))??;
    let mut sessions = home.sessions()?;
    let Some(slot) = sessions.slots.get(key).cloned() else {
        return Err(Error::not_authenticated(format!(
            "No spotify-cli session is saved in {} for {key}.",
            home.dir.display(),
        )));
    };
    if !force && slot.pending_refresh_key.is_none() && slot.expires_at > now() + REFRESH_MARGIN_SECS
    {
        return Ok(slot);
    }
    let refresh_key = slot
        .pending_refresh_key
        .clone()
        .unwrap_or_else(|| refresh_key(&slot.session.refresh_token));
    if slot.pending_refresh_key.is_none() {
        let mut pending = slot.clone();
        pending.pending_refresh_key = Some(refresh_key.clone());
        pending.refresh_started_at = Some(now());
        sessions.slots.insert(key.to_owned(), pending);
        home.save_sessions(&sessions, &lock)?;
    }
    match api.refresh(&slot.session.refresh_token, &refresh_key).await {
        Ok(mut session) => {
            // Refresh responses never carry the login-time Ting registration; keep it.
            if session.ting.is_none() {
                session.ting.clone_from(&slot.session.ting);
            }
            let updated = Slot {
                api_url: slot.api_url.clone(),
                expires_at: now() + session.expires_in.max(0),
                logged_in_at: slot.logged_in_at,
                session,
                pending_refresh_key: None,
                refresh_started_at: None,
            };
            sessions.slots.insert(key.to_owned(), updated.clone());
            home.save_sessions(&sessions, &lock)?;
            Ok(updated)
        }
        Err(error) if error.code == "not_authenticated" => {
            sessions.slots.remove(key);
            home.save_sessions(&sessions, &lock)?;
            Err(Error::not_authenticated(
                "The saved spotify-cli session was revoked or expired in IAM (refresh was refused), so it was removed.",
            )
            .with_details(json!({"iam": error})))
        }
        Err(error) => {
            let started = slot.refresh_started_at.unwrap_or_else(now);
            Err(error.with_details(json!({
                "refresh_pending_since": started,
                "note": "The refresh outcome is uncertain. Retrying within 10 minutes replays it safely with the same idempotency key.",
            })))
        }
    }
}

/// Settings from `spotify config set`.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Backend origin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_url: Option<String>,
    /// Telemetry opt-in (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub telemetry: Option<bool>,
    /// Default organization for org-scoped commands (else `SILICON_ORG`, else the session's).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub org: Option<String>,
    /// `auto`, `spotify_player` or `applescript`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub strategy: Option<Strategy>,
    /// Launch Spotify.app (hidden) when a control command finds it closed (default true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch_spotify: Option<bool>,
    /// How long to wait for spotify_player's effect before falling back (ms, default 2500).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verify_timeout_ms: Option<u64>,
    /// Explicit spotify_player binary.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotify_player_binary: Option<String>,
    /// spotify_player config folder (`-c`), e.g. to bring your own Spotify client id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotify_player_config_dir: Option<String>,
    /// spotify_player cache folder (`-C`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spotify_player_cache_dir: Option<String>,
    /// Default number of search results per kind (1–50, default 10).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub search_limit: Option<u32>,
    /// ISI written into Ting metadata when the `ISI` env var is absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notify_isi: Option<String>,
    /// Keep an eye on new releases and install them (script installs; Honeycomb installs are
    /// updated by Honeycomb). Default true.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_update: Option<bool>,
    /// Default output: `human` or `json`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
}

/// Every configurable key: (name, type, default, description).
pub const CONFIG_KEYS: &[(&str, &str, &str, &str)] = &[
    (
        "api_url",
        "string (https origin)",
        crate::DEFAULT_API_URL,
        "spotify-cli backend origin. SPOTIFY_API_URL overrides it.",
    ),
    (
        "telemetry",
        "boolean",
        "true",
        "Send anonymous usage and diagnostics to Space Station. Env kill switches: SPOTIFY_TELEMETRY, SPACE_STATION_TELEMETRY, SILICON_TELEMETRY = 0|false|off|no.",
    ),
    (
        "org",
        "string (org handle)",
        "SILICON_ORG, then the session's org",
        "Organization used for org-scoped calls (Ting delivery).",
    ),
    (
        "strategy",
        "auto | spotify_player | applescript",
        "auto",
        "Playback control path. auto = spotify_player first, verified against Spotify.app, AppleScript fallback.",
    ),
    (
        "launch_spotify",
        "boolean",
        "true",
        "Start Spotify.app hidden when a control command finds it closed.",
    ),
    (
        "verify_timeout_ms",
        "integer 200-15000",
        "2500",
        "How long to wait for spotify_player's effect before falling back to AppleScript.",
    ),
    (
        "spotify_player_binary",
        "string (path)",
        "auto-detect",
        "spotify_player executable (PATH, /opt/homebrew/bin, /usr/local/bin, ~/.cargo/bin).",
    ),
    (
        "spotify_player_config_dir",
        "string (path)",
        "~/.config/spotify-player",
        "spotify_player config folder, e.g. to bring your own Spotify client_id (BYO).",
    ),
    (
        "spotify_player_cache_dir",
        "string (path)",
        "~/.cache/spotify-player",
        "spotify_player cache folder (holds its Spotify tokens).",
    ),
    (
        "search_limit",
        "integer 1-50",
        "10",
        "Default results per kind for `spotify search`.",
    ),
    (
        "notify_isi",
        "string",
        "unset",
        "ISI named in Ting metadata when the ISI env var is absent, so your flow can route trigger notifications.",
    ),
    (
        "auto_update",
        "boolean",
        "true",
        "Let the daemon check hourly for a new release and install it (script installs only; Honeycomb updates its own installs).",
    ),
    (
        "output",
        "human | json",
        "human",
        "Default output format when --json is not given.",
    ),
];

impl Config {
    /// Applies a JSON object: known keys are set, `null` unsets. Rejects duplicates, unknown keys
    /// and wrong types before changing anything.
    ///
    /// # Errors
    /// `invalid_input` naming the offending key and the valid keys.
    pub fn apply_json(&self, text: &str) -> Result<(Self, Vec<String>)> {
        let entries = strict_object(text)?;
        let mut merged = serde_json::to_value(self)?;
        let object = merged
            .as_object_mut()
            .ok_or_else(|| Error::internal("config is not an object"))?;
        let mut changed = Vec::new();
        for (key, value) in entries {
            if !CONFIG_KEYS.iter().any(|(name, ..)| *name == key) {
                return Err(Error::invalid(
                    format!("`{key}` is not a spotify-cli setting."),
                    format!(
                        "Valid keys: {}. See `spotify config keys` for types and defaults.",
                        CONFIG_KEYS
                            .iter()
                            .map(|(n, ..)| *n)
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ));
            }
            if value.is_null() {
                object.remove(&key);
            } else {
                object.insert(key.clone(), value);
            }
            changed.push(key);
        }
        let config: Self = serde_json::from_value(merged).map_err(|error| {
            Error::invalid(
                format!("A value has the wrong type: {error}."),
                "Check `spotify config keys` for each key's type, e.g. {\"telemetry\": false, \"strategy\": \"applescript\"}.",
            )
        })?;
        config.validate()?;
        Ok((config, changed))
    }

    /// Range and format checks.
    ///
    /// # Errors
    /// `invalid_input`.
    pub fn validate(&self) -> Result<()> {
        if let Some(url) = &self.api_url {
            crate::api::validate_base(url)?;
        }
        if let Some(ms) = self.verify_timeout_ms
            && !(200..=15_000).contains(&ms)
        {
            return Err(Error::invalid(
                "verify_timeout_ms must be between 200 and 15000.",
                "For example {\"verify_timeout_ms\": 2500}.",
            ));
        }
        if let Some(limit) = self.search_limit
            && !(1..=50).contains(&limit)
        {
            return Err(Error::invalid(
                "search_limit must be between 1 and 50.",
                "For example {\"search_limit\": 10}.",
            ));
        }
        if let Some(org) = &self.org
            && !valid_handle(org, 3, 50)
        {
            return Err(Error::invalid(
                format!("`{org}` is not an organization handle."),
                "Use the bare org handle, e.g. unlikefraction.",
            ));
        }
        if let Some(output) = &self.output
            && output != "human"
            && output != "json"
        {
            return Err(Error::invalid(
                "output must be human or json.",
                "For example {\"output\": \"json\"}.",
            ));
        }
        for (key, path) in [
            ("spotify_player_binary", &self.spotify_player_binary),
            ("spotify_player_config_dir", &self.spotify_player_config_dir),
            ("spotify_player_cache_dir", &self.spotify_player_cache_dir),
        ] {
            if let Some(path) = path
                && !Path::new(path).is_absolute()
            {
                return Err(Error::invalid(
                    format!("{key} must be an absolute path."),
                    "Pass the full path, e.g. /opt/homebrew/bin/spotify_player.",
                ));
            }
        }
        Ok(())
    }

    /// The effective backend origin: `SPOTIFY_API_URL`, then config, then the default.
    #[must_use]
    pub fn api_url(&self) -> String {
        std::env::var("SPOTIFY_API_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .or_else(|| self.api_url.clone())
            .unwrap_or_else(|| crate::DEFAULT_API_URL.to_owned())
    }

    /// Effective settings with defaults filled in (what `config show` prints).
    #[must_use]
    pub fn effective(&self) -> Value {
        json!({
            "api_url": self.api_url(),
            "telemetry": crate::telemetry::enabled(self.telemetry),
            "telemetry_configured": self.telemetry.unwrap_or(true),
            "org": self.org,
            "strategy": self.strategy.unwrap_or_default(),
            "launch_spotify": self.launch_spotify.unwrap_or(true),
            "verify_timeout_ms": self.verify_timeout_ms.unwrap_or(2500),
            "spotify_player_binary": self.spotify_player_binary,
            "spotify_player_config_dir": self.spotify_player_config_dir,
            "spotify_player_cache_dir": self.spotify_player_cache_dir,
            "search_limit": self.search_limit.unwrap_or(10),
            "notify_isi": self.notify_isi,
            "auto_update": self.auto_update.unwrap_or(true),
            "output": self.output.clone().unwrap_or_else(|| "human".into()),
        })
    }
}

pub use crate::valid_handle;

/// Parses a JSON object, rejecting duplicate keys and trailing data.
///
/// # Errors
/// `invalid_input` with the position of the problem.
pub fn strict_object(text: &str) -> Result<Vec<(String, Value)>> {
    struct Entries(Vec<(String, Value)>);
    impl<'de> serde::Deserialize<'de> for Entries {
        fn deserialize<D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> std::result::Result<Self, D::Error> {
            struct Visitor;
            impl<'de> serde::de::Visitor<'de> for Visitor {
                type Value = Entries;
                fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                    f.write_str("a JSON object")
                }
                fn visit_map<A: serde::de::MapAccess<'de>>(
                    self,
                    mut map: A,
                ) -> std::result::Result<Entries, A::Error> {
                    let mut entries: Vec<(String, Value)> = Vec::new();
                    while let Some((key, value)) = map.next_entry::<String, Value>()? {
                        if entries.iter().any(|(k, _)| *k == key) {
                            return Err(serde::de::Error::custom(format!("duplicate key `{key}`")));
                        }
                        entries.push((key, value));
                    }
                    Ok(Entries(entries))
                }
            }
            deserializer.deserialize_map(Visitor)
        }
    }
    serde_json::from_str::<Entries>(text.trim()).map(|e| e.0).map_err(|error| {
        Error::invalid(
            format!("Config must be one JSON object: {error}."),
            "Quote it for the shell, e.g. spotify config set '{\"telemetry\": false, \"strategy\": \"auto\"}'.",
        )
    })
}

fn read_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map(Some).map_err(|error| {
            Error::new(
                "corrupt_store",
                format!("{} is not valid: {error}.", path.display()),
                format!(
                    "Fix or delete {} (deleting a session file logs you out).",
                    path.display()
                ),
            )
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io_error("read", path, &error)),
    }
}

/// Writes JSON atomically with mode 0600 (temp file, fsync, rename, directory fsync).
///
/// # Errors
/// Filesystem errors.
pub fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| Error::internal("path without parent"))?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        uuid::Uuid::now_v7().simple()
    ));
    let bytes = serde_json::to_vec_pretty(value)?;
    let mut file = private(OpenOptions::new().create_new(true).write(true))
        .open(&tmp)
        .map_err(|error| io_error("create", &tmp, &error))?;
    file.write_all(&bytes)
        .map_err(|error| io_error("write", &tmp, &error))?;
    file.sync_all()
        .map_err(|error| io_error("sync", &tmp, &error))?;
    drop(file);
    fs::rename(&tmp, path).map_err(|error| {
        let _ = fs::remove_file(&tmp);
        io_error("replace", path, &error)
    })?;
    if let Ok(dir) = File::open(dir) {
        let _ = dir.sync_all();
    }
    Ok(())
}

/// Owner-only file mode on unix.
fn private(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    options.mode(0o600);
    options
}

fn io_error(action: &str, path: &Path, error: &std::io::Error) -> Error {
    Error::new(
        "store_io",
        format!("Could not {action} {}: {error}.", path.display()),
        "Check that the directory exists, is owned by you and is writable (mode 0700). SILICON_HOME selects the home.",
    )
}

/// Turns entries of `apply_json` into a printable object without secrets.
#[must_use]
pub fn describe_changes(changed: &[String], config: &Config) -> Value {
    let effective = config.effective();
    let mut out = Map::new();
    for key in changed {
        out.insert(
            key.clone(),
            effective.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_set_is_strict() {
        let config = Config::default();
        let (config, changed) = config
            .apply_json(r#"{"telemetry": false, "strategy": "applescript"}"#)
            .expect("applies");
        assert_eq!(config.telemetry, Some(false));
        assert_eq!(config.strategy, Some(Strategy::Applescript));
        assert_eq!(changed, vec!["telemetry", "strategy"]);
        assert_eq!(
            config
                .apply_json(r#"{"nope": 1}"#)
                .expect_err("unknown")
                .code,
            "invalid_input"
        );
        assert!(
            config
                .apply_json(r#"{"telemetry": false, "telemetry": true}"#)
                .is_err()
        );
        assert!(config.apply_json(r#"{"telemetry": "yes"}"#).is_err());
        assert!(config.apply_json(r#"{"verify_timeout_ms": 5}"#).is_err());
        let (reset, _) = config.apply_json(r#"{"telemetry": null}"#).expect("unset");
        assert_eq!(reset.telemetry, None);
        assert!(config.apply_json("[1]").is_err());
    }

    #[test]
    fn slots_are_separate_for_testing() {
        let prod = slot_key("https://x/", None);
        let test = slot_key(
            "https://x",
            Some(&Testing {
                app_secret: format!("ask_{}", "a".repeat(43)),
            }),
        );
        assert_eq!(prod, "https://x#production");
        assert!(test.starts_with("https://x#test:"));
        assert_ne!(login_key("a"), login_key("b"));
    }

    #[test]
    fn writes_private_files_atomically() {
        let dir = tempfile::tempdir().expect("tmp");
        let home = Home::from_root(dir.path().to_path_buf());
        home.save_config(&Config {
            telemetry: Some(false),
            ..Config::default()
        })
        .expect("save");
        let mode = fs::metadata(home.config_path())
            .expect("meta")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
        let dir_mode = fs::metadata(&home.dir).expect("meta").permissions().mode() & 0o777;
        assert_eq!(dir_mode, 0o700);
        assert_eq!(home.config().expect("load").telemetry, Some(false));
    }
}
