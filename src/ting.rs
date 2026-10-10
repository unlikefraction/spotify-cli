//! Ting calls: recipient registration and sends, with exact-bytes bodies.

use std::time::Duration;

use async_trait::async_trait;
use secrecy::ExposeSecret as _;
use serde_json::{Value, json};
use url::Url;

use crate::error::{AppError, AppResult};
use crate::identity::{AuthContext, Identity, TingAccess, TingEndpoint};

/// Largest Ting send body.
pub const MAX_SEND_BYTES: usize = 256 * 1024;

/// HTTP transport to Ting.
#[async_trait]
pub trait TingApi: Send + Sync {
    /// POSTs exact bytes with a Silicon Accounts verification proof; returns (status, JSON body).
    async fn post(&self, path: &str, body: Vec<u8>, proof: &TingAccess) -> AppResult<(u16, Value)>;
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
    async fn post(&self, path: &str, body: Vec<u8>, proof: &TingAccess) -> AppResult<(u16, Value)> {
        let url = self.base.join(path).map_err(AppError::internal)?;
        let request = self
            .client
            .post(url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header("Ting-Client-Version", env!("CARGO_PKG_VERSION"))
            .header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", proof.token.expose_secret()),
            )
            .body(body);
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
    if body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .is_some_and(|message| message.contains("org_id"))
    {
        return AppError::new(
            axum::http::StatusCode::SERVICE_UNAVAILABLE,
            "dependency_unavailable",
            "Ting has not migrated to Silicon Accounts yet.",
            "Your login and pending work are preserved. Retry after Ting supports Carbon and Silicon user verification.",
        );
    }
    let code = body
        .pointer("/error/code")
        .and_then(Value::as_str)
        .unwrap_or("ting_error");
    if matches!(
        code,
        "invalid_obo_token" | "invalid_proof" | "obo_grant_revoked" | "obo_grant_expired"
    ) {
        return crate::auth_cache::required().with_details(json!({"ting_code": code}));
    }
    let message = body
        .pointer("/error/message")
        .and_then(Value::as_str)
        .unwrap_or("Ting refused the request.");
    let (http, hint) = match code {
        "recipient_not_registered" => (403, "Register again with `spotify ting register`."),
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
        "permission_denied" | "test_context_mismatch" => (403, "Check the app's Ting approval."),
        "proof_verification_uncertain" | "dependency_unavailable" | "storage_unavailable" => {
            (503, "Temporary: retry the same pending action and key.")
        }
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
/// Silicon Accounts or Ting refusals, mapped.
pub async fn register(
    identity: &dyn Identity,
    ting: &dyn TingApi,
    context: &AuthContext,
    app_id: &str,
    attempt: &str,
) -> AppResult<Value> {
    let body = serde_json::to_vec(&json!({"app_id": app_id, "for": context.actor.public_id}))
        .map_err(AppError::internal)?;
    let proof = identity
        .ting_access(context, TingEndpoint::Register, &body, attempt)
        .await?;
    let body = provider_body(&body, &proof)?;
    let (status, value) = ting
        .post(TingEndpoint::Register.path(), body, &proof)
        .await?;
    if !matches!(status, 200 | 201) {
        return Err(ting_error(status, &value));
    }
    if value.get("for").and_then(Value::as_str) != Some(proof.actor.uuid.as_str())
        || value.get("app_id").and_then(Value::as_str) != Some(app_id)
        || value.get("active") != Some(&Value::Bool(true))
    {
        return Err(AppError::dependency("ting"));
    }
    Ok(
        json!({"id": value.get("id"), "app_id": value.get("app_id"), "for": value.get("for"), "active": true, "required_delivery": value.get("required_delivery")}),
    )
}

/// Builds the exact send bytes. `for` comes from the verified session, never the caller.
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
/// Silicon Accounts or Ting refusals, mapped.
pub async fn send(
    identity: &dyn Identity,
    ting: &dyn TingApi,
    context: &AuthContext,
    body: Vec<u8>,
) -> AppResult<Value> {
    let value: Value = serde_json::from_slice(&body).map_err(AppError::internal)?;
    let attempt = value["key"]
        .as_str()
        .ok_or_else(|| AppError::internal("missing operation key"))?;
    let proof = identity
        .ting_access(context, TingEndpoint::Send, &body, attempt)
        .await?;
    let body = provider_body(&body, &proof)?;
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

fn provider_body(body: &[u8], proof: &TingAccess) -> AppResult<Vec<u8>> {
    let mut value: Value = serde_json::from_slice(body).map_err(AppError::internal)?;
    value["for"] = json!(proof.actor.uuid);
    serde_json::to_vec(&value).map_err(AppError::internal)
}
