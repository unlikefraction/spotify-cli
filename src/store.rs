//! Backend SQLite: bug reports and IAM webhook dedupe. No credentials are stored.

use std::path::Path;
use std::sync::Mutex;

use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::Value;

use crate::error::{AppError, AppResult};

/// Thread-safe SQLite handle.
pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    /// Opens and migrates.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        let conn = Connection::open(path)?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// In-memory store for tests.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn memory() -> anyhow::Result<Self> {
        let conn = Connection::open_in_memory()?;
        Self::migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    fn migrate(conn: &Connection) -> rusqlite::Result<()> {
        conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA busy_timeout=5000;
             CREATE TABLE IF NOT EXISTS reports (
               id TEXT PRIMARY KEY, idempotency_key TEXT UNIQUE, actor TEXT, org_id TEXT, message TEXT NOT NULL,
               pr TEXT, body TEXT NOT NULL, status TEXT NOT NULL, issue_url TEXT, environment TEXT, created_at TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS webhook_events (event_id TEXT PRIMARY KEY, event_type TEXT NOT NULL, received_at TEXT NOT NULL);",
        )
    }

    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Whether the database answers.
    #[must_use]
    pub fn healthy(&self) -> bool {
        self.conn()
            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .is_ok()
    }

    /// Saves a report; replays by idempotency key.
    ///
    /// # Errors
    /// SQLite errors.
    #[allow(clippy::too_many_arguments)]
    pub fn save_report(
        &self,
        id: &str,
        key: &str,
        actor: Option<&str>,
        org: Option<&str>,
        message: &str,
        pr: Option<&str>,
        body: &Value,
        environment: &str,
    ) -> AppResult<(String, String, Option<String>)> {
        let conn = self.conn();
        if let Some(existing) = conn
            .query_row(
                "SELECT id, status, issue_url FROM reports WHERE idempotency_key = ?1",
                params![key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(AppError::internal)?
        {
            return Ok(existing);
        }
        conn.execute(
            "INSERT INTO reports(id, idempotency_key, actor, org_id, message, pr, body, status, environment, created_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, 'stored', ?8, ?9)",
            params![id, key, actor, org, message, pr, body.to_string(), environment, silicon_spotify_client::model::now_rfc3339()],
        )
        .map_err(AppError::internal)?;
        Ok((id.to_owned(), "stored".into(), None))
    }

    /// Records the GitHub issue.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn mark_filed(&self, id: &str, url: &str) -> AppResult<()> {
        self.conn()
            .execute(
                "UPDATE reports SET status = 'filed', issue_url = ?2 WHERE id = ?1",
                params![id, url],
            )
            .map_err(AppError::internal)?;
        Ok(())
    }

    /// Returns false when this webhook event was already received.
    ///
    /// # Errors
    /// SQLite errors.
    pub fn first_webhook(&self, event_id: &str, event_type: &str) -> AppResult<bool> {
        let inserted = self
            .conn()
            .execute(
                "INSERT OR IGNORE INTO webhook_events(event_id, event_type, received_at) VALUES(?1, ?2, ?3)",
                params![event_id, event_type, silicon_spotify_client::model::now_rfc3339()],
            )
            .map_err(AppError::internal)?;
        Ok(inserted == 1)
    }
}
