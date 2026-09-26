//! The daemon's durable state (`~/.silicon-spotify/daemon.sqlite`, mode 0600).
//!
//! - `triggers`: every trigger with its runtime state and delivery target.
//! - `firings`: the Ting outbox and trigger history (pending → sent | failed | local).
//! - `kv`: the managed queue, the play tracker, the resume point and daemon settings.
//! - `telemetry`: a bounded outbox of CLI/daemon events relayed to the backend.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OptionalExtension as _, params};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use silicon_spotify_client::trigger::Trigger;
use silicon_spotify_client::{Error, Result};

/// Telemetry events relayed per request batch.
pub const TELEMETRY_PEEK: usize = 40;

/// Where a trigger's notifications go.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Delivery {
    /// Canonical Silicon home (holds the IAM session).
    pub home: String,
    /// Backend origin.
    pub api_url: String,
    /// Session slot key in that home.
    pub slot: String,
    /// Uses the home's testing plane.
    pub testing: bool,
    /// Send through Ting (false = record locally only).
    pub ting: bool,
    /// Recipient public id (`si:x`), known from the session at creation.
    pub recipient: Option<String>,
    /// Organization for delivery.
    pub org: Option<String>,
    /// ISI echoed in Ting metadata.
    pub isi: Option<String>,
}

/// A stored trigger.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StoredTrigger {
    /// The trigger and its runtime state.
    pub trigger: Trigger,
    /// Delivery target.
    pub delivery: Delivery,
}

/// One firing (outbox row).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct FiringRow {
    /// `fir_<uuid>`.
    pub id: String,
    /// Trigger id.
    pub trigger_id: String,
    /// Home.
    pub home: String,
    /// `fired`, `expired` or `test`.
    pub outcome: String,
    /// Ting type.
    pub ting_type: String,
    /// Ting key without the recipient prefix.
    pub key_suffix: String,
    /// Ting data.
    pub data: Value,
    /// Ting metadata.
    pub metadata: Value,
    /// Delivery target.
    pub delivery: Delivery,
    /// `pending`, `sent`, `failed` or `local`.
    pub state: String,
    /// Delivery attempts.
    pub attempts: i64,
    /// Next attempt (unix ms).
    pub next_attempt_ms: i64,
    /// Last delivery error.
    pub last_error: Option<Value>,
    /// Ting acceptance (`{"id","created_at","silent"}`).
    pub ting: Option<Value>,
    /// When it fired.
    pub created_at: String,
    /// When Ting accepted it.
    pub sent_at: Option<String>,
}

/// Thread-safe handle.
pub struct Db {
    conn: Mutex<Connection>,
}

fn db_error(error: &rusqlite::Error) -> Error {
    Error::new(
        "daemon_store",
        format!("The daemon database failed: {error}."),
        "Check disk space and permissions of ~/.silicon-spotify/. If the file is corrupt, stop the daemon and move daemon.sqlite aside (triggers are lost).",
    )
}

impl Db {
    /// Opens (and migrates) the database.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn open(path: &Path) -> Result<Self> {
        let conn = Connection::open(path).map_err(|e| db_error(&e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
        }
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS triggers (
                id TEXT PRIMARY KEY, home TEXT NOT NULL, body TEXT NOT NULL, delivery TEXT NOT NULL,
                status TEXT NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS triggers_status ON triggers(status);
             CREATE TABLE IF NOT EXISTS firings (
                id TEXT PRIMARY KEY, trigger_id TEXT NOT NULL, home TEXT NOT NULL, outcome TEXT NOT NULL,
                ting_type TEXT NOT NULL, key_suffix TEXT NOT NULL, data TEXT NOT NULL, metadata TEXT NOT NULL,
                delivery TEXT NOT NULL, state TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                next_attempt_ms INTEGER NOT NULL, last_error TEXT, ting TEXT, created_at TEXT NOT NULL, sent_at TEXT);
             CREATE INDEX IF NOT EXISTS firings_pending ON firings(state, next_attempt_ms);
             CREATE INDEX IF NOT EXISTS firings_home ON firings(home, created_at);
             CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS telemetry (id TEXT PRIMARY KEY, body TEXT NOT NULL, created_ms INTEGER NOT NULL);",
        )
        .map_err(|e| db_error(&e))?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Reads a JSON value from `kv`.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn get<T: DeserializeOwned>(&self, key: &str) -> Result<Option<T>> {
        let text: Option<String> = self
            .conn()
            .query_row("SELECT value FROM kv WHERE key = ?1", params![key], |row| {
                row.get(0)
            })
            .optional()
            .map_err(|e| db_error(&e))?;
        text.map(|t| serde_json::from_str(&t).map_err(Error::from))
            .transpose()
    }

    /// Writes a JSON value to `kv`.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn put<T: Serialize>(&self, key: &str, value: &T) -> Result<()> {
        let text = serde_json::to_string(value)?;
        self.conn()
            .execute(
                "INSERT INTO kv(key, value) VALUES(?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, text],
            )
            .map_err(|e| db_error(&e))?;
        Ok(())
    }

    /// Inserts or updates a trigger.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn save_trigger(&self, stored: &StoredTrigger) -> Result<()> {
        let body = serde_json::to_string(&stored.trigger)?;
        let delivery = serde_json::to_string(&stored.delivery)?;
        let status = serde_json::to_value(stored.trigger.status)?;
        self.conn()
            .execute(
                "INSERT INTO triggers(id, home, body, delivery, status, created_at, updated_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET body = excluded.body, delivery = excluded.delivery,
                   status = excluded.status, updated_at = excluded.updated_at",
                params![
                    stored.trigger.id,
                    stored.delivery.home,
                    body,
                    delivery,
                    status.as_str().unwrap_or("active"),
                    stored.trigger.created_at,
                    silicon_spotify_client::model::now_rfc3339()
                ],
            )
            .map_err(|e| db_error(&e))?;
        Ok(())
    }

    /// Triggers, optionally only active ones and/or one home's.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn triggers(&self, active_only: bool, home: Option<&str>) -> Result<Vec<StoredTrigger>> {
        let conn = self.conn();
        let mut statement = conn
            .prepare(
                "SELECT body, delivery FROM triggers
                 WHERE (?1 = 0 OR status = 'active') AND (?2 IS NULL OR home = ?2)
                 ORDER BY created_at",
            )
            .map_err(|e| db_error(&e))?;
        let rows = statement
            .query_map(params![i32::from(active_only), home], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| db_error(&e))?;
        let mut out = Vec::new();
        for row in rows {
            let (body, delivery) = row.map_err(|e| db_error(&e))?;
            out.push(StoredTrigger {
                trigger: serde_json::from_str(&body)?,
                delivery: serde_json::from_str(&delivery)?,
            });
        }
        Ok(out)
    }

    /// One trigger by id.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn trigger(&self, id: &str) -> Result<Option<StoredTrigger>> {
        let row: Option<(String, String)> = self
            .conn()
            .query_row(
                "SELECT body, delivery FROM triggers WHERE id = ?1",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()
            .map_err(|e| db_error(&e))?;
        row.map(|(body, delivery)| {
            Ok(StoredTrigger {
                trigger: serde_json::from_str(&body)?,
                delivery: serde_json::from_str(&delivery)?,
            })
        })
        .transpose()
    }

    /// Records a firing.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn insert_firing(&self, row: &FiringRow) -> Result<()> {
        self.conn()
            .execute(
                "INSERT OR IGNORE INTO firings(id, trigger_id, home, outcome, ting_type, key_suffix, data, metadata, delivery,
                   state, attempts, next_attempt_ms, last_error, ting, created_at, sent_at)
                 VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                params![
                    row.id,
                    row.trigger_id,
                    row.home,
                    row.outcome,
                    row.ting_type,
                    row.key_suffix,
                    row.data.to_string(),
                    row.metadata.to_string(),
                    serde_json::to_string(&row.delivery)?,
                    row.state,
                    row.attempts,
                    row.next_attempt_ms,
                    row.last_error.as_ref().map(Value::to_string),
                    row.ting.as_ref().map(Value::to_string),
                    row.created_at,
                    row.sent_at
                ],
            )
            .map_err(|e| db_error(&e))?;
        Ok(())
    }

    /// Updates delivery state.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn update_firing(&self, row: &FiringRow) -> Result<()> {
        self.conn()
            .execute(
                "UPDATE firings SET state = ?2, attempts = ?3, next_attempt_ms = ?4, last_error = ?5, ting = ?6, sent_at = ?7
                 WHERE id = ?1",
                params![
                    row.id,
                    row.state,
                    row.attempts,
                    row.next_attempt_ms,
                    row.last_error.as_ref().map(Value::to_string),
                    row.ting.as_ref().map(Value::to_string),
                    row.sent_at
                ],
            )
            .map_err(|e| db_error(&e))?;
        Ok(())
    }

    fn read_firings(&self, sql: &str, bind: &[&dyn rusqlite::ToSql]) -> Result<Vec<FiringRow>> {
        let conn = self.conn();
        let mut statement = conn.prepare(sql).map_err(|e| db_error(&e))?;
        let rows = statement
            .query_map(bind, |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, String>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, i64>(10)?,
                    row.get::<_, i64>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, Option<String>>(13)?,
                    row.get::<_, String>(14)?,
                    row.get::<_, Option<String>>(15)?,
                ))
            })
            .map_err(|e| db_error(&e))?;
        let mut out = Vec::new();
        for row in rows {
            let r = row.map_err(|e| db_error(&e))?;
            out.push(FiringRow {
                id: r.0,
                trigger_id: r.1,
                home: r.2,
                outcome: r.3,
                ting_type: r.4,
                key_suffix: r.5,
                data: serde_json::from_str(&r.6)?,
                metadata: serde_json::from_str(&r.7)?,
                delivery: serde_json::from_str(&r.8)?,
                state: r.9,
                attempts: r.10,
                next_attempt_ms: r.11,
                last_error: r.12.map(|t| serde_json::from_str(&t)).transpose()?,
                ting: r.13.map(|t| serde_json::from_str(&t)).transpose()?,
                created_at: r.14,
                sent_at: r.15,
            });
        }
        Ok(out)
    }

    const COLUMNS: &'static str = "id, trigger_id, home, outcome, ting_type, key_suffix, data, metadata, delivery, state, attempts, next_attempt_ms, last_error, ting, created_at, sent_at";

    /// Pending firings due now.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn due_firings(&self, now_ms: i64) -> Result<Vec<FiringRow>> {
        self.read_firings(
            &format!("SELECT {} FROM firings WHERE state = 'pending' AND next_attempt_ms <= ?1 ORDER BY created_at LIMIT 50", Self::COLUMNS),
            &[&now_ms],
        )
    }

    /// Earliest pending attempt time.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn next_due(&self) -> Result<Option<i64>> {
        self.conn()
            .query_row(
                "SELECT MIN(next_attempt_ms) FROM firings WHERE state = 'pending'",
                [],
                |row| row.get(0),
            )
            .map_err(|e| db_error(&e))
    }

    /// History, newest first.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn history(
        &self,
        home: Option<&str>,
        trigger: Option<&str>,
        limit: i64,
    ) -> Result<Vec<FiringRow>> {
        self.read_firings(
            &format!(
                "SELECT {} FROM firings WHERE (?1 IS NULL OR home = ?1) AND (?2 IS NULL OR trigger_id = ?2)
                 ORDER BY created_at DESC LIMIT ?3",
                Self::COLUMNS
            ),
            &[&home, &trigger, &limit],
        )
    }

    /// One firing.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn firing(&self, id: &str) -> Result<Option<FiringRow>> {
        Ok(self
            .read_firings(
                &format!("SELECT {} FROM firings WHERE id = ?1", Self::COLUMNS),
                &[&id],
            )?
            .pop())
    }

    /// Counts by state (for status).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn counts(&self) -> Result<Value> {
        let conn = self.conn();
        let count = |sql: &str| -> Result<i64> {
            conn.query_row(sql, [], |row| row.get(0))
                .map_err(|e| db_error(&e))
        };
        Ok(serde_json::json!({
            "active_triggers": count("SELECT COUNT(*) FROM triggers WHERE status = 'active'")?,
            "pending_deliveries": count("SELECT COUNT(*) FROM firings WHERE state = 'pending'")?,
            "failed_deliveries": count("SELECT COUNT(*) FROM firings WHERE state = 'failed'")?,
            "telemetry_backlog": count("SELECT COUNT(*) FROM telemetry")?,
        }))
    }

    /// Drops history older than `days` (sent/failed/local only).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn prune(&self, cutoff_rfc3339: &str) -> Result<()> {
        let conn = self.conn();
        conn.execute(
            "DELETE FROM firings WHERE state != 'pending' AND created_at < ?1",
            params![cutoff_rfc3339],
        )
        .map_err(|e| db_error(&e))?;
        conn.execute(
            "DELETE FROM triggers WHERE status != 'active' AND updated_at < ?1",
            params![cutoff_rfc3339],
        )
        .map_err(|e| db_error(&e))?;
        Ok(())
    }

    /// Queues telemetry events (bounded to 2 000; oldest dropped).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn push_telemetry(&self, events: &[Value]) -> Result<()> {
        let conn = self.conn();
        let now = i64::try_from(silicon_spotify_client::model::now_ms()).unwrap_or(0);
        for event in events {
            let id = event
                .get("id")
                .and_then(Value::as_str)
                .map_or_else(|| uuid::Uuid::now_v7().to_string(), str::to_owned);
            conn.execute(
                "INSERT OR IGNORE INTO telemetry(id, body, created_ms) VALUES(?1, ?2, ?3)",
                params![id, event.to_string(), now],
            )
            .map_err(|e| db_error(&e))?;
        }
        conn.execute(
            "DELETE FROM telemetry WHERE id NOT IN (SELECT id FROM telemetry ORDER BY created_ms DESC LIMIT 2000)",
            [],
        )
        .map_err(|e| db_error(&e))?;
        Ok(())
    }

    /// Takes up to [`TELEMETRY_PEEK`] telemetry events, oldest first.
    ///
    /// # Errors
    /// SQLite or JSON errors.
    pub fn peek_telemetry(&self) -> Result<Vec<(String, Value)>> {
        let conn = self.conn();
        let mut statement = conn
            .prepare(&format!(
                "SELECT id, body FROM telemetry ORDER BY created_ms, rowid LIMIT {TELEMETRY_PEEK}"
            ))
            .map_err(|e| db_error(&e))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| db_error(&e))?;
        let mut out = Vec::new();
        for row in rows {
            let (id, body) = row.map_err(|e| db_error(&e))?;
            out.push((id, serde_json::from_str(&body)?));
        }
        Ok(out)
    }

    /// Deletes relayed telemetry.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn drop_telemetry(&self, ids: &[String]) -> Result<()> {
        let conn = self.conn();
        for id in ids {
            conn.execute("DELETE FROM telemetry WHERE id = ?1", params![id])
                .map_err(|e| db_error(&e))?;
        }
        Ok(())
    }

    /// Clears the telemetry backlog (opt-out).
    ///
    /// # Errors
    /// SQLite errors.
    pub fn clear_telemetry(&self) -> Result<()> {
        self.conn()
            .execute("DELETE FROM telemetry", [])
            .map_err(|e| db_error(&e))?;
        Ok(())
    }
}
