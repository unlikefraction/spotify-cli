//! HTTP errors: `{"error":{"code","message","hint","retryable","request_id"}}`.

use axum::Json;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::json;

/// Result alias.
pub type AppResult<T> = Result<T, AppError>;

/// The hint an error without a specific one is sent with.
const GENERIC_HINT: &str = "See `spotify docs api` for the request shape; if it persists, report it with `spotify report`.";

/// A failure with a stable code, a status and an agent-readable hint.
#[derive(Clone, Debug)]
pub struct AppError {
    /// HTTP status.
    pub status: StatusCode,
    /// Stable snake_case code.
    pub code: String,
    /// What happened.
    pub message: String,
    /// What to do next.
    pub hint: String,
    /// Extra fields.
    pub details: Option<serde_json::Value>,
}

impl AppError {
    /// Any error.
    pub fn new(
        status: StatusCode,
        code: &str,
        message: impl Into<String>,
        hint: impl Into<String>,
    ) -> Self {
        Self {
            status,
            code: code.to_owned(),
            message: message.into(),
            hint: hint.into(),
            details: None,
        }
    }

    /// 400 `invalid_input`.
    pub fn invalid(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(StatusCode::BAD_REQUEST, "invalid_input", message, hint)
    }

    /// 401 `unauthenticated`: the session is missing, expired or revoked.
    pub fn unauthenticated() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "unauthenticated",
            "The access token is missing, expired or revoked.",
            "Refresh the session (`spotify login status` does it), or log in again with `spotify login '<SLT>'`.",
        )
    }

    /// 401 `slt_rejected`: IAM refused the short-lived login token itself.
    ///
    /// 401 like `unauthenticated`, so clients that only branch on the status still treat it as
    /// "not signed in"; the code and hint name the SLT instead of a session refresh.
    pub fn slt_rejected() -> Self {
        Self::new(
            StatusCode::UNAUTHORIZED,
            "slt_rejected",
            "IAM rejected the short-lived login token (SLT): it expired (SLTs last about 2 minutes), was already used, or was minted for another app.",
            "Mint a fresh SLT for app `spotify` and log in right away: iam silicon-login --app-id spotify --grant-org <org> --approve-scopes, then spotify login '<SLT>'.",
        )
    }

    /// 403 `forbidden`.
    pub fn forbidden(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(StatusCode::FORBIDDEN, "forbidden", message, hint)
    }

    /// 503 for an upstream (IAM, Ting) problem; never a user logout.
    pub fn dependency(name: &str) -> Self {
        Self::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "dependency_unavailable",
            format!("{name} could not be reached or answered unexpectedly."),
            "Retry shortly with the same idempotency key.",
        )
        .with_details(json!({"dependency": name}))
    }

    /// 429.
    pub fn rate_limited() -> Self {
        Self::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limited",
            "Too many requests.",
            "Wait a moment and retry.",
        )
    }

    /// 413 `payload_too_large`.
    pub fn payload_too_large(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(
            StatusCode::PAYLOAD_TOO_LARGE,
            "payload_too_large",
            message,
            hint,
        )
    }

    /// 409.
    pub fn conflict(message: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(StatusCode::CONFLICT, "conflict", message, hint)
    }

    /// 500 (internal; logs the cause).
    pub fn internal(cause: impl std::fmt::Display) -> Self {
        tracing::error!(%cause, "internal error");
        Self::new(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal",
            "The server failed unexpectedly.",
            "Retry; if it persists, report it with `spotify report`.",
        )
    }

    /// Adds details.
    #[must_use]
    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = Some(details);
        self
    }

    fn retryable(&self) -> bool {
        matches!(self.status.as_u16(), 429 | 502 | 503 | 504)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let retryable = self.retryable();
        // Every documented error carries a next step; a call site that forgot one still gets this.
        let hint = if self.hint.trim().is_empty() {
            GENERIC_HINT.to_owned()
        } else {
            self.hint
        };
        let mut body = json!({"error": {
            "code": self.code,
            "message": self.message,
            "hint": hint,
            "retryable": retryable,
        }});
        if let Some(details) = &self.details {
            body["error"]["details"] = details.clone();
        }
        let mut response = (self.status, Json(body)).into_response();
        response.headers_mut().insert(
            axum::http::header::CACHE_CONTROL,
            axum::http::HeaderValue::from_static("no-store"),
        );
        response
    }
}
