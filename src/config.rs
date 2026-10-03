//! Backend settings from environment variables (see `.env.example`).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use secrecy::SecretString;
use url::Url;

/// Validated settings.
#[derive(Clone, Debug)]
pub struct Settings {
    /// Listen address (`SPOTIFY_BIND`, default 127.0.0.1:8787).
    pub bind: SocketAddr,
    /// SQLite file (`SPOTIFY_DATABASE_PATH`).
    pub database_path: PathBuf,
    /// Public origin (`SPOTIFY_PUBLIC_ORIGIN`).
    pub public_origin: String,
    /// IAM API origin (`SPOTIFY_IAM_URL`).
    pub iam_url: Url,
    /// Bare application id (`SPOTIFY_IAM_APP_ID`, default `spotify`).
    pub app_id: String,
    /// Application secret (`SPOTIFY_IAM_APP_SECRET`, `ask_…`).
    pub app_secret: SecretString,
    /// AES-256 key for durable OBO credentials (SPOTIFY_ENCRYPTION_KEY, 64 hex characters).
    pub encryption_key: SecretString,
    /// IAM webhook signing secret (`SPOTIFY_IAM_WEBHOOK_SECRET`).
    pub webhook_secret: SecretString,
    /// Its key version (`SPOTIFY_IAM_WEBHOOK_KEY_VERSION`, default 1).
    pub webhook_key_version: i64,
    /// IAM timeout.
    pub iam_timeout: Duration,
    /// Ting API origin (`SPOTIFY_TING_URL`).
    pub ting_url: Url,
    /// Ting timeout.
    pub ting_timeout: Duration,
    /// Telemetry master switch (`SPOTIFY_TELEMETRY`).
    pub telemetry: bool,
    /// Space Station keys per table.
    pub table_keys: TableKeys,
    /// Space Station spool home.
    pub telemetry_home: PathBuf,
    /// Space Station URL.
    pub telemetry_url: String,
    /// Optional GitHub token to file bug reports as issues.
    pub github_token: Option<SecretString>,
    /// `owner/repo` for issues.
    pub github_repository: String,
    /// Browser origins allowed to post web telemetry.
    pub web_origins: Vec<String>,
}

/// Space Station table keys (write-only).
#[derive(Clone, Debug, Default)]
pub struct TableKeys {
    /// `spotifybackend`.
    pub backend: Option<SecretString>,
    /// `spotifyclidaemon`.
    pub clidaemon: Option<SecretString>,
    /// `spotifyfrontendanalytics`.
    pub frontend_analytics: Option<SecretString>,
    /// `spotifyfrontendevents`.
    pub frontend_events: Option<SecretString>,
}

fn var(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_owned())
        .filter(|v| !v.is_empty())
}

fn required(name: &str) -> anyhow::Result<String> {
    var(name).ok_or_else(|| anyhow::anyhow!("{name} is required (see .env.example)"))
}

fn origin(name: &str, default: &str) -> anyhow::Result<Url> {
    let value = var(name).unwrap_or_else(|| default.to_owned());
    let url = Url::parse(&value).map_err(|e| anyhow::anyhow!("{name} is not a URL: {e}"))?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"));
    anyhow::ensure!(
        url.scheme() == "https" || (url.scheme() == "http" && loopback),
        "{name} must be https (http only on loopback)"
    );
    Ok(url)
}

fn seconds(name: &str, default: u64) -> anyhow::Result<Duration> {
    let value = var(name).map_or(Ok(default), |v| {
        v.parse::<u64>()
            .map_err(|_| anyhow::anyhow!("{name} must be whole seconds"))
    })?;
    anyhow::ensure!((1..=120).contains(&value), "{name} must be 1-120 seconds");
    Ok(Duration::from_secs(value))
}

fn key(name: &str, table: &str) -> anyhow::Result<Option<SecretString>> {
    match var(name) {
        None => Ok(None),
        Some(value) => {
            let prefix = format!("table-{table}-");
            let hex = value.strip_prefix(&prefix).unwrap_or_default();
            anyhow::ensure!(
                hex.len() == 32 && hex.bytes().all(|b| b.is_ascii_hexdigit()),
                "{name} must be table-{table}-<32 hex>"
            );
            Ok(Some(SecretString::from(value)))
        }
    }
}

impl Settings {
    /// Reads and validates the environment.
    ///
    /// # Errors
    /// A message naming the bad or missing variable.
    pub fn from_env() -> anyhow::Result<Self> {
        let app_secret = required("SPOTIFY_IAM_APP_SECRET")?;
        anyhow::ensure!(
            app_secret.starts_with("ask_") && app_secret.len() == 47,
            "SPOTIFY_IAM_APP_SECRET must be the 47-character ask_… application secret"
        );
        let encryption_key = required("SPOTIFY_ENCRYPTION_KEY")?;
        anyhow::ensure!(
            encryption_key.len() == 64 && encryption_key.bytes().all(|b| b.is_ascii_hexdigit()),
            "SPOTIFY_ENCRYPTION_KEY must be 64 hex characters"
        );
        let webhook_secret = required("SPOTIFY_IAM_WEBHOOK_SECRET")?;
        anyhow::ensure!(
            (32..=512).contains(&webhook_secret.len()),
            "SPOTIFY_IAM_WEBHOOK_SECRET must be 32-512 characters"
        );
        let app_id = var("SPOTIFY_IAM_APP_ID").unwrap_or_else(|| "spotify".into());
        anyhow::ensure!(
            silicon_spotify_client::valid_handle(&app_id, 1, 80),
            "SPOTIFY_IAM_APP_ID must be a bare app id"
        );
        Ok(Self {
            bind: var("SPOTIFY_BIND")
                .unwrap_or_else(|| "127.0.0.1:8787".into())
                .parse()
                .map_err(|e| anyhow::anyhow!("SPOTIFY_BIND: {e}"))?,
            database_path: PathBuf::from(
                var("SPOTIFY_DATABASE_PATH").unwrap_or_else(|| "spotify.sqlite".into()),
            ),
            public_origin: var("SPOTIFY_PUBLIC_ORIGIN")
                .unwrap_or_else(|| silicon_spotify_client::DEFAULT_API_URL.into()),
            iam_url: origin("SPOTIFY_IAM_URL", "https://backend.iam.teamofsilicons.com")?,
            app_id,
            app_secret: SecretString::from(app_secret),
            encryption_key: SecretString::from(encryption_key),
            webhook_secret: SecretString::from(webhook_secret),
            webhook_key_version: var("SPOTIFY_IAM_WEBHOOK_KEY_VERSION")
                .map_or(Ok(1), |v| v.parse())
                .map_err(|_| {
                    anyhow::anyhow!("SPOTIFY_IAM_WEBHOOK_KEY_VERSION must be an integer")
                })?,
            iam_timeout: seconds("SPOTIFY_IAM_TIMEOUT_SECONDS", 8)?,
            ting_url: origin(
                "SPOTIFY_TING_URL",
                "https://backend.ting.teamofsilicons.com",
            )?,
            ting_timeout: seconds("SPOTIFY_TING_TIMEOUT_SECONDS", 15)?,
            telemetry: !var("SPOTIFY_TELEMETRY")
                .is_some_and(|v| silicon_spotify_client::telemetry::is_off(&v)),
            table_keys: TableKeys {
                backend: key("SPOTIFY_BACKEND_TABLE_KEY", "spotifybackend")?,
                clidaemon: key("SPOTIFY_CLIDAEMON_TABLE_KEY", "spotifyclidaemon")?,
                frontend_analytics: key(
                    "SPOTIFY_FRONTEND_ANALYTICS_TABLE_KEY",
                    "spotifyfrontendanalytics",
                )?,
                frontend_events: key("SPOTIFY_FRONTEND_EVENTS_TABLE_KEY", "spotifyfrontendevents")?,
            },
            telemetry_home: PathBuf::from(
                var("SPOTIFY_TELEMETRY_HOME").unwrap_or_else(|| "telemetry".into()),
            ),
            telemetry_url: var("SPOTIFY_TELEMETRY_URL")
                .unwrap_or_else(|| "https://backend.spacestation.teamofsilicons.com".into()),
            github_token: var("SPOTIFY_GITHUB_TOKEN").map(SecretString::from),
            github_repository: var("SPOTIFY_GITHUB_REPOSITORY")
                .unwrap_or_else(|| "unlikefraction/spotify-cli".into()),
            web_origins: var("SPOTIFY_WEB_ORIGINS")
                .unwrap_or_else(|| "https://spotify.unlikefraction.com".into())
                .split(',')
                .map(|s| s.trim().to_owned())
                .filter(|s| !s.is_empty())
                .collect(),
        })
    }
}
