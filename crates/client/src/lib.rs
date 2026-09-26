//! Stateless building blocks for spotify-cli.
//!
//! spotify-cli turns the Spotify desktop app on macOS into a command-line app that Carbons and
//! Silicons can drive, with playback checkpoints ("triggers") that notify a Silicon through Ting.
//!
//! This crate is the shared, stateless core:
//!
//! - [`uri`] parses Spotify ids, URIs and `open.spotify.com` links.
//! - [`timing`] parses positions, offsets and percentages such as `1:30`, `+15s` or `25%`.
//! - [`model`] holds the playback, track and library shapes every surface prints.
//! - [`applescript`] builds and parses the AppleScript that talks to Spotify.app.
//! - [`player`] wraps the `spotify_player` CLI and classifies its failures.
//! - [`control`] combines both: it tries `spotify_player` first, verifies the effect against
//!   Spotify.app, and falls back to AppleScript. It never stores anything.
//! - [`trigger`] is the pure trigger engine: given playback observations it decides what fires.
//! - [`api`] (feature `api`) is the HTTP client for the spotify-cli backend.
//! - [`store`] and [`ipc`] (feature `runtime`) are the stateful helpers shared by the `spotify`
//!   CLI and `spotify-daemon`; library consumers normally leave them off.
//!
//! Nothing in this crate caches credentials, checks for updates or retries behind the caller's
//! back. Session persistence and refresh decisions belong to the caller (the CLI and daemon use
//! [`store`]).

pub mod applescript;
pub mod control;
pub mod error;
pub mod model;
pub mod player;
pub mod telemetry;
pub mod timing;
pub mod trigger;
pub mod uri;

#[cfg(feature = "api")]
pub mod api;
#[cfg(feature = "runtime")]
pub mod ipc;
#[cfg(feature = "runtime")]
pub mod store;

pub use error::{Error, Result};

/// The bare IAM application id. Stemcell and `iam` use it to mint short-lived tokens.
pub const APP_ID: &str = "spotify";
/// The organization that owns the application in IAM and Honeycomb.
pub const OWNER_ORG: &str = "unlikefraction";
/// Human name used in help text and documentation.
pub const APP_NAME: &str = "spotify";
/// This library's version; the CLI and daemon share it.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Production backend origin. Override with `SPOTIFY_API_URL` or `spotify config set`.
pub const DEFAULT_API_URL: &str = "https://backend.spotify.unlikefraction.com";
/// IAM API origin (for discovery output only; the CLI never talks to IAM directly).
pub const IAM_URL: &str = "https://backend.iam.teamofsilicons.com";
/// IAM hosted login and consent page.
pub const IAM_AUTH_URL: &str = "https://auth.iam.teamofsilicons.com";
/// Source repository. Every bug can be reproduced, patched and sent as a pull request here.
pub const REPOSITORY: &str = "https://github.com/unlikefraction/spotify-cli";
/// Online documentation.
pub const DOCS_URL: &str = "https://spotify.unlikefraction.com/docs";
/// Website with the one-line installer.
pub const WEBSITE: &str = "https://spotify.unlikefraction.com";
/// One-line installer shown on the docs page.
pub const INSTALL_COMMAND: &str = "curl -fsSL https://spotify.unlikefraction.com/install.sh | sh";
/// This crate's source (it is not published to crates.io).
pub const RUST_PACKAGE: &str =
    "https://github.com/unlikefraction/spotify-cli/tree/main/crates/client";
/// The CLI crate's source.
pub const CLI_PACKAGE: &str = "https://github.com/unlikefraction/spotify-cli/tree/main/crates/cli";
/// Ting event types this app publishes. Each must be registered once per Ting context.
pub const TING_TYPES: &[(&str, &str)] = &[
    (
        "spotify.trigger.fired",
        "A spotify-cli playback trigger reached its checkpoint (time remaining, time elapsed, track end or track change).",
    ),
    (
        "spotify.trigger.expired",
        "A one-shot spotify-cli trigger can no longer fire because its track stopped or changed before the checkpoint.",
    ),
];

/// Validates an IAM-style handle (org or app id): lowercase letters, digits, `_`, `-`, starting
/// with a letter, `min..=max` bytes.
#[must_use]
pub fn valid_handle(value: &str, min: usize, max: usize) -> bool {
    (min..=max).contains(&value.len())
        && value.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && value
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}
