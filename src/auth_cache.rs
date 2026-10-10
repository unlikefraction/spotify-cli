//! Encrypted replay results for one-use Silicon Accounts exchanges.
use crate::{AppError, AppResult};
use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use rusqlite::{Connection, OptionalExtension as _, params};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::Value;
use std::path::Path;
use std::sync::Mutex;

pub(crate) fn required() -> AppError {
    AppError::forbidden(
        "Ting user verification was refused.",
        "Sign in to Silicon Accounts and grant this account access to Ting, then run `spotify ting register`.",
    )
}
pub(crate) struct Store {
    conn: Mutex<Connection>,
    cipher: Aes256Gcm,
}
impl Store {
    pub(crate) fn open(path: &Path, key: &SecretString) -> anyhow::Result<Self> {
        let key = key.expose_secret();
        anyhow::ensure!(
            key.len() == 64 && key.bytes().all(|b| b.is_ascii_hexdigit()),
            "SPOTIFY_ENCRYPTION_KEY must be 64 hex characters"
        );
        let bytes = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&key[i..i + 2], 16))
            .collect::<Result<Vec<_>, _>>()?;
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000; CREATE TABLE IF NOT EXISTS accounts_exchanges (key TEXT PRIMARY KEY, response BLOB, created_at INTEGER NOT NULL);")?;
        Ok(Self {
            conn: Mutex::new(conn),
            cipher: Aes256Gcm::new_from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid encryption key"))?,
        })
    }
    pub(crate) fn cached(&self, key: &str) -> AppResult<Option<Value>> {
        let conn = self
            .conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let row: Option<(Option<Vec<u8>>, i64)> = conn
            .query_row(
                "SELECT response,created_at FROM accounts_exchanges WHERE key=?1",
                [key],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(AppError::internal)?;
        let Some((bytes, created)) = row else {
            return Ok(None);
        };
        let Some(bytes) = bytes else {
            return Err(AppError::conflict(
                "The previous token exchange has an uncertain result.",
                "Sign in again with a fresh short-lived token; retrying the consumed refresh token could revoke your session.",
            ));
        };
        if bytes.len() < 28 {
            return Err(AppError::dependency("accounts"));
        }
        let plain = self
            .cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: key.as_bytes(),
                },
            )
            .map_err(|_| AppError::dependency("accounts"))?;
        let mut value: Value = serde_json::from_slice(&plain).map_err(AppError::internal)?;
        let remaining = value["expires_in"].as_i64().unwrap_or(0)
            - (time::OffsetDateTime::now_utc().unix_timestamp() - created).max(0);
        value["expires_in"] = remaining.max(0).into();
        Ok(Some(value))
    }
    pub(crate) fn reserve(&self, key: &str) -> AppResult<()> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execute(
                "INSERT INTO accounts_exchanges(key,created_at) VALUES(?1,?2)",
                params![key, time::OffsetDateTime::now_utc().unix_timestamp()],
            )
            .map_err(AppError::internal)?;
        Ok(())
    }
    pub(crate) fn complete(&self, key: &str, value: &Value) -> AppResult<()> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let plain = serde_json::to_vec(value).map_err(AppError::internal)?;
        let encrypted = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &plain,
                    aad: key.as_bytes(),
                },
            )
            .map_err(|_| AppError::dependency("accounts"))?;
        let bytes = [nonce.as_slice(), &encrypted].concat();
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execute(
                "UPDATE accounts_exchanges SET response=?2,created_at=?3 WHERE key=?1",
                params![key, bytes, time::OffsetDateTime::now_utc().unix_timestamp()],
            )
            .map_err(AppError::internal)?;
        Ok(())
    }
    pub(crate) fn release(&self, key: &str) -> AppResult<()> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execute(
                "DELETE FROM accounts_exchanges WHERE key=?1 AND response IS NULL",
                [key],
            )
            .map_err(AppError::internal)?;
        Ok(())
    }
}
