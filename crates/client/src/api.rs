//! HTTP client for the spotify-cli backend.
//!
//! The backend holds the Silicon Accounts application secret; this client never sees it. It exchanges a Silicon Accounts
//! short-lived token (SLT) for an application session, refreshes and revokes it, registers the
//! Silicon as a Ting recipient, and asks the backend to deliver trigger notifications through Ting.

use std::time::Duration;

use reqwest::{Method, StatusCode, header};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use url::Url;

use crate::{Error, Result};

/// An authenticated actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Actor {
    /// `silicon` or `carbon`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `si:<handle>` or `c:<handle>`.
    pub public_id: String,
    /// Stable Silicon Accounts identity.
    #[serde(default)]
    pub uuid: String,
}

/// Ting recipient registration result, separate from ordinary login.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TingRegistration {
    /// Whether Ting holds an active grant for this app to notify this actor.
    pub subscribed: bool,
    /// Ting subscription id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subscription_id: Option<String>,
    /// Why registration failed (login still succeeds; triggers need it).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<Error>,
}

/// An application session returned by login and refresh.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// `JWT`, about 30 minutes.
    pub access_token: String,
    /// `sar_…`, rotating; every refresh consumes it.
    pub refresh_token: String,
    /// Fixed refresh family expiry supplied by Silicon Accounts.
    #[serde(default)]
    pub refresh_token_expires_at: Option<String>,
    /// `Bearer`.
    pub token_type: String,
    /// Access token lifetime in seconds.
    pub expires_in: i64,
    /// Space-separated scopes.
    pub scope: String,
    /// Who logged in.
    pub actor: Actor,
    /// Ting registration (login only).
    #[serde(default)]
    pub ting: Option<TingRegistration>,
}

/// A trigger notification for the backend to send through Ting.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TingDelivery {
    /// Full type, e.g. `spotify.trigger.fired`.
    #[serde(rename = "type")]
    pub event_type: String,
    /// Ting idempotency key (stable across retries).
    pub key: String,
    /// Ting `data`.
    pub data: Value,
    /// Ting `metadata` (e.g. `{"isi": "planner"}`).
    pub metadata: Value,
}

/// Backend client. Cheap to clone.
#[derive(Clone, Debug)]
pub struct Api {
    http: reqwest::Client,
    base: Url,
}

/// Installs the `ring` rustls provider once when no provider is installed yet (pure Rust, so every
/// release target cross-compiles).
pub fn ensure_crypto() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

impl Api {
    /// Builds a client for `base_url` (HTTPS, or HTTP on loopback only).
    ///
    /// # Errors
    /// `invalid_input` for a bad URL.
    pub fn new(base_url: &str) -> Result<Self> {
        let base = validate_base(base_url)?;
        ensure_crypto();
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(8))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!(
                "silicon-spotify-client/",
                env!("CARGO_PKG_VERSION")
            ))
            .build()
            .map_err(|error| Error::internal(format!("HTTP client setup failed: {error}")))?;
        Ok(Self { http, base })
    }

    /// The backend origin.
    #[must_use]
    pub fn base_url(&self) -> &Url {
        &self.base
    }

    async fn call(
        &self,
        method: Method,
        path: &str,
        bearer: Option<&str>,
        idempotency_key: Option<&str>,
        body: Option<&Value>,
    ) -> Result<(StatusCode, Value)> {
        let url = self
            .base
            .join(path)
            .map_err(|error| Error::internal(format!("bad API path {path}: {error}")))?;
        let mut request = self
            .http
            .request(method, url)
            .header(header::ACCEPT, "application/json")
            // Older backend deployments also honor this fixed opt-out.
            .header("X-Spotify-Telemetry", "off");
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(key) = idempotency_key {
            request = request.header("Idempotency-Key", key);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().await.map_err(|error| {
            Error::backend_unavailable(format!(
                "Could not reach the spotify-cli backend at {}: {}.",
                self.base,
                transport_reason(&error)
            ))
        })?;
        let status = response.status();
        let request_id = response
            .headers()
            .get("x-request-id")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let bytes = response.bytes().await.map_err(|error| {
            Error::backend_unavailable(format!("The backend response was cut off: {error}."))
        })?;
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or_else(
                |_| json!({"raw": String::from_utf8_lossy(&bytes[..bytes.len().min(400)])}),
            )
        };
        if status.is_success() {
            return Ok((status, value));
        }
        Err(api_error(status, &value, request_id))
    }

    /// `GET /api/v1/accounts`: backend discovery (app id, Silicon Accounts URL).
    ///
    /// # Errors
    /// Transport or backend errors.
    pub async fn accounts(&self) -> Result<Value> {
        Ok(self
            .call(Method::GET, "/api/v1/accounts", None, None, None)
            .await?
            .1)
    }

    /// `GET /api/v1/version`: the backend's version, supported API majors and `min_cli`, the
    /// oldest CLI it still supports.
    ///
    /// # Errors
    /// Transport or backend errors.
    pub async fn version(&self) -> Result<Value> {
        Ok(self
            .call(Method::GET, "/api/v1/version", None, None, None)
            .await?
            .1)
    }

    /// `POST /api/v1/auth/login`: exchanges an SLT. Use a key derived from the SLT so an uncertain
    /// retry replays instead of consuming it twice.
    ///
    /// # Errors
    /// `not_authenticated` when Silicon Accounts rejects the SLT; transport errors.
    pub async fn login(&self, slt: &str, idempotency_key: &str) -> Result<Session> {
        let (_, value) = self
            .call(
                Method::POST,
                "/api/v1/auth/login",
                None,
                Some(idempotency_key),
                Some(&json!({"slt": slt})),
            )
            .await?;
        decode(value)
    }

    /// `POST /api/v1/auth/refresh`.
    ///
    /// # Errors
    /// `not_authenticated` when the refresh family is gone; transport errors (retry with the same key).
    pub async fn refresh(&self, refresh_token: &str, idempotency_key: &str) -> Result<Session> {
        let (_, value) = self
            .call(
                Method::POST,
                "/api/v1/auth/refresh",
                None,
                Some(idempotency_key),
                Some(&json!({"refresh_token": refresh_token})),
            )
            .await?;
        decode(value)
    }

    /// `POST /api/v1/auth/logout`: revokes the refresh family.
    ///
    /// # Errors
    /// Transport errors.
    pub async fn logout(&self, token: &str, idempotency_key: &str) -> Result<()> {
        self.call(
            Method::POST,
            "/api/v1/auth/logout",
            None,
            Some(idempotency_key),
            Some(&json!({"token": token})),
        )
        .await
        .map(drop)
    }

    /// `GET /api/v1/auth/me`: live identity check.
    ///
    /// # Errors
    /// `not_authenticated` on 401; transport errors.
    pub async fn me(&self, access_token: &str) -> Result<Value> {
        Ok(self
            .call(
                Method::GET,
                "/api/v1/auth/me",
                Some(access_token),
                None,
                None,
            )
            .await?
            .1)
    }

    /// `POST /api/v1/ting/subscription`: (re)registers this actor as a Ting recipient for the app.
    ///
    /// # Errors
    /// Backend or Ting errors.
    pub async fn register_ting(&self, access_token: &str, idempotency_key: &str) -> Result<Value> {
        Ok(self
            .call(
                Method::POST,
                "/api/v1/ting/subscription",
                Some(access_token),
                Some(idempotency_key),
                Some(&json!({})),
            )
            .await?
            .1)
    }

    /// `POST /api/v1/tings`: delivers one notification to the calling Silicon through Ting.
    ///
    /// # Errors
    /// Backend or Ting errors (codes pass through, e.g. `recipient_not_registered`).
    pub async fn send_ting(&self, access_token: &str, delivery: &TingDelivery) -> Result<Value> {
        let body = serde_json::to_value(delivery)?;
        Ok(self
            .call(
                Method::POST,
                "/api/v1/tings",
                Some(access_token),
                Some(&delivery.key),
                Some(&body),
            )
            .await?
            .1)
    }

    /// `POST /api/v1/reports`: files a bug report (optionally with a PR link).
    ///
    /// # Errors
    /// Backend errors.
    pub async fn report(
        &self,
        bearer: Option<&str>,
        report: &Value,
        idempotency_key: &str,
    ) -> Result<Value> {
        Ok(self
            .call(
                Method::POST,
                "/api/v1/reports",
                bearer,
                Some(idempotency_key),
                Some(report),
            )
            .await?
            .1)
    }
}

fn decode<T: for<'de> Deserialize<'de>>(value: Value) -> Result<T> {
    serde_json::from_value(value).map_err(|error| {
        Error::new(
            "backend_unexpected_response",
            format!("The backend answered in an unexpected shape: {error}."),
            "Your CLI and the backend may be on incompatible versions. Update with `spotify update` and retry.",
        )
    })
}

fn transport_reason(error: &reqwest::Error) -> String {
    if error.is_timeout() {
        "the request timed out".into()
    } else if error.is_connect() {
        "the connection failed (offline, DNS, or the server is down)".into()
    } else {
        error.to_string()
    }
}

fn api_error(status: StatusCode, value: &Value, request_id: Option<String>) -> Error {
    let body = value.get("error");
    let code = body
        .and_then(|e| e.get("code"))
        .and_then(Value::as_str)
        .map(str::to_owned);
    let message = body
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| format!("The backend answered HTTP {status}."));
    let hint = body
        .and_then(|e| e.get("hint"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let retryable = body
        .and_then(|e| e.get("retryable"))
        .and_then(Value::as_bool)
        .unwrap_or(matches!(status.as_u16(), 429 | 502 | 503 | 504));
    let request_id = request_id.or_else(|| {
        body.and_then(|e| e.get("request_id"))
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let code = match (status.as_u16(), code) {
        (401, _) => "not_authenticated".to_owned(),
        (_, Some(code)) => code,
        (403, None) => "forbidden".to_owned(),
        (404, None) => "not_found".to_owned(),
        (429, None) => "rate_limited".to_owned(),
        (500..=599, None) => "backend_unavailable".to_owned(),
        _ => "backend_error".to_owned(),
    };
    let mut error = Error::new(code, message, hint);
    if error.code == "not_authenticated" && error.hint.is_empty() {
        error.hint = Error::not_authenticated("").hint;
    }
    error.retryable = retryable;
    error.with_details(json!({"status": status.as_u16(), "request_id": request_id}))
}

/// Validates a backend origin: HTTPS, or HTTP on loopback; no credentials, query or fragment.
///
/// # Errors
/// `invalid_input` explaining the rule.
pub fn validate_base(value: &str) -> Result<Url> {
    let url = Url::parse(value.trim()).map_err(|_| {
        Error::invalid(
            format!("`{value}` is not a URL."),
            "Use the backend origin, e.g. https://backend.spotify.unlikefraction.com.",
        )
    })?;
    let loopback = matches!(url.host_str(), Some("localhost" | "127.0.0.1" | "[::1]"))
        || url.host_str().is_some_and(|h| h.ends_with(".localhost"));
    let ok_scheme = url.scheme() == "https" || (url.scheme() == "http" && loopback);
    if !ok_scheme
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(Error::invalid(
            format!("`{value}` is not an allowed backend origin."),
            "The API URL must be https:// (http:// only for localhost), without credentials, query or fragment.",
        ));
    }
    Ok(url)
}
