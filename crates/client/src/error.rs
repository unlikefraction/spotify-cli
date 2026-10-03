//! One structured error shape for every surface: library, CLI, daemon IPC and backend.
//!
//! Agents read these. Every error says what failed (`code`, stable snake_case), why (`message`)
//! and what to do next (`hint`, usually an exact command). `retryable` says whether repeating the
//! same request unchanged can succeed.

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Result alias used across the crate.
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// A structured, agent-readable failure.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Error {
    /// Stable snake_case identifier. Branch on this, never on `message`.
    pub code: String,
    /// What happened and why, in one or two sentences.
    pub message: String,
    /// The next step, usually a runnable command. Empty when there is nothing to suggest.
    #[serde(default)]
    pub hint: String,
    /// Whether retrying the identical request later can succeed.
    #[serde(default)]
    pub retryable: bool,
    /// Extra machine-readable context (exit status, stderr excerpt, limits, candidates).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<Value>,
}

impl Error {
    /// Builds an error with a code, message and hint.
    pub fn new(
        code: impl Into<String>,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            hint: hint.into(),
            retryable: false,
            details: None,
        }
    }

    /// Marks the error as safe to retry unchanged.
    #[must_use]
    pub fn retryable(mut self) -> Self {
        self.retryable = true;
        self
    }

    /// Attaches machine-readable details.
    #[must_use]
    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    /// `invalid_input`: the caller sent something malformed. Exit code 2.
    pub fn invalid(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new("invalid_input", message, hint)
    }

    /// `internal`: a bug in spotify-cli. The hint points at `spotify report`.
    pub fn internal(message: impl Into<String>) -> Self {
        Self::new(
            "internal",
            message,
            "This is a spotify-cli bug. Report it with `spotify report \"<what you ran and saw>\"` (add --pr <url> if you patched it).",
        )
    }

    /// `unsupported`: the underlying tools cannot do this at all.
    pub fn unsupported(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new("unsupported", message, hint)
    }

    /// `not_found`.
    pub fn not_found(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new("not_found", message, hint)
    }

    /// `not_authenticated`: no usable spotify-cli (IAM) session. Exit code 3.
    pub fn not_authenticated(message: impl Into<String>) -> Self {
        Self::new(
            "not_authenticated",
            message,
            "Mint a short-lived token with `iam silicon-login --app-id spotify --grant-org <org> --approve-scopes` (Silicon) or `iam login --app-id spotify --grant-org <org>` (Carbon), then run `spotify login '<SLT>'`.",
        )
    }

    /// `nothing_playing`: Spotify has no current track.
    pub fn nothing_playing() -> Self {
        Self::new(
            "nothing_playing",
            "Spotify has no current track, so there is nothing to inspect or control.",
            "Start something first, for example `spotify play --search 'artist or song'` or `spotify play spotify:playlist:<id>`.",
        )
    }

    /// `platform_unsupported`: Spotify control needs macOS.
    pub fn platform_unsupported(what: &str) -> Self {
        Self::new(
            "platform_unsupported",
            format!(
                "{what} needs macOS: spotify-cli controls the Spotify desktop app through AppleScript and spotify_player on a Mac."
            ),
            "Run this command on the Mac that plays the music. Login, config, docs and report work on every platform.",
        )
    }

    /// `daemon_unavailable`: the local daemon could not be reached or started.
    pub fn daemon_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            "daemon_unavailable",
            message,
            "Run `spotify daemon start` (or `spotify daemon status` to see why it is down; the log is `~/.silicon-spotify/daemon.log`).",
        )
        .retryable()
    }

    /// `backend_unavailable`: the spotify-cli backend could not be reached.
    pub fn backend_unavailable(message: impl Into<String>) -> Self {
        Self::new(
            "backend_unavailable",
            message,
            "Check the network and `spotify config get api_url`, then retry. `spotify doctor` checks every dependency.",
        )
        .retryable()
    }

    /// Process exit code for this error (IAM CLI taxonomy):
    /// 2 usage, 3 not signed in, 4 refused, 5 transport or unavailable dependency, 1 otherwise.
    #[must_use]
    pub fn exit_code(&self) -> i32 {
        match self.code.as_str() {
            "invalid_input" | "usage" | "threshold_passed" => 2,
            "not_authenticated" | "spotify_auth_required" => 3,
            "permission_denied"
            | "automation_permission_denied"
            | "forbidden"
            | "reconsent_required"
            | "recipient_not_registered" => 4,
            "daemon_unavailable"
            | "backend_unavailable"
            | "transport"
            | "rate_limited"
            | "dependency_unavailable"
            | "timeout" => 5,
            _ => 1,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.message)?;
        if !self.hint.is_empty() {
            write!(f, " Hint: {}", self.hint)?;
        }
        Ok(())
    }
}

impl std::error::Error for Error {}

impl From<serde_json::Error> for Error {
    fn from(error: serde_json::Error) -> Self {
        Self::internal(format!("JSON encoding or decoding failed: {error}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_follow_the_iam_taxonomy() {
        assert_eq!(Error::invalid("x", "y").exit_code(), 2);
        assert_eq!(Error::not_authenticated("x").exit_code(), 3);
        assert_eq!(
            Error::new("automation_permission_denied", "", "").exit_code(),
            4
        );
        assert_eq!(Error::daemon_unavailable("x").exit_code(), 5);
        assert_eq!(Error::nothing_playing().exit_code(), 1);
    }

    #[test]
    fn serializes_without_empty_details() {
        let value = serde_json::to_value(Error::invalid("bad", "fix")).expect("json");
        assert_eq!(
            value,
            serde_json::json!({"code":"invalid_input","message":"bad","hint":"fix","retryable":false})
        );
    }
}
