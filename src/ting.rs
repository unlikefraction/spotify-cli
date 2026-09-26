//! Ting calls: recipient registration and sends, with exact-bytes bodies.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::identity::{AuthContext, Identity, Proof, TingEndpoint};

/// Largest Ting send body.
pub const MAX_SEND_BYTES: usize = 256 * 1024;

/// HTTP transport to Ting (a trait so tests can fake it).
#[async_trait]
pub trait TingApi: Send + Sync {
    /// POSTs exact bytes with a proof; returns (status, JSON body).
    async fn post(&self, path: &str, body: Vec<u8>, proof: &Proof) -> AppResult<(u16, Value)>;
}

/// The real transport.
pub struct TingHttp {
    client: reqwest::Client,
    base: Url,
}

impl TingHttp {
    /// Builds a client for `base`.
    ///
    /// # Errors
    /// HTTP client setup.
    pub fn new(base: Url, timeout: Duration) -> anyhow::Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .timeout(timeout)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(format!("silicon-spotify/{}", env!("CARGO_PKG_VERSION")))
                .build()?,
            base,
        })
    }
}

#[async_trait]
impl TingApi for TingHttp {
    async fn post(&self, path: &str, body: Vec<u8>, proof: &Proof) -> AppResult<(u16, Value)> {
        let url = self.base.join(path).map_err(AppError::internal)?;
        let mut request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("Ting-Client-Version", env!("CARGO_PKG_VERSION"))
            .bearer_auth(proof.token.expose_secret())
            .body(body);
        if let Some((secret, key)) = &proof.testing {
            request = request
                .header("IAM_TEST_APP_SECRET", secret.expose_secret())
                .header("X-Testing-Environment-Key", key.expose_secret());
        }
        let response = request
            .send()
            .await
            .map_err(|_| AppError::dependency("ting"))?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|_| AppError::dependency("ting"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(AppError::dependency("ting"));
        }
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok((status, value))
    }
}

fn ting_error(status: u16, body: &Value) -> AppError {
    let code = body
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("ting_error");
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Ting refused the request.");
    let (http, hint) = match code {
        "recipient_not_registered" => (
            403,
            "Register again with `spotify ting register` (or log in again).",
        ),
        "not_found" => (
            404,
            "The Ting type is not registered for this app yet; operators run deploy/ting-types.sh.",
        ),
        "idempotency_conflict" => (
            409,
            "This key was used for a different notification; report it with `spotify report`.",
        ),
        "payload_too_large" => (413, "Shorten the note."),
        "temporarily_rate_limited" => (429, "Retry later."),
        "permission_denied" | "test_context_mismatch" => {
            (403, "Check the app's Ting approval and testing plane.")
        }
        "invalid_proof"
        | "proof_expired"
        | "proof_consumed"
        | "proof_verification_uncertain"
        | "dependency_unavailable"
        | "storage_unavailable" => (
            503,
            "Temporary: retry with the same key (a fresh proof is minted per attempt).",
        ),
        _ if status >= 500 => (503, "Retry later."),
        _ => (502, "Report it with `spotify report`."),
    };
    AppError::new(
        axum::http::StatusCode::from_u16(http).unwrap_or(axum::http::StatusCode::BAD_GATEWAY),
        code,
        format!("Ting: {message}"),
        hint,
    )
    .with_details(json!({"ting_status": status}))
}

/// Registers the actor as a recipient for this app (`POST /v1/subscriptions`).
///
/// # Errors
/// IAM or Ting refusals, mapped.
pub async fn register(
    identity: &dyn Identity,
    ting: &dyn TingApi,
    context: &AuthContext,
    app_id: &str,
) -> AppResult<Value> {
    let body = serde_json::to_vec(
        &json!({"org_id": context.org_id, "app_id": app_id, "for": context.actor.public_id}),
    )
    .map_err(AppError::internal)?;
    let attempt = uuid::Uuid::now_v7().to_string();
    let proof = identity
        .ting_proof(context, TingEndpoint::Register, &body, &attempt)
        .await?;
    let (status, value) = ting
        .post(TingEndpoint::Register.path(), body, &proof)
        .await?;
    if !matches!(status, 200 | 201) {
        return Err(ting_error(status, &value));
    }
    if value.get("for").and_then(Value::as_str) != Some(context.actor.public_id.as_str())
        || value.get("active") != Some(&Value::Bool(true))
    {
        return Err(AppError::dependency("ting"));
    }
    Ok(
        json!({"id": value.get("id"), "app_id": value.get("app_id"), "for": value.get("for"), "active": true, "required_delivery": value.get("required_delivery")}),
    )
}

/// Builds the exact send bytes. `for` and `org_id` come from the verified session, never the caller.
///
/// # Errors
/// `invalid_input` for bad fields.
pub fn send_body(
    context: &AuthContext,
    event_type: &str,
    key: &str,
    data: &Value,
    metadata: &Value,
) -> AppResult<Vec<u8>> {
    if !silicon_spotify_client::TING_TYPES
        .iter()
        .any(|(t, _)| *t == event_type)
    {
        return Err(AppError::invalid(
            format!("`{event_type}` is not a spotify-cli Ting type."),
            format!(
                "Use one of: {}.",
                silicon_spotify_client::TING_TYPES
                    .iter()
                    .map(|(t, _)| *t)
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    let prefix = format!("{}/", context.actor.public_id);
    if key.is_empty()
        || key.len() > 200
        || !key.starts_with(&prefix)
        || key.chars().any(char::is_control)
    {
        return Err(AppError::invalid(
            format!("The Ting key must be 1-200 bytes and start with `{prefix}`."),
            "The daemon derives it as <recipient>/<trigger>/<play>/<outcome>.",
        ));
    }
    if !data.is_object() || !metadata.is_object() {
        return Err(AppError::invalid(
            "`data` and `metadata` must be JSON objects.",
            "Send both as objects, e.g. {\"data\": {\"trigger\": {…}}, \"metadata\": {}}; `metadata` may be left out.",
        ));
    }
    let body = serde_json::to_vec(&json!({
        "org_id": context.org_id,
        "type": event_type,
        "for": context.actor.public_id,
        "key": key,
        "data": data,
        "metadata": metadata,
    }))
    .map_err(AppError::internal)?;
    if body.len() > MAX_SEND_BYTES {
        return Err(AppError::new(
            axum::http::StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            "The notification exceeds Ting's 256 KiB limit.",
            "Shorten the trigger note.",
        ));
    }
    Ok(body)
}

/// Sends one Ting to the calling actor (`POST /v1/tings`) and returns Ting's acceptance.
///
/// # Errors
/// IAM or Ting refusals, mapped.
pub async fn send(
    identity: &dyn Identity,
    ting: &dyn TingApi,
    context: &AuthContext,
    body: Vec<u8>,
) -> AppResult<Value> {
    let attempt = uuid::Uuid::now_v7().to_string();
    let proof = identity
        .ting_proof(context, TingEndpoint::Send, &body, &attempt)
        .await?;
    let (status, value) = ting.post(TingEndpoint::Send.path(), body, &proof).await?;
    if !matches!(status, 200 | 202) {
        return Err(ting_error(status, &value));
    }
    if value.get("status").and_then(Value::as_str) != Some("accepted") {
        return Err(AppError::dependency("ting"));
    }
    Ok(json!({
        "id": value.get("id"),
        "created_at": value.get("created_at"),
        "key": value.get("key"),
        "silent": value.get("silent"),
        "replayed": status == 200,
    }))
}
