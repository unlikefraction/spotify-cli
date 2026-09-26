//! Space Station recording. Keys live only here; clients send events to the gateway.
//!
//! Four tables: `spotifybackend` (this server), `spotifyclidaemon` (CLI and daemon, relayed),
//! `spotifyfrontendanalytics` and `spotifyfrontendevents` (the website, relayed). Missing keys
//! disable a table without failing startup.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use space_station::SpaceClient;

use crate::config::Settings;

/// Records events; cheap to clone.
#[derive(Clone, Default)]
pub struct Recorder {
    clients: Arc<HashMap<&'static str, SpaceClient>>,
    environment: &'static str,
}

impl Recorder {
    /// Builds one client per configured table.
    #[must_use]
    pub fn new(settings: &Settings) -> Self {
        let mut clients = HashMap::new();
        if settings.telemetry {
            let keys = &settings.table_keys;
            for (table, key) in [
                (
                    silicon_spotify_client::telemetry::BACKEND_TABLE,
                    &keys.backend,
                ),
                (
                    silicon_spotify_client::telemetry::CLI_DAEMON_TABLE,
                    &keys.clidaemon,
                ),
                (
                    silicon_spotify_client::telemetry::WEB_ANALYTICS_TABLE,
                    &keys.frontend_analytics,
                ),
                (
                    silicon_spotify_client::telemetry::WEB_EVENTS_TABLE,
                    &keys.frontend_events,
                ),
            ] {
                let Some(key) = key else { continue };
                match SpaceClient::builder(key.expose_secret())
                    .home(settings.telemetry_home.clone())
                    .url(settings.telemetry_url.clone())
                    .flush_timeout(Duration::from_millis(500))
                    .on_error(|error| tracing::debug!(%error, "space station"))
                    .build()
                {
                    Ok(client) => {
                        clients.insert(table, client);
                    }
                    Err(error) => tracing::warn!(table, %error, "telemetry table disabled"),
                }
            }
        }
        if clients.is_empty() {
            tracing::info!(
                "telemetry inactive (no Space Station table keys configured or SPOTIFY_TELEMETRY=off)"
            );
        }
        Self {
            clients: Arc::new(clients),
            environment: "production",
        }
    }

    /// Whether a table is active.
    #[must_use]
    pub fn has(&self, table: &str) -> bool {
        self.clients.contains_key(table)
    }

    /// A backend event with the standard envelope.
    pub fn backend(
        &self,
        event: &str,
        step: &str,
        outcome: &str,
        error_code: Option<&str>,
        context: Value,
    ) {
        let Some(client) = self
            .clients
            .get(silicon_spotify_client::telemetry::BACKEND_TABLE)
        else {
            return;
        };
        client.record(json!({
            "schema_version": 1,
            "app": "spotify",
            "service": "spotify-backend",
            "source": "backend",
            "version": env!("CARGO_PKG_VERSION"),
            "environment": self.environment,
            "event": event,
            "step": step,
            "outcome": outcome,
            "error_code": error_code,
            "progress": if event.ends_with(".started") { 0.0 } else { 1.0 },
            "context": context,
        }));
    }

    /// Relays a client event to its table (already validated by the gateway).
    pub fn relay(&self, table: &str, event: Value) {
        if let Some(client) = self.clients.get(table) {
            client.record(event);
        }
    }

    /// Flushes before shutdown.
    pub fn flush(&self) {
        for client in self.clients.values() {
            client.flush();
        }
    }
}
