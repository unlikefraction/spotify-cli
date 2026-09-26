//! In-process fakes of IAM and Ting for tests and local end-to-end runs (feature `dev`).
//!
//! Never compiled into release builds. `cargo run --example dev_backend --features dev` serves the
//! real router with these fakes so the CLI and daemon can be exercised end to end without an IAM
//! application secret; every Ting "sent" is appended to a JSONL file for inspection.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use silicon_iam_client::models;
use uuid::Uuid;

use crate::api::{AppState, TestingPlanes};
use crate::config::{Settings, TableKeys};
use crate::error::{AppError, AppResult};
use crate::identity::{Actor, AppSession, AuthContext, Identity, Proof, TingEndpoint};
use crate::store::Store;
use crate::telemetry::Recorder;
use crate::ting::TingApi;

/// Scopes a full consent grants.
pub const SCOPES: &str =
    "obo:ting:subscriptions.register obo:ting:tings.send self.identity.read self.profile.read";

#[derive(Clone)]
struct Grant {
    actor: String,
    org: String,
    scopes: String,
}

/// Fake IAM: SLTs `oac_<anything>` log in as `si:dev`; `si:<handle>` / `c:<handle>` log in as that actor.
#[derive(Default)]
pub struct FakeIam {
    access: Mutex<HashMap<String, Grant>>,
    refresh: Mutex<HashMap<String, Grant>>,
    /// Scopes to grant (tests can remove Ting scopes).
    pub scopes: Mutex<Option<String>>,
}

impl FakeTing {
    /// A fake that also appends every accepted send to `log` (JSONL).
    #[must_use]
    pub fn logging(log: Option<PathBuf>) -> Self {
        Self {
            log,
            ..Self::default()
        }
    }
}

impl FakeIam {
    fn issue(&self, grant: Grant) -> AppSession {
        let access = format!("oat_{}", Uuid::now_v7().simple());
        let refresh = format!("ort_{}", Uuid::now_v7().simple());
        self.access
            .lock()
            .expect("lock")
            .insert(access.clone(), grant.clone());
        self.refresh
            .lock()
            .expect("lock")
            .insert(refresh.clone(), grant.clone());
        AppSession {
            access_token: access,
            refresh_token: refresh,
            token_type: "Bearer",
            expires_in: 1800,
            scope: grant.scopes.clone(),
            actor: Actor {
                kind: if grant.actor.starts_with("si:") {
                    "silicon".into()
                } else {
                    "carbon".into()
                },
                public_id: grant.actor,
            },
            org_id: grant.org.clone(),
            org_ids: vec![grant.org],
            testing_environment_id: None,
        }
    }
}

#[async_trait]
impl Identity for FakeIam {
    async fn login(&self, slt: &SecretString, _key: &str) -> AppResult<AppSession> {
        let value = slt.expose_secret();
        let actor = if value.starts_with("si:") || value.starts_with("c:") {
            value.to_owned()
        } else if value.starts_with("oac_") {
            "si:dev".to_owned()
        } else {
            return Err(AppError::unauthenticated());
        };
        let scopes = self
            .scopes
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or_else(|| SCOPES.to_owned());
        Ok(self.issue(Grant {
            actor,
            org: "tos".into(),
            scopes,
        }))
    }

    async fn refresh(&self, token: &SecretString, _key: &str) -> AppResult<AppSession> {
        let grant = self
            .refresh
            .lock()
            .expect("lock")
            .remove(token.expose_secret())
            .ok_or_else(AppError::unauthenticated)?;
        Ok(self.issue(grant))
    }

    async fn logout(&self, token: &SecretString, _key: &str) -> AppResult<()> {
        self.refresh
            .lock()
            .expect("lock")
            .remove(token.expose_secret());
        Ok(())
    }

    async fn authenticate(&self, token: &SecretString, org_id: &str) -> AppResult<AuthContext> {
        let grant = self
            .access
            .lock()
            .expect("lock")
            .get(token.expose_secret())
            .cloned()
            .ok_or_else(AppError::unauthenticated)?;
        if grant.org != org_id {
            return Err(AppError::unauthenticated());
        }
        Ok(AuthContext {
            actor: Actor {
                kind: if grant.actor.starts_with("si:") {
                    "silicon".into()
                } else {
                    "carbon".into()
                },
                public_id: grant.actor.clone(),
            },
            org_id: grant.org.clone(),
            membership_id: format!("{}[{}]", grant.actor, grant.org),
            session_id: None,
            scopes: grant.scopes.split_whitespace().map(str::to_owned).collect(),
            token: token.clone(),
        })
    }

    async fn ting_proof(
        &self,
        context: &AuthContext,
        endpoint: TingEndpoint,
        _body: &[u8],
        _attempt_key: &str,
    ) -> AppResult<Proof> {
        if !context.has(&endpoint.scope()) || !context.has("self.identity.read") {
            return Err(AppError::new(
                axum::http::StatusCode::FORBIDDEN,
                "reconsent_required",
                "missing Ting scopes",
                "log in again",
            ));
        }
        Ok(Proof {
            token: SecretString::from(format!("proof_{}", Uuid::now_v7().simple())),
            testing: None,
        })
    }

    fn verify_webhook(
        &self,
        _headers: &axum::http::HeaderMap,
        _body: &[u8],
    ) -> AppResult<models::WebhookEvent> {
        Err(AppError::unauthenticated())
    }

    fn environment_id(&self) -> Option<Uuid> {
        None
    }
}

/// Fake Ting: accepts registrations and sends, keeping (and optionally logging) every send.
#[derive(Default)]
pub struct FakeTing {
    /// Every accepted send body.
    pub sent: Mutex<Vec<Value>>,
    /// Keys seen (idempotent replay → 200).
    keys: Mutex<HashMap<String, Value>>,
    /// Registered recipients.
    pub recipients: Mutex<Vec<String>>,
    /// Append sends here (JSONL) when set.
    pub log: Option<PathBuf>,
    /// Refuse sends with this code (tests).
    pub refuse: Mutex<Option<String>>,
}

#[async_trait]
impl TingApi for FakeTing {
    async fn post(&self, path: &str, body: Vec<u8>, proof: &Proof) -> AppResult<(u16, Value)> {
        if !proof.token.expose_secret().starts_with("proof_") {
            return Ok((
                401,
                json!({"error": {"code": "invalid_proof", "message": "bad proof"}}),
            ));
        }
        let value: Value = serde_json::from_slice(&body).map_err(AppError::internal)?;
        match path {
            "/v1/subscriptions" => {
                let recipient = value["for"].as_str().unwrap_or_default().to_owned();
                self.recipients
                    .lock()
                    .expect("lock")
                    .push(recipient.clone());
                Ok((
                    201,
                    json!({"id": format!("sub_{}", Uuid::now_v7().simple()), "app_id": value["app_id"], "for": recipient, "active": true, "required_delivery": false}),
                ))
            }
            "/v1/tings" => {
                if let Some(code) = self.refuse.lock().expect("lock").clone() {
                    return Ok((
                        403,
                        json!({"error": {"code": code, "message": "refused by fake"}}),
                    ));
                }
                let recipient = value["for"].as_str().unwrap_or_default();
                if !self
                    .recipients
                    .lock()
                    .expect("lock")
                    .iter()
                    .any(|r| r == recipient)
                {
                    return Ok((
                        403,
                        json!({"error": {"code": "recipient_not_registered", "message": "no grant"}}),
                    ));
                }
                let key = value["key"].as_str().unwrap_or_default().to_owned();
                if let Some(previous) = self.keys.lock().expect("lock").get(&key) {
                    return Ok((200, previous.clone()));
                }
                let accepted = json!({"id": format!("msg_{}", Uuid::now_v7().simple()), "created_at": silicon_spotify_client::model::now_rfc3339(), "status": "accepted", "key": key, "silent": false});
                self.keys
                    .lock()
                    .expect("lock")
                    .insert(key, accepted.clone());
                self.sent.lock().expect("lock").push(value.clone());
                if let Some(log) = &self.log {
                    use std::io::Write as _;
                    if let Ok(mut file) = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(log)
                    {
                        let _ = writeln!(file, "{}", json!({"accepted": accepted, "ting": value}));
                    }
                }
                Ok((202, accepted))
            }
            _ => Ok((404, json!({"error": {"code": "not_found"}}))),
        }
    }
}

struct NoTesting;

#[async_trait]
impl TestingPlanes for NoTesting {
    async fn resolve(&self, _secret: &str) -> AppResult<Arc<dyn Identity>> {
        Err(AppError::invalid(
            "The dev backend has no testing planes.",
            "",
        ))
    }
}

/// Settings for the fakes.
#[must_use]
pub fn settings() -> Settings {
    Settings {
        bind: "127.0.0.1:8787".parse().expect("addr"),
        database_path: PathBuf::from(":memory:"),
        public_origin: "http://127.0.0.1:8787".into(),
        iam_url: url::Url::parse("http://127.0.0.1:1").expect("url"),
        app_id: "spotify".into(),
        app_secret: SecretString::from(format!("ask_{}", "x".repeat(43))),
        webhook_secret: SecretString::from("w".repeat(32)),
        webhook_key_version: 1,
        iam_timeout: std::time::Duration::from_secs(5),
        ting_url: url::Url::parse("http://127.0.0.1:1").expect("url"),
        ting_timeout: std::time::Duration::from_secs(5),
        telemetry: false,
        table_keys: TableKeys::default(),
        telemetry_home: std::env::temp_dir().join("silicon-spotify-dev-telemetry"),
        telemetry_url: "http://127.0.0.1:1".into(),
        github_token: None,
        github_repository: "unlikefraction/spotify-cli".into(),
        web_origins: vec!["http://localhost:4321".into()],
    }
}

/// State wired with fakes.
///
/// # Errors
/// SQLite errors.
pub fn state(ting: Arc<FakeTing>, iam: Arc<FakeIam>) -> anyhow::Result<Arc<AppState>> {
    let settings = settings();
    Ok(Arc::new(AppState {
        telemetry: Recorder::new(&settings),
        settings,
        identity: iam,
        testing: Arc::new(NoTesting),
        ting,
        store: Arc::new(Store::memory()?),
        http: reqwest::Client::new(),
    }))
}
