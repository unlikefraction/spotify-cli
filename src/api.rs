//! HTTP API (`/api/v1/*`, health, Silicon Accounts webhook).

use std::sync::Arc;
use std::time::Duration;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, HeaderValue, Method, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use secrecy::{ExposeSecret as _, SecretString};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::config::Settings;
use crate::error::{AppError, AppResult};
use crate::identity::{self, Accounts, AuthContext, Identity};
use crate::store::Store;
use crate::ting::{self, TingApi};

/// Shared state.
pub struct AppState {
    /// Settings.
    pub settings: Settings,
    /// Production identity adapter.
    pub identity: Arc<dyn Identity>,
    /// Ting transport.
    pub ting: Arc<dyn TingApi>,
    /// SQLite.
    pub store: Arc<Store>,
    /// Outbound HTTP (GitHub issues).
    pub http: reqwest::Client,
}

type Shared = State<Arc<AppState>>;

/// The router.
pub fn router(state: Arc<AppState>) -> Router {
    let routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/accounts", get(accounts))
        .route("/api/v1/version", get(version))
        .route("/api/v1/auth/code", post(code).options(preflight))
        .route("/api/v1/auth/login", post(login).options(preflight))
        .route("/api/v1/auth/refresh", post(refresh).options(preflight))
        .route("/api/v1/auth/logout", post(logout).options(preflight))
        .route("/api/v1/auth/me", get(me).options(preflight))
        .route("/api/v1/ting/subscription", post(subscribe))
        .route("/api/v1/tings", post(send_ting))
        .route("/api/v1/reports", post(report))
        .fallback(not_found)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        // Outside the body limit, so its 413 gets a request id too.
        .layer(middleware::from_fn_with_state(Arc::clone(&state), response_headers))
        .with_state(state);
    // axum finishes a 405 (its `Allow` header) only after the per-route layers, so framework
    // errors are rewritten as JSON around the whole router.
    Router::new()
        .fallback_service(routes)
        .layer(middleware::map_response(json_error))
}

/// Largest request body any route reads.
const MAX_BODY_BYTES: usize = 512 * 1024;

async fn response_headers(State(state): Shared, request: Request, next: Next) -> Response {
    let request_id = format!("req_{}", uuid::Uuid::now_v7().simple());
    let request_headers = request.headers().clone();
    let mut response = next.run(request).await;
    cors(&state, &request_headers, &mut response);
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    response
}

/// Rewrites an error the framework produced as plain text or an empty body (axum's 405, the body
/// limit's 413, an unreadable body) into the documented JSON envelope, keeping its status and its
/// other headers (such as `Allow`).
async fn json_error(method: Method, response: Response) -> Response {
    let status = response.status();
    let json = response
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.starts_with("application/json"));
    if json || !(status.is_client_error() || status.is_server_error()) {
        return response;
    }
    let (mut parts, _) = response.into_parts();
    let error = match status {
        StatusCode::METHOD_NOT_ALLOWED => {
            let allow = parts
                .headers
                .get(header::ALLOW)
                .and_then(|v| v.to_str().ok())
                .filter(|v| !v.is_empty())
                .unwrap_or("another method");
            AppError::new(
                status,
                "method_not_allowed",
                format!("This route does not accept {method}."),
                format!("Use {allow}; see `spotify docs api` for each route's method."),
            )
        }
        StatusCode::PAYLOAD_TOO_LARGE => AppError::payload_too_large(
            format!(
                "The request body exceeds the backend's {} KiB limit.",
                MAX_BODY_BYTES / 1024
            ),
            "Send less: shorten the report or its attachments.",
        ),
        StatusCode::NOT_FOUND => not_found_error(),
        _ if status.is_client_error() => AppError::invalid(
            format!("The request could not be read (HTTP {}).", status.as_u16()),
            "Send a JSON body with `Content-Type: application/json`; see `spotify docs api` for the request shape.",
        )
        .with_details(json!({"status": status.as_u16()})),
        _ => AppError::new(
            status,
            "internal",
            "The server failed unexpectedly.",
            "Retry; if it persists, report it with `spotify report`.",
        ),
    };
    let (rewritten, body) = error.into_response().into_parts();
    parts.headers.remove(header::CONTENT_LENGTH);
    for (name, value) in &rewritten.headers {
        parts.headers.insert(name.clone(), value.clone());
    }
    Response::from_parts(parts, body)
}

async fn not_found() -> AppError {
    not_found_error()
}

fn not_found_error() -> AppError {
    AppError::new(
        StatusCode::NOT_FOUND,
        "not_found",
        "No such route.",
        "See `spotify docs api` for the backend API.",
    )
}

async fn healthz() -> Json<Value> {
    Json(json!({"status": "ok", "service": "spotify-cli", "version": env!("CARGO_PKG_VERSION")}))
}

async fn readyz(State(state): Shared) -> Response {
    if state.store.healthy() {
        Json(json!({"status": "ready"})).into_response()
    } else {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "reason": "database"})),
        )
            .into_response()
    }
}

async fn version() -> Json<Value> {
    Json(json!({"version": env!("CARGO_PKG_VERSION"), "api_versions": ["v1"], "min_cli": "0.2.0"}))
}

fn no_store(value: Value) -> Response {
    let mut response = Json(value).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
    response
}

async fn accounts(State(state): Shared) -> Response {
    no_store(identity::discovery(&state.settings))
}

fn idempotency_key(headers: &HeaderMap) -> AppResult<String> {
    let value = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .ok_or_else(|| {
            AppError::invalid(
                "Idempotency-Key header is required.",
                "Send 16-255 visible ASCII characters; reuse it only to retry the same request.",
            )
        })?;
    if !(16..=255).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(AppError::invalid(
            "Idempotency-Key must be 16-255 visible ASCII characters.",
            "Send a fresh random key per request (for example a prefixed UUID) and reuse it only to retry that same request.",
        ));
    }
    Ok(value.to_owned())
}

fn parse<T: for<'de> Deserialize<'de>>(body: &Bytes) -> AppResult<T> {
    serde_json::from_slice(body).map_err(|error| {
        AppError::invalid(
            format!("Invalid JSON body: {error}."),
            "See `spotify docs api` for the request shape.",
        )
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LoginInput {
    slt: SecretString,
}

async fn login(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let key = idempotency_key(&headers)?;
    let input: LoginInput = parse(&body)?;
    let identity = Arc::clone(&state.identity);
    // Keep token rotation and its durable replay result alive after a client disconnects.
    let session = tokio::spawn(async move { identity.login(&input.slt, &key).await })
        .await
        .map_err(AppError::internal)??;
    let mut value = serde_json::to_value(&session).map_err(AppError::internal)?;
    value["ting"] = json!({"subscribed": false});
    Ok(no_store(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CodeInput {
    code: SecretString,
    redirect_uri: String,
    code_verifier: SecretString,
}
async fn code(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    idempotency_key(&headers)?;
    let input: CodeInput = parse(&body)?;
    if !state
        .settings
        .web_origins
        .iter()
        .any(|origin| input.redirect_uri == format!("{}/", origin.trim_end_matches('/')))
        || !(43..=128).contains(&input.code_verifier.expose_secret().len())
        || !input
            .code_verifier
            .expose_secret()
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
    {
        return Err(AppError::invalid(
            "Invalid sign-in redirect or PKCE verifier.",
            "Start sign-in again from the Spotify website.",
        ));
    }
    let identity = Arc::clone(&state.identity);
    let session = tokio::spawn(async move {
        identity
            .code(&input.code, &input.redirect_uri, &input.code_verifier)
            .await
    })
    .await
    .map_err(AppError::internal)??;
    Ok(no_store(
        serde_json::to_value(session).map_err(AppError::internal)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshInput {
    refresh_token: SecretString,
}

async fn refresh(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let key = idempotency_key(&headers)?;
    let input: RefreshInput = parse(&body)?;
    let identity = Arc::clone(&state.identity);
    let session = tokio::spawn(async move { identity.refresh(&input.refresh_token, &key).await })
        .await
        .map_err(AppError::internal)??;
    Ok(no_store(
        serde_json::to_value(&session).map_err(AppError::internal)?,
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LogoutInput {
    token: SecretString,
}

async fn logout(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let key = idempotency_key(&headers)?;
    let input: LogoutInput = parse(&body)?;
    let identity = Arc::clone(&state.identity);
    tokio::spawn(async move { identity.logout(&input.token, &key).await })
        .await
        .map_err(AppError::internal)??;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn bearer(
    state: &AppState,
    headers: &HeaderMap,
) -> AppResult<(Arc<dyn Identity>, AuthContext)> {
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| identity::well_formed(t))
        .ok_or_else(AppError::unauthenticated)?;
    let identity = Arc::clone(&state.identity);
    let context = identity
        .authenticate(&SecretString::from(token.to_owned()))
        .await?;
    Ok((identity, context))
}

async fn me(State(state): Shared, headers: HeaderMap) -> AppResult<Response> {
    let (_, context) = bearer(&state, &headers).await?;
    Ok(no_store(json!({
        "authenticated": true,
        "actor": context.actor,
        "membership_id": context.membership_id,
        "scopes": context.scopes,
    })))
}

async fn subscribe(State(state): Shared, headers: HeaderMap) -> AppResult<Response> {
    let key = idempotency_key(&headers)?;
    let (identity, context) = bearer(&state, &headers).await?;
    let subscription = ting::register(
        identity.as_ref(),
        state.ting.as_ref(),
        &context,
        &state.settings.app_id,
        &key,
    )
    .await?;
    Ok(no_store(subscription))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TingInput {
    #[serde(rename = "type")]
    event_type: String,
    key: String,
    data: Value,
    #[serde(default = "empty_object")]
    metadata: Value,
}

fn empty_object() -> Value {
    json!({})
}

async fn send_ting(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let input: TingInput = parse(&body)?;
    let (identity, context) = bearer(&state, &headers).await?;
    let bytes = ting::send_body(
        &context,
        &input.event_type,
        &input.key,
        &input.data,
        &input.metadata,
    )?;
    let result = ting::send(identity.as_ref(), state.ting.as_ref(), &context, bytes).await;
    Ok(no_store(result?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReportInput {
    message: String,
    #[serde(default)]
    pr: Option<String>,
    #[serde(default)]
    attachments: Vec<Value>,
    #[serde(default)]
    context: Value,
}

async fn report(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let key = idempotency_key(&headers)?;
    let input: ReportInput = parse(&body)?;
    let message = input.message.trim();
    if message.len() < 10 || message.len() > 20_000 {
        return Err(AppError::invalid(
            "A report must be 10-20000 characters.",
            "Describe what you ran, what happened and how to reproduce it.",
        ));
    }
    if let Some(pr) = &input.pr {
        let number = pr
            .strip_prefix("https://github.com/unlikefraction/spotify-cli/pull/")
            .unwrap_or("");
        if number.is_empty() || !number.bytes().all(|b| b.is_ascii_digit()) {
            return Err(AppError::invalid(
                "`pr` must be https://github.com/unlikefraction/spotify-cli/pull/<n>.",
                "Link a pull request on unlikefraction/spotify-cli, or leave `pr` out.",
            ));
        }
    }
    if input.attachments.len() > 5 {
        return Err(AppError::invalid(
            "At most 5 attachments.",
            "Combine logs into at most 5 attachments (each is cut to 20000 characters).",
        ));
    }
    // Reporter identity is optional; a bad token does not block a bug report.
    let reporter = if headers.contains_key(header::AUTHORIZATION) {
        bearer(&state, &headers).await.ok().map(|(_, c)| c)
    } else {
        None
    };
    let id = format!("rep_{}", uuid::Uuid::now_v7().simple());
    let stored = json!({"message": message, "pr": input.pr, "attachments": input.attachments, "context": input.context});
    let (id, mut status, mut issue_url) = state.store.save_report(
        &id,
        &key,
        reporter.as_ref().map(|c| c.actor.public_id.as_str()),
        message,
        input.pr.as_deref(),
        &stored,
    )?;
    if status == "stored"
        && let Some(token) = &state.settings.github_token
    {
        match file_issue(
            &state,
            token,
            &id,
            message,
            input.pr.as_deref(),
            &input.context,
            &input.attachments,
            reporter.as_ref(),
        )
        .await
        {
            Ok(url) => {
                state.store.mark_filed(&id, &url)?;
                status = "filed".into();
                issue_url = Some(url);
            }
            Err(error) => {
                tracing::warn!(%error, "could not file the report on GitHub; it stays stored")
            }
        }
    }
    Ok(no_store(
        json!({"id": id, "status": status, "issue_url": issue_url}),
    ))
}

#[allow(clippy::too_many_arguments)]
async fn file_issue(
    state: &AppState,
    token: &SecretString,
    id: &str,
    message: &str,
    pr: Option<&str>,
    context: &Value,
    attachments: &[Value],
    reporter: Option<&AuthContext>,
) -> anyhow::Result<String> {
    let title = format!(
        "[report] {}",
        silicon_spotify_client::model::truncate(message.lines().next().unwrap_or(message), 90)
    );
    let mut body = format!("{message}\n\n");
    if let Some(pr) = pr {
        body.push_str(&format!("Proposed fix: {pr}\n\n"));
    }
    body.push_str(&format!(
        "Report `{id}` · reporter: {} · context:\n```json\n{}\n```\n",
        reporter.map_or("anonymous", |c| c.actor.public_id.as_str()),
        serde_json::to_string_pretty(context)?
    ));
    for attachment in attachments {
        let name = attachment
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("attachment");
        let content = attachment
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("");
        body.push_str(&format!(
            "\n<details><summary>{name}</summary>\n\n```\n{}\n```\n</details>\n",
            silicon_spotify_client::model::truncate(content, 20_000)
        ));
    }
    let response = state
        .http
        .post(format!(
            "https://api.github.com/repos/{}/issues",
            state.settings.github_repository
        ))
        .bearer_auth(token.expose_secret())
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "silicon-spotify-backend")
        .json(&json!({"title": title, "body": body, "labels": ["bug-report"]}))
        .send()
        .await?;
    anyhow::ensure!(
        response.status().is_success(),
        "GitHub answered {}",
        response.status()
    );
    let value: Value = response.json().await?;
    value
        .get("html_url")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow::anyhow!("no html_url"))
}

fn cors(state: &AppState, headers: &HeaderMap, response: &mut Response) {
    if let Some(origin) = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok())
        && state.settings.web_origins.iter().any(|o| o == origin)
        && let Ok(value) = HeaderValue::from_str(origin)
    {
        let h = response.headers_mut();
        h.insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, value);
        h.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, OPTIONS"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("content-type, authorization, idempotency-key"),
        );
        h.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
}

async fn preflight(State(state): Shared, headers: HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    cors(&state, &headers, &mut response);
    response
}

/// Builds the production state.
///
/// # Errors
/// Configuration problems.
pub fn production(settings: Settings) -> anyhow::Result<Arc<AppState>> {
    let identity: Arc<dyn Identity> = Arc::new(Accounts::new(&settings)?);
    let ting: Arc<dyn TingApi> = Arc::new(ting::TingHttp::new(
        settings.ting_url.clone(),
        settings.ting_timeout,
    )?);
    let store = Arc::new(Store::open(&settings.database_path)?);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(Arc::new(AppState {
        settings,
        identity,
        ting,
        store,
        http,
    }))
}
