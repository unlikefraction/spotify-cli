//! HTTP API (`/api/v1/*`, health, IAM webhook).

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
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
use crate::identity::{self, AuthContext, Iam, Identity};
use crate::store::Store;
use crate::telemetry::Recorder;
use crate::ting::{self, TingApi};

/// Resolves the identity adapter for a testing plane.
#[async_trait]
pub trait TestingPlanes: Send + Sync {
    /// The adapter for this testing app secret.
    async fn resolve(&self, secret: &str) -> AppResult<Arc<dyn Identity>>;
}

/// A resolved testing-plane adapter and when it was resolved.
type CachedPlane = (Arc<dyn Identity>, Instant);

/// IAM-backed resolver with a 10-minute cache.
pub struct IamTestingPlanes {
    settings: Settings,
    cache: Mutex<HashMap<String, CachedPlane>>,
}

impl IamTestingPlanes {
    /// New resolver.
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            settings,
            cache: Mutex::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl TestingPlanes for IamTestingPlanes {
    async fn resolve(&self, secret: &str) -> AppResult<Arc<dyn Identity>> {
        let key = blake3::hash(secret.as_bytes()).to_hex().to_string();
        if let Some((identity, at)) = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&key)
            && at.elapsed() < Duration::from_secs(600)
        {
            return Ok(Arc::clone(identity));
        }
        let identity: Arc<dyn Identity> = Arc::new(Iam::discover(&self.settings, secret).await?);
        let mut cache = self
            .cache
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if cache.len() > 256 {
            cache.clear();
        }
        cache.insert(key, (Arc::clone(&identity), Instant::now()));
        Ok(identity)
    }
}

/// Shared state.
pub struct AppState {
    /// Settings.
    pub settings: Settings,
    /// Production identity adapter.
    pub identity: Arc<dyn Identity>,
    /// Testing plane resolver.
    pub testing: Arc<dyn TestingPlanes>,
    /// Ting transport.
    pub ting: Arc<dyn TingApi>,
    /// SQLite.
    pub store: Arc<Store>,
    /// Space Station.
    pub telemetry: Recorder,
    /// Outbound HTTP (GitHub issues).
    pub http: reqwest::Client,
}

type Shared = State<Arc<AppState>>;

/// The router.
pub fn router(state: Arc<AppState>) -> Router {
    let routes = Router::new()
        .route("/healthz", get(healthz))
        .route("/readyz", get(readyz))
        .route("/api/v1/iam", get(iam))
        .route("/api/v1/version", get(version))
        .route("/api/v1/auth/login", post(login))
        .route("/api/v1/auth/refresh", post(refresh))
        .route("/api/v1/auth/logout", post(logout))
        .route("/api/v1/auth/me", get(me))
        .route("/api/v1/ting/subscription", post(subscribe))
        .route("/api/v1/tings", post(send_ting))
        .route("/api/v1/reports", post(report))
        .route("/api/v1/telemetry", post(telemetry).options(preflight))
        .route("/webhook", post(webhook))
        .route("/webhook/", post(webhook))
        .fallback(not_found)
        .layer(tower_http::limit::RequestBodyLimitLayer::new(MAX_BODY_BYTES))
        // Outside the body limit, so its 413 gets a request id too.
        .layer(middleware::from_fn_with_state(Arc::clone(&state), observe))
        .with_state(state);
    // axum finishes a 405 (its `Allow` header) only after the per-route layers, so framework
    // errors are rewritten as JSON around the whole router.
    Router::new()
        .fallback_service(routes)
        .layer(middleware::map_response(json_error))
}

/// Largest request body any route reads.
const MAX_BODY_BYTES: usize = 512 * 1024;

/// Largest telemetry batch the gateway relays.
const MAX_TELEMETRY_BYTES: usize = 64 * 1024;

async fn observe(State(state): Shared, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().clone();
    let route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map(|p| p.as_str().to_owned());
    let telemetry_off = request
        .headers()
        .get("x-spotify-telemetry")
        .and_then(|v| v.to_str().ok())
        == Some("off");
    let source = request
        .headers()
        .get("x-spotify-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .chars()
        .take(16)
        .collect::<String>();
    let request_id = format!("req_{}", uuid::Uuid::now_v7().simple());
    let mut response = next.run(request).await;
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response.headers_mut().insert("x-request-id", value);
    }
    let route = route.unwrap_or_else(|| "unmatched".into());
    if !telemetry_off && route != "/healthz" && route != "/readyz" && route != "/api/v1/telemetry" {
        let status = response.status().as_u16();
        state.telemetry.backend(
            "http.completed",
            &route,
            if status < 400 { "ok" } else { "error" },
            None,
            json!({"method": method.as_str(), "route": route, "status": status, "duration_ms": started.elapsed().as_millis(), "source": source, "request_id": request_id}),
        );
    }
    tracing::info!(method = %method, route, status = response.status().as_u16(), ms = started.elapsed().as_millis(), "request");
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
            "Send less: shorten the report or its attachments, or split telemetry into batches of at most 64 KiB.",
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
    Json(json!({"version": env!("CARGO_PKG_VERSION"), "api_versions": ["v1"], "min_cli": "0.1.0"}))
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

async fn identity_for(state: &AppState, headers: &HeaderMap) -> AppResult<Arc<dyn Identity>> {
    match headers.get("x-testing-environment-key") {
        None => Ok(Arc::clone(&state.identity)),
        Some(value) => {
            let secret = value
                .to_str()
                .map_err(|_| {
                    AppError::invalid(
                        "X-Testing-Environment-Key is not text.",
                        "Send the testing plane's ask_… app secret as plain ASCII; `spotify testing use --app-secret-file -` sets it for the CLI.",
                    )
                })?;
            state.testing.resolve(secret.trim()).await
        }
    }
}

async fn iam(State(state): Shared, headers: HeaderMap) -> AppResult<Response> {
    let identity = identity_for(&state, &headers).await?;
    Ok(no_store(identity::discovery(
        &state.settings,
        identity.environment_id(),
    )))
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
    let identity = identity_for(&state, &headers).await?;
    let session = identity.login(&input.slt, &key).await?;
    // Register the Silicon as a Ting recipient right away (explicit login only: re-registering
    // re-activates a grant the recipient may have revoked). Failure does not fail login.
    let context = identity
        .authenticate(
            &SecretString::from(session.access_token.clone()),
            &session.org_id,
        )
        .await?;
    let registration = match ting::register(
        identity.as_ref(),
        state.ting.as_ref(),
        &context,
        &state.settings.app_id,
    )
    .await
    {
        Ok(subscription) => json!({"subscribed": true, "subscription_id": subscription["id"]}),
        Err(error) => {
            state.telemetry.backend(
                "ting.register.failed",
                "login",
                "error",
                Some(&error.code),
                json!({"status": error.status.as_u16()}),
            );
            json!({"subscribed": false, "error": {"code": error.code, "message": error.message, "hint": error.hint, "retryable": matches!(error.status.as_u16(), 429 | 503)}})
        }
    };
    let mut value = serde_json::to_value(&session).map_err(AppError::internal)?;
    value["ting"] = registration;
    state.telemetry.backend("auth.login.completed", "login", "ok", None, json!({"actor_type": session.actor.kind, "testing": session.testing_environment_id.is_some()}));
    Ok(no_store(value))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RefreshInput {
    refresh_token: SecretString,
}

async fn refresh(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let key = idempotency_key(&headers)?;
    let input: RefreshInput = parse(&body)?;
    let identity = identity_for(&state, &headers).await?;
    let session = identity.refresh(&input.refresh_token, &key).await?;
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
    let identity = identity_for(&state, &headers).await?;
    identity.logout(&input.token, &key).await?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn bearer(
    state: &AppState,
    headers: &HeaderMap,
) -> AppResult<(Arc<dyn Identity>, AuthContext)> {
    // A bearer that cannot be an access token is refused before the organization is checked.
    let token = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|t| identity::well_formed(t, "oat_"))
        .ok_or_else(AppError::unauthenticated)?;
    let org = headers
        .get("x-org-id")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|o| silicon_spotify_client::valid_handle(o, 3, 50))
        .ok_or_else(|| {
            AppError::invalid(
                "X-Org-ID header with an organization handle is required.",
                "The CLI sends the session's organization; pass --org to choose another.",
            )
        })?;
    let identity = identity_for(state, headers).await?;
    let context = identity
        .authenticate(&SecretString::from(token.to_owned()), org)
        .await?;
    Ok((identity, context))
}

async fn me(State(state): Shared, headers: HeaderMap) -> AppResult<Response> {
    let (identity, context) = bearer(&state, &headers).await?;
    Ok(no_store(json!({
        "authenticated": true,
        "actor": context.actor,
        "org_id": context.org_id,
        "membership_id": context.membership_id,
        "session_id": context.session_id,
        "scopes": context.scopes,
        "ting_ready": context.has("obo:ting:tings.send") && context.has("self.identity.read"),
        "testing_environment_id": identity.environment_id(),
    })))
}

async fn subscribe(State(state): Shared, headers: HeaderMap) -> AppResult<Response> {
    let (identity, context) = bearer(&state, &headers).await?;
    let subscription = ting::register(
        identity.as_ref(),
        state.ting.as_ref(),
        &context,
        &state.settings.app_id,
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
    state.telemetry.backend(
        "ting.send.completed",
        &input.event_type,
        if result.is_ok() { "ok" } else { "error" },
        result.as_ref().err().map(|e| e.code.as_str()),
        json!({"type": input.event_type, "testing": identity.environment_id().is_some()}),
    );
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
    let testing = headers.contains_key("x-testing-environment-key");
    let id = format!("rep_{}", uuid::Uuid::now_v7().simple());
    let stored = json!({"message": message, "pr": input.pr, "attachments": input.attachments, "context": input.context});
    let (id, mut status, mut issue_url) = state.store.save_report(
        &id,
        &key,
        reporter.as_ref().map(|c| c.actor.public_id.as_str()),
        reporter.as_ref().map(|c| c.org_id.as_str()),
        message,
        input.pr.as_deref(),
        &stored,
        if testing { "testing" } else { "production" },
    )?;
    if status == "stored"
        && !testing
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
    state.telemetry.backend("report.received", "report", "ok", None, json!({"has_pr": input.pr.is_some(), "attachments": input.attachments.len(), "status": status}));
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
            HeaderValue::from_static("POST, OPTIONS"),
        );
        h.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("content-type, x-spotify-telemetry, x-spotify-source"),
        );
        h.insert(header::VARY, HeaderValue::from_static("Origin"));
    }
}

async fn preflight(State(state): Shared, headers: HeaderMap) -> Response {
    let mut response = StatusCode::NO_CONTENT.into_response();
    cors(&state, &headers, &mut response);
    response
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TelemetryInput {
    table: String,
    events: Vec<Value>,
}

async fn telemetry(State(state): Shared, headers: HeaderMap, body: Bytes) -> Response {
    let mut response = match relay_batch(&state, &headers, &body) {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error.into_response(),
    };
    // Refusals carry CORS headers too, so the website can read why a batch was refused.
    cors(&state, &headers, &mut response);
    response
}

fn relay_batch(state: &AppState, headers: &HeaderMap, body: &Bytes) -> AppResult<()> {
    let off = headers
        .get("x-spotify-telemetry")
        .and_then(|v| v.to_str().ok())
        == Some("off");
    if off {
        return Ok(());
    }
    if body.len() > MAX_TELEMETRY_BYTES {
        return Err(AppError::payload_too_large(
            format!(
                "The telemetry batch is {} KiB; the gateway relays at most {} KiB.",
                body.len().div_ceil(1024),
                MAX_TELEMETRY_BYTES / 1024
            ),
            "Split it into smaller batches (at most 64 KiB and 40 events each) and send them separately.",
        ));
    }
    let input: TelemetryInput = parse(body)?;
    let allowed = [
        silicon_spotify_client::telemetry::CLI_DAEMON_TABLE,
        silicon_spotify_client::telemetry::WEB_ANALYTICS_TABLE,
        silicon_spotify_client::telemetry::WEB_EVENTS_TABLE,
    ];
    if !allowed.contains(&input.table.as_str()) {
        return Err(AppError::invalid(
            format!("`{}` is not a relayable table.", input.table),
            format!("Use one of: {}.", allowed.join(", ")),
        ));
    }
    // Browser events must come from our own site.
    let from_browser = headers.contains_key(header::ORIGIN);
    if from_browser {
        let origin = headers
            .get(header::ORIGIN)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !state.settings.web_origins.iter().any(|o| o == origin)
            || input.table == silicon_spotify_client::telemetry::CLI_DAEMON_TABLE
        {
            return Err(AppError::forbidden(
                "This origin may not send telemetry.",
                "Browsers may relay only the website tables, and only from the spotify-cli website; the CLI and daemon send without an Origin header.",
            ));
        }
    }
    if input.events.is_empty() || input.events.len() > 40 {
        return Err(AppError::invalid(
            "Send 1-40 events per batch.",
            "Split larger batches into several requests; skip the request when there is nothing to send.",
        ));
    }
    let source = headers
        .get("x-spotify-source")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown")
        .chars()
        .take(16)
        .collect::<String>();
    for mut event in input.events {
        if !event.is_object() {
            continue;
        }
        event["relay"] = json!({"received_at": silicon_spotify_client::model::now_rfc3339(), "source": source, "backend_version": env!("CARGO_PKG_VERSION")});
        state.telemetry.relay(&input.table, event);
    }
    Ok(())
}

async fn webhook(State(state): Shared, headers: HeaderMap, body: Bytes) -> AppResult<Response> {
    let event = state.identity.verify_webhook(&headers, &body)?;
    let first = state
        .store
        .first_webhook(&event.event_id.to_string(), &event.event_type)?;
    if first {
        tracing::info!(event = %event.event_type, id = %event.event_id, "IAM webhook");
        state.telemetry.backend(
            "iam.webhook.received",
            &event.event_type,
            "ok",
            None,
            json!({"event_type": event.event_type}),
        );
    }
    Ok(Json(json!({"received": true, "duplicate": !first})).into_response())
}

/// Builds the production state.
///
/// # Errors
/// Configuration problems.
pub fn production(settings: Settings) -> anyhow::Result<Arc<AppState>> {
    let identity: Arc<dyn Identity> = Arc::new(Iam::new(&settings)?);
    let ting: Arc<dyn TingApi> = Arc::new(ting::TingHttp::new(
        settings.ting_url.clone(),
        settings.ting_timeout,
    )?);
    let store = Arc::new(Store::open(&settings.database_path)?);
    let telemetry = Recorder::new(&settings);
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()?;
    Ok(Arc::new(AppState {
        testing: Arc::new(IamTestingPlanes::new(settings.clone())),
        settings,
        identity,
        ting,
        store,
        telemetry,
        http,
    }))
}
