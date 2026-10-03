//! Durable, independently consented Ting roots. Secrets are sealed with context-bound AES-GCM;
//! SQLite leases serialize refresh and exchange across processes, including uncertain retries.
use std::path::Path;
use std::sync::Mutex;

use aes_gcm::{
    Aes256Gcm, Nonce,
    aead::{Aead, AeadCore, KeyInit, OsRng, Payload},
};
use axum::http::StatusCode;
use rusqlite::{Connection, OptionalExtension as _, params};
use secrecy::{ExposeSecret as _, SecretString};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use silicon_iam_client::{IdempotencyKey, Mutation, models};
use uuid::Uuid;

use crate::identity::{Actor, AuthContext, Iam, TingAccess, TingEndpoint};
use crate::{AppError, AppResult};

const ENDPOINTS: [&str; 2] = ["subscriptions.register", "tings.send"];

pub(crate) fn required() -> AppError {
    AppError::new(
        StatusCode::FORBIDDEN,
        "reconsent_required",
        "Ting permission is required for this feature.",
        "Run `spotify ting authorize`, approve in IAM, then complete with the one-use code. Your login and pending action are preserved.",
    )
}
fn invalid() -> AppError {
    AppError::dependency("iam")
}
fn busy() -> AppError {
    AppError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "obo_operation_in_progress",
        "Ting authorization is being updated.",
        "Retry the same operation with its original key.",
    )
}
fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
fn mutation(key: &str) -> AppResult<Mutation> {
    IdempotencyKey::parse(key)
        .map(Mutation::with_key)
        .map_err(|_| invalid())
}
fn failure(error: &silicon_iam_client::Error) -> AppError {
    match error.api() {
        Some(e) if e.status == 412 => AppError::new(
            StatusCode::PRECONDITION_FAILED,
            "reconsent_required",
            "The Ting permission graph changed.",
            "Start a new Ting authorization and review it in IAM. Your pending action is preserved.",
        ),
        Some(e)
            if matches!(e.status, 400 | 401 | 403 | 404 | 410)
                && !matches!(e.code.as_str(), "invalid_client" | "unauthorized_client") =>
        {
            required()
        }
        _ => crate::identity::map_error(error),
    }
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
        let bytes: Vec<u8> = (0..64)
            .step_by(2)
            .map(|i| u8::from_str_radix(&key[i..i + 2], 16))
            .collect::<Result<_, _>>()?;
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; PRAGMA busy_timeout=5000;
            CREATE TABLE IF NOT EXISTS obo_locks (scope TEXT PRIMARY KEY, owner TEXT NOT NULL, expires INTEGER NOT NULL);
            CREATE TABLE IF NOT EXISTS obo_requests (scope TEXT NOT NULL, id TEXT NOT NULL, request_key TEXT NOT NULL, payload BLOB NOT NULL, consent TEXT, code_hash TEXT, completed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY(scope,id), UNIQUE(scope,request_key));
            CREATE TABLE IF NOT EXISTS obo_roots (scope TEXT NOT NULL, endpoint TEXT NOT NULL, credentials BLOB NOT NULL, PRIMARY KEY(scope,endpoint));
            CREATE TABLE IF NOT EXISTS obo_operations (scope TEXT NOT NULL, endpoint TEXT NOT NULL, operation_key TEXT NOT NULL, binding TEXT NOT NULL, PRIMARY KEY(scope,endpoint,operation_key));")?;
        Ok(Self {
            conn: Mutex::new(conn),
            cipher: Aes256Gcm::new_from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid encryption key"))?,
        })
    }
    fn conn(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
    fn seal(&self, aad: &str, value: &impl Serialize) -> AppResult<Vec<u8>> {
        let nonce = Aes256Gcm::generate_nonce(&mut OsRng);
        let bytes = serde_json::to_vec(value).map_err(AppError::internal)?;
        let ciphertext = self
            .cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: &bytes,
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| invalid())?;
        Ok([nonce.as_slice(), &ciphertext].concat())
    }
    fn open_value<T: for<'a> Deserialize<'a>>(&self, aad: &str, bytes: &[u8]) -> AppResult<T> {
        if bytes.len() < 28 {
            return Err(invalid());
        }
        let plain = self
            .cipher
            .decrypt(
                Nonce::from_slice(&bytes[..12]),
                Payload {
                    msg: &bytes[12..],
                    aad: aad.as_bytes(),
                },
            )
            .map_err(|_| invalid())?;
        serde_json::from_slice(&plain).map_err(|_| invalid())
    }
}
// ponytail: serialize one account's OBO operations; use per-family leases if contention matters.
struct Lease<'a> {
    store: &'a Store,
    scope: String,
    owner: String,
}
impl<'a> Lease<'a> {
    fn acquire(iam: &'a Iam, context: &AuthContext) -> AppResult<Self> {
        let scope = json!([
            iam.app_id,
            iam.environment_id,
            iam.generation,
            context.org_id,
            context.actor.kind,
            context.actor.public_id
        ])
        .to_string();
        let owner = Uuid::new_v4().to_string();
        let n=iam.obo.conn().execute("INSERT INTO obo_locks(scope,owner,expires) VALUES(?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET owner=excluded.owner,expires=excluded.expires WHERE obo_locks.expires<?4",params![scope,owner,now()+300,now()]).map_err(AppError::internal)?;
        if n != 1 {
            return Err(busy());
        }
        Ok(Self {
            store: &iam.obo,
            scope,
            owner,
        })
    }
    fn call<T>(&self, f: impl FnOnce(&Connection) -> AppResult<T>) -> AppResult<T> {
        let mut conn = self.store.conn();
        let tx = conn
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(AppError::internal)?;
        let held:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM obo_locks WHERE scope=?1 AND owner=?2 AND expires>=?3)",params![self.scope,self.owner,now()],|r|r.get(0)).map_err(AppError::internal)?;
        if !held {
            return Err(busy());
        }
        let result = f(&tx)?;
        tx.commit().map_err(AppError::internal)?;
        Ok(result)
    }
    fn aad(&self, kind: &str, id: &str) -> String {
        json!(["spotify-obo-v1", self.scope, kind, id]).to_string()
    }
    fn request(&self, id: &str) -> AppResult<Request> {
        self.call(|c|c.query_row("SELECT id,payload,consent,code_hash,completed FROM obo_requests WHERE scope=?1 AND id=?2",params![self.scope,id],|r|Ok(Request{id:r.get(0)?,payload:r.get(1)?,consent:r.get(2)?,code_hash:r.get(3)?,completed:r.get(4)?})).optional().map_err(AppError::internal)?.ok_or_else(required))
    }
    fn save_root(&self, endpoint: &str, credentials: &Credentials) -> AppResult<()> {
        let bytes = self.store.seal(&self.aad("root", endpoint), credentials)?;
        self.call(|c| { c.execute("INSERT INTO obo_roots(scope,endpoint,credentials) VALUES(?1,?2,?3) ON CONFLICT(scope,endpoint) DO UPDATE SET credentials=excluded.credentials",params![self.scope,endpoint,bytes]).map_err(AppError::internal)?; Ok(()) })
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        let _ = self.store.conn().execute(
            "DELETE FROM obo_locks WHERE scope=?1 AND owner=?2",
            params![self.scope, self.owner],
        );
    }
}
struct Request {
    id: String,
    payload: Vec<u8>,
    consent: Option<String>,
    code_hash: Option<String>,
    completed: bool,
}
impl Request {
    fn detail(&self) -> AppResult<models::OboConsentDetail> {
        serde_json::from_str(self.consent.as_deref().ok_or_else(busy)?).map_err(|_| invalid())
    }
    fn view(&self, roots: Vec<Value>) -> AppResult<Value> {
        Ok(
            json!({"request_id":self.id,"authorization":self.detail()?,"completed":self.completed,"roots":roots}),
        )
    }
}
#[derive(Serialize, Deserialize)]
struct Credentials {
    pair: models::OboTokenPair,
    refresh_key: Option<String>,
}
fn safe(pair: &models::OboTokenPair) -> Value {
    json!({"grant_id":pair.grant_id,"audience":pair.audience,"endpoint_id":pair.endpoint_id,"actor":pair.actor,"org_id":pair.org_id,"expires_at":pair.expires_at.unix_timestamp()})
}
fn check_consent(
    iam: &Iam,
    context: &AuthContext,
    detail: &models::OboConsentDetail,
) -> AppResult<()> {
    if detail.id.is_nil()
        || detail.app_id != iam.app_id
        || detail.org_id != context.org_id
        || detail.actor.public_id != context.actor.public_id
        || serde_json::to_value(&detail.actor.type_field).ok() != Some(json!(context.actor.kind))
        || detail.redirect_uri.is_some()
        || detail.state.is_some()
    {
        return Err(invalid());
    }
    if let Some(value) = &detail.authorization_url {
        let url = url::Url::parse(value).map_err(|_| invalid())?;
        if url.scheme() != "https"
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
        {
            return Err(invalid());
        }
    }
    Ok(())
}
async fn validate_pair(iam: &Iam, pair: &models::OboTokenPair) -> AppResult<()> {
    let actor = pair.actor.as_ref().ok_or_else(invalid)?;
    let kind = serde_json::to_value(&actor.type_field).map_err(|_| invalid())?;
    if pair.audience != "ting"
        || !ENDPOINTS.contains(&pair.endpoint_id.as_str())
        || pair.grant_id.is_nil()
        || !crate::identity::well_formed(&pair.access_token, "oba_")
        || !crate::identity::well_formed(&pair.refresh_token, "obr_")
        || pair.token_type != models::OboTokenPairTokenType::Bearer
        || pair.expires_in <= 0
        || pair.expires_at.unix_timestamp() <= now()
        || !silicon_spotify_client::valid_handle(&pair.org_id, 3, 50)
        || !((kind == "silicon" && actor.public_id.starts_with("si:"))
            || (kind == "carbon" && actor.public_id.starts_with("c:")))
        || !pair
            .scope
            .split_whitespace()
            .any(|s| s == format!("obo:ting:{}", pair.endpoint_id))
    {
        return Err(invalid());
    }
    match (iam.environment_id, &pair.testing_context) {
        (None, None) => {}
        (Some(expected), Some(t)) if t.app_id == "ting" => {
            let selected = iam
                .client
                .with_credential(silicon_iam_client::Credential::application(
                    "ting",
                    &t.app_secret,
                ))
                .with_environment(
                    silicon_iam_client::EnvironmentKey::new(t.iam_test_key.clone())
                        .map_err(|e| failure(&e))?,
                )
                .applications()
                .testing_context()
                .await
                .map_err(|e| failure(&e))?;
            if selected.environment_id != expected
                || selected.application.app_id != "ting"
                || selected
                    .environment
                    .as_ref()
                    .map(|e| json!([e.key_generation, e.cleaned_at]).to_string())
                    .as_ref()
                    != Some(&iam.generation)
            {
                return Err(invalid());
            }
        }
        _ => return Err(invalid()),
    }
    Ok(())
}

pub(crate) async fn start(iam: &Iam, context: &AuthContext, key: &str) -> AppResult<Value> {
    let lease = Lease::acquire(iam, context)?;
    let id = Uuid::new_v4().to_string();
    let body = models::OboAuthorizationRequest {
        redirect_uri: None,
        state: None,
        subject_token: context.token.expose_secret().to_owned(),
        org_id: context.org_id.clone(),
        endpoints: ENDPOINTS
            .iter()
            .map(|e| models::OboAuthorizationEndpoint {
                audience: "ting".into(),
                endpoint_id: (*e).into(),
            })
            .collect(),
    };
    let payload = iam.obo.seal(&lease.aad("request", &id), &body)?;
    let id = lease.call(|c| {
        c.execute(
            "INSERT OR IGNORE INTO obo_requests(scope,id,request_key,payload) VALUES(?1,?2,?3,?4)",
            params![lease.scope, id, key, payload],
        )
        .map_err(AppError::internal)?;
        c.query_row(
            "SELECT id FROM obo_requests WHERE scope=?1 AND request_key=?2",
            params![lease.scope, key],
            |r| r.get::<_, String>(0),
        )
        .map_err(AppError::internal)
    })?;
    let row = lease.request(&id)?;
    if row.consent.is_some() {
        return row.view(vec![]);
    }
    let body = iam
        .obo
        .open_value(&lease.aad("request", &id), &row.payload)?;
    let detail = iam
        .client
        .obo()
        .authorize(&body, &mutation(&format!("spotify-obo-start-{id}"))?)
        .await
        .map_err(|e| failure(&e))?;
    check_consent(iam, context, &detail)?;
    let consent = serde_json::to_string(&detail).map_err(AppError::internal)?;
    lease.call(|c| {
        c.execute(
            "UPDATE obo_requests SET consent=?3,payload=x'' WHERE scope=?1 AND id=?2",
            params![lease.scope, id, consent],
        )
        .map_err(AppError::internal)?;
        Ok(())
    })?;
    lease.request(&id)?.view(vec![])
}

pub(crate) async fn status(iam: &Iam, context: &AuthContext, id: &str) -> AppResult<Value> {
    let lease = Lease::acquire(iam, context)?;
    let row = lease.request(id)?;
    let auth = row.detail()?.id;
    let detail = iam
        .client
        .obo()
        .authorization(auth)
        .await
        .map_err(|e| failure(&e))?;
    check_consent(iam, context, &detail)?;
    if detail.id != auth {
        return Err(invalid());
    }
    let consent = serde_json::to_string(&detail).map_err(AppError::internal)?;
    lease.call(|c| {
        c.execute(
            "UPDATE obo_requests SET consent=?3 WHERE scope=?1 AND id=?2",
            params![lease.scope, id, consent],
        )
        .map_err(AppError::internal)?;
        Ok(())
    })?;
    lease.request(id)?.view(vec![])
}

pub(crate) async fn complete(
    iam: &Iam,
    context: &AuthContext,
    id: &str,
    code: &str,
) -> AppResult<Value> {
    if code.is_empty() || code.len() > 1024 || !code.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AppError::invalid(
            "Invalid authorization code.",
            "Use the one-use code displayed by IAM.",
        ));
    }
    let lease = Lease::acquire(iam, context)?;
    let row = lease.request(id)?;
    let hash = blake3::hash(code.as_bytes()).to_hex().to_string();
    if row.code_hash.as_ref().is_some_and(|h| h != &hash) {
        return Err(AppError::conflict(
            "This request already used another code.",
            "Retry with the original code, or start a new authorization.",
        ));
    }
    if row.completed {
        return row.view(vec![]);
    }
    let auth = row.detail()?.id;
    let detail = iam
        .client
        .obo()
        .authorization(auth)
        .await
        .map_err(|e| failure(&e))?;
    check_consent(iam, context, &detail)?;
    if detail.id != auth
        || !matches!(
            detail.status,
            models::OboConsentDetailStatus::Approved | models::OboConsentDetailStatus::Exchanged
        )
    {
        return Err(required());
    }
    lease.call(|c| {
        c.execute(
            "UPDATE obo_requests SET code_hash=?3 WHERE scope=?1 AND id=?2",
            params![lease.scope, id, hash],
        )
        .map_err(AppError::internal)?;
        Ok(())
    })?;
    let response = iam
        .client
        .obo()
        .exchange_code(auth, code, &mutation(&format!("spotify-obo-code-{id}"))?)
        .await
        .map_err(|e| failure(&e))?;
    if response.items.len() != ENDPOINTS.len() {
        return Err(invalid());
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut roots = Vec::new();
    let mut sealed = Vec::new();
    let mut destination = None;
    for pair in response.items {
        validate_pair(iam, &pair).await?;
        if !seen.insert(pair.endpoint_id.clone()) {
            return Err(invalid());
        }
        let selected = json!([pair.org_id, pair.actor]);
        if destination.as_ref().is_some_and(|d| d != &selected) {
            return Err(invalid());
        }
        destination = Some(selected);
        roots.push(safe(&pair));
        let endpoint = pair.endpoint_id.clone();
        let bytes = iam.obo.seal(
            &lease.aad("root", &endpoint),
            &Credentials {
                pair,
                refresh_key: None,
            },
        )?;
        sealed.push((endpoint, bytes));
    }
    let consent = serde_json::to_string(&detail).map_err(AppError::internal)?;
    lease.call(|c|{
        for (endpoint,bytes) in sealed { c.execute("INSERT INTO obo_roots(scope,endpoint,credentials) VALUES(?1,?2,?3) ON CONFLICT(scope,endpoint) DO UPDATE SET credentials=excluded.credentials",params![lease.scope,endpoint,bytes]).map_err(AppError::internal)?; }
        c.execute("UPDATE obo_requests SET completed=1,consent=?3 WHERE scope=?1 AND id=?2",params![lease.scope,id,consent]).map_err(AppError::internal)?; Ok(())
    })?;
    lease.request(id)?.view(roots)
}

pub(crate) async fn access(
    iam: &Iam,
    context: &AuthContext,
    endpoint: TingEndpoint,
    body: &[u8],
    operation: &str,
) -> AppResult<TingAccess> {
    let lease = Lease::acquire(iam, context)?;
    let endpoint = endpoint.id();
    let bytes = lease
        .call(|c| {
            c.query_row(
                "SELECT credentials FROM obo_roots WHERE scope=?1 AND endpoint=?2",
                params![lease.scope, endpoint],
                |r| r.get::<_, Vec<u8>>(0),
            )
            .optional()
            .map_err(AppError::internal)
        })?
        .ok_or_else(required)?;
    let mut credentials: Credentials = iam.obo.open_value(&lease.aad("root", endpoint), &bytes)?;
    if credentials.refresh_key.is_some()
        || credentials.pair.expires_at.unix_timestamp() <= now() + 60
    {
        let key = credentials
            .refresh_key
            .get_or_insert_with(|| format!("spotify-obo-refresh-{}", Uuid::new_v4()))
            .clone();
        lease.save_root(endpoint, &credentials)?;
        let mut response = iam
            .client
            .obo()
            .refresh(&credentials.pair.refresh_token, &mutation(&key)?)
            .await
            .map_err(|e| failure(&e))?;
        if response.items.len() != 1 {
            return Err(invalid());
        }
        let next = response.items.remove(0);
        validate_pair(iam, &next).await?;
        if next.grant_id != credentials.pair.grant_id
            || next.endpoint_id != endpoint
            || next.org_id != credentials.pair.org_id
            || serde_json::to_value(&next.actor).ok()
                != serde_json::to_value(&credentials.pair.actor).ok()
        {
            return Err(invalid());
        }
        credentials = Credentials {
            pair: next,
            refresh_key: None,
        };
        lease.save_root(endpoint, &credentials)?;
    }
    validate_pair(iam, &credentials.pair).await?;
    let pair = credentials.pair;
    let actor = pair.actor.ok_or_else(invalid)?;
    let binding = json!([
        blake3::hash(body).to_hex().as_str(),
        pair.org_id,
        actor.public_id
    ])
    .to_string();
    lease.call(|c|{
        c.execute("INSERT OR IGNORE INTO obo_operations(scope,endpoint,operation_key,binding) VALUES(?1,?2,?3,?4)",params![lease.scope,endpoint,operation,binding]).map_err(AppError::internal)?;
        let saved:String=c.query_row("SELECT binding FROM obo_operations WHERE scope=?1 AND endpoint=?2 AND operation_key=?3",params![lease.scope,endpoint,operation],|r|r.get(0)).map_err(AppError::internal)?;
        if saved!=binding { return Err(AppError::conflict("This pending action belongs to its original Ting account, organization and payload.","Restore its original permission or create a new action.")); } Ok(())
    })?;
    Ok(TingAccess {
        actor: Actor {
            kind: if actor.public_id.starts_with("si:") {
                "silicon".into()
            } else {
                "carbon".into()
            },
            public_id: actor.public_id,
        },
        org_id: pair.org_id,
        token: SecretString::from(pair.access_token),
        testing: pair.testing_context.map(|t| {
            (
                SecretString::from(t.app_secret),
                SecretString::from(t.iam_test_key),
            )
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sealed_credentials_cannot_move_contexts_or_keys() {
        let store = Store::open(Path::new(":memory:"), &SecretString::from("11".repeat(32)))
            .expect("store");
        let value = json!({"refresh_token":"obr_secret"});
        let sealed = store
            .seal("account/org/generation/root", &value)
            .expect("seal");
        assert_eq!(
            store
                .open_value::<Value>("account/org/generation/root", &sealed)
                .expect("open"),
            value
        );
        assert!(
            store
                .open_value::<Value>("another/account", &sealed)
                .is_err()
        );
        assert_ne!(
            store
                .seal("account/org/generation/root", &value)
                .expect("seal again"),
            sealed
        );
        let other = Store::open(Path::new(":memory:"), &SecretString::from("22".repeat(32)))
            .expect("store");
        assert!(
            other
                .open_value::<Value>("account/org/generation/root", &sealed)
                .is_err()
        );
    }
}
