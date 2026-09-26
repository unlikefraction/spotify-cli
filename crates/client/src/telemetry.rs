//! Telemetry envelope and opt-out rules (Space Station).
//!
//! Telemetry is on by default and can be turned off at four levels; the explicit opt-out always
//! wins:
//! - `spotify config set '{"telemetry": false}'` (per Silicon home),
//! - env `SPOTIFY_TELEMETRY`, `SPACE_STATION_TELEMETRY` or `SILICON_TELEMETRY` set to
//!   `0|false|off|no` (Stemcell sets `SPACE_STATION_TELEMETRY=0` when its telemetry is off),
//! - the `X-Spotify-Telemetry: off` request header (the backend then skips request events),
//! - the backend-wide `SPOTIFY_TELEMETRY=off`.
//!
//! The CLI and daemon never hold a Space Station key. Events are self-contained (source, step,
//! progress, outcome, error code, versions, trace id) and go to the backend's telemetry gateway,
//! which writes them to the `spotifyclidaemon` table. Titles, lyrics, search queries, notes,
//! tokens and SLTs are never recorded.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

use crate::model::now_rfc3339;

/// Space Station table for CLI and daemon events.
pub const CLI_DAEMON_TABLE: &str = "spotifyclidaemon";
/// Space Station table for backend events.
pub const BACKEND_TABLE: &str = "spotifybackend";
/// Space Station table for website analytics.
pub const WEB_ANALYTICS_TABLE: &str = "spotifyfrontendanalytics";
/// Space Station table for website events.
pub const WEB_EVENTS_TABLE: &str = "spotifyfrontendevents";

/// Env switches that disable telemetry when set to `0|false|off|no`.
pub const KILL_SWITCHES: &[&str] = &[
    "SPOTIFY_TELEMETRY",
    "SPACE_STATION_TELEMETRY",
    "SILICON_TELEMETRY",
];

/// Whether an env value means "off".
#[must_use]
pub fn is_off(value: &str) -> bool {
    matches!(
        value.trim().to_ascii_lowercase().as_str(),
        "0" | "false" | "off" | "no"
    )
}

/// Resolves the effective setting: any env kill switch wins, then the saved config (default on).
#[must_use]
pub fn enabled(configured: Option<bool>) -> bool {
    if KILL_SWITCHES
        .iter()
        .any(|name| std::env::var(name).is_ok_and(|v| is_off(&v)))
    {
        return false;
    }
    configured.unwrap_or(true)
}

/// One self-contained telemetry event.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Unique id (UUIDv7).
    pub id: String,
    /// Dotted event name, e.g. `cli.command.completed`, `trigger.fired`, `ting.delivery.failed`.
    #[serde(rename = "type")]
    pub event: String,
    /// Event payload (allowlisted fields only).
    pub data: Value,
    /// Common envelope (source, versions, trace).
    pub metadata: Value,
}

/// Builds events for one process.
#[derive(Clone, Debug)]
pub struct Builder {
    /// `cli` or `daemon`.
    pub source: &'static str,
    /// Correlates CLI → daemon → backend for one command.
    pub trace_id: String,
    /// Per-process id.
    pub instance_id: String,
    /// `production` or `testing`.
    pub environment: String,
}

impl Builder {
    /// A builder with fresh ids.
    #[must_use]
    pub fn new(source: &'static str, environment: &str) -> Self {
        Self {
            source,
            trace_id: uuid::Uuid::now_v7().to_string(),
            instance_id: uuid::Uuid::now_v7().to_string(),
            environment: environment.to_owned(),
        }
    }

    /// An event with `step`, `outcome` (`ok|error|skipped|timeout`), optional `error_code`,
    /// `duration_ms`, `progress` (0–1) and allowlisted context.
    #[must_use]
    pub fn event(
        &self,
        event: &str,
        step: &str,
        outcome: &str,
        error_code: Option<&str>,
        duration_ms: Option<u64>,
        context: Map<String, Value>,
    ) -> Event {
        let progress = if event.ends_with(".started") {
            0.0
        } else {
            1.0
        };
        Event {
            id: uuid::Uuid::now_v7().to_string(),
            event: event.to_owned(),
            data: json!({
                "step": step,
                "outcome": outcome,
                "error_code": error_code,
                "duration_ms": duration_ms,
                "progress": progress,
                "context": context,
            }),
            metadata: json!({
                "schema_version": 1,
                "app": crate::APP_ID,
                "service": format!("spotify-{}", self.source),
                "source": self.source,
                "version": crate::VERSION,
                "environment": self.environment,
                "instance_id": self.instance_id,
                "trace_id": self.trace_id,
                "isi": std::env::var("ISI").ok().filter(|v| !v.is_empty()),
                "occurred_at": now_rfc3339(),
                "client": {
                    "os": std::env::consts::OS,
                    "arch": std::env::consts::ARCH,
                },
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_values() {
        for v in ["0", "false", "OFF", " no "] {
            assert!(is_off(v));
        }
        assert!(!is_off("1"));
        assert!(!is_off("on"));
    }

    #[test]
    fn events_are_self_contained() {
        let builder = Builder::new("cli", "production");
        let event = builder.event(
            "cli.command.completed",
            "pause",
            "ok",
            None,
            Some(12),
            Map::new(),
        );
        assert_eq!(event.metadata["app"], "spotify");
        assert_eq!(event.metadata["source"], "cli");
        assert_eq!(event.data["progress"], 1.0);
        assert_eq!(event.data["step"], "pause");
    }
}
