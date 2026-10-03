//! Silicon IAM integration (official SDK, `silicon-iam-client` 5.2.1).
//!
//! Mirrors the reviewed patterns of other IAM apps: sessions are validated against IAM's own
//! authorization snapshot, every bearer is introspected live with its organization, Ting uses separately approved, encrypted reusable OBO credentials, and testing planes are selected only by a
//! verified testing application secret, never by falling back to production.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;
use serde_json::{Value, json};
use silicon_iam_client::{
    ApiError, Client, Credential, EnvironmentKey, IdempotencyKey, Mutation, WebhookSecret,
    WebhookSecretKeyring, WebhookVerifier, models,
};
use uuid::Uuid;

use crate::config::Settings;
use crate::error::{AppError, AppResult};

/// Ting's bare application id (OBO audience).
pub const TING: &str = "ting";

/// An authenticated actor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Actor {
    /// `silicon` or `carbon`.
    #[serde(rename = "type")]
    pub kind: String,
    /// `si:<handle>` or `c:<handle>`.
    pub public_id: String,
}

/// A validated application session (what the CLI stores).
#[derive(Clone, Debug, Serialize)]
pub struct AppSession {
    /// `oat_…`.
    pub access_token: String,
    /// `ort_…`.
    pub refresh_token: String,
    /// `Bearer`.
    pub token_type: &'static str,
    /// Seconds.
    pub expires_in: i64,
    /// Space-separated scopes.
    pub scope: String,
    /// Who.
    pub actor: Actor,
    /// Selected organization.
    pub org_id: String,
    /// All organizations the token reaches.
    pub org_ids: Vec<String>,
    /// Testing plane, when any.
    pub testing_environment_id: Option<Uuid>,
    /// Opaque testing cleaning/key generation binding; absent in production.
    pub testing_generation: Option<String>,
}

/// A verified bearer for one organization.
#[derive(Clone, Debug)]
pub struct AuthContext {
    /// Who.
    pub actor: Actor,
    /// Organization handle.
    pub org_id: String,
    /// Membership (`si:x[org]`).
    pub membership_id: String,
    /// IAM session id.
    pub session_id: Option<Uuid>,
    /// Granted scopes.
    pub scopes: BTreeSet<String>,
    /// The bearer itself (subject token for OBO).
    pub token: SecretString,
}

impl AuthContext {
    /// Whether a scope was granted.
    #[must_use]
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}

/// A Ting endpoint this app calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TingEndpoint {
    /// `subscriptions.register` → `POST /v1/subscriptions`.
    Register,
    /// `tings.send` → `POST /v1/tings`.
    Send,
}

impl TingEndpoint {
    /// IAM endpoint id.
    #[must_use]
    pub fn id(self) -> &'static str {
        match self {
            Self::Register => "subscriptions.register",
            Self::Send => "tings.send",
        }
    }

    /// Ting path.
    #[must_use]
    pub fn path(self) -> &'static str {
        match self {
            Self::Register => "/v1/subscriptions",
            Self::Send => "/v1/tings",
        }
    }
}

/// A reusable OBO access token plus its selected provider destination.
pub struct TingAccess {
    /// Provider account selected during feature consent.
    pub actor: Actor,
    /// Provider organization selected during feature consent.
    pub org_id: String,
    /// `Authorization: Bearer <obo_access_token>`.
    pub token: SecretString,
    /// (`IAM_TEST_APP_SECRET`, `X-Testing-Environment-Key`) for Ting's own test plane.
    pub testing: Option<(SecretString, SecretString)>,
}

/// What the HTTP layer needs from IAM (a trait so tests can use a fake).
#[async_trait]
pub trait Identity: Send + Sync {
    /// Exchanges an SLT.
    async fn login(&self, slt: &SecretString, key: &str) -> AppResult<AppSession>;
    /// Rotates a refresh token.
    async fn refresh(&self, token: &SecretString, key: &str) -> AppResult<AppSession>;
    /// Revokes a refresh (or access) token's family.
    async fn logout(&self, token: &SecretString, key: &str) -> AppResult<()>;
    /// Verifies a bearer live for one organization.
    async fn authenticate(&self, token: &SecretString, org_id: &str) -> AppResult<AuthContext>;
    /// Loads or refreshes a reusable Ting token and pins the operation destination.
    async fn ting_access(
        &self,
        context: &AuthContext,
        endpoint: TingEndpoint,
        body: &[u8],
        attempt_key: &str,
    ) -> AppResult<TingAccess>;
    /// Starts feature-specific Ting authorization.
    async fn ting_authorize(&self, _context: &AuthContext, _key: &str) -> AppResult<Value> {
        Err(crate::obo::required())
    }
    /// Reads a context-bound feature authorization.
    async fn ting_authorization(&self, _context: &AuthContext, _id: &str) -> AppResult<Value> {
        Err(crate::obo::required())
    }
    /// Redeems the one-use IAM code and durably stores each root family.
    async fn ting_complete(
        &self,
        _context: &AuthContext,
        _id: &str,
        _code: &str,
    ) -> AppResult<Value> {
        Err(crate::obo::required())
    }
    /// Verifies an IAM webhook delivery.
    fn verify_webhook(
        &self,
        headers: &axum::http::HeaderMap,
        body: &[u8],
    ) -> AppResult<models::WebhookEvent>;
    /// The testing plane this adapter is bound to.
    fn environment_id(&self) -> Option<Uuid>;
}

/// The real adapter.
pub struct Iam {
    pub(crate) client: Client,
    pub(crate) app_id: String,
    pub(crate) environment_id: Option<Uuid>,
    verifier: Arc<WebhookVerifier>,
    pub(crate) obo: crate::obo::Store,
    pub(crate) generation: String,
}

fn dependency() -> AppError {
    AppError::dependency("iam")
}

/// Codes with which IAM refuses the token a request carries (never this app's own credential).
const TOKEN_REFUSALS: [&str; 5] = [
    "invalid_grant",
    "invalid_token",
    "refresh_token_reuse",
    "token_expired",
    "token_revoked",
];

/// Whether IAM refused this application's credential (a backend misconfiguration, never the user).
fn client_rejected(api: &ApiError) -> bool {
    matches!(api.code.as_str(), "invalid_client" | "unauthorized_client")
}

/// Whether IAM refused the token in the request body (unknown, malformed, expired, used or
/// revoked) rather than this app's credential, the idempotency key or a rate limit.
fn token_refused(api: &ApiError) -> bool {
    !client_rejected(api)
        && !api.is_idempotency_conflict()
        && (matches!(api.status, 400 | 410 | 422)
            || (api.status == 401 && TOKEN_REFUSALS.contains(&api.code.as_str())))
}

/// The error for an explicit IAM refusal of the presented token.
fn refused_by_iam(refused: AppError, api: &ApiError) -> AppError {
    refused.with_details(json!({"iam_code": api.code, "request_id": api.request_id}))
}

pub(crate) fn map_error(error: &silicon_iam_client::Error) -> AppError {
    if let Some(api) = error.api() {
        if client_rejected(api) {
            tracing::error!(
                status = api.status,
                code = %api.code,
                "IAM rejected the application credential: check SPOTIFY_IAM_APP_SECRET"
            );
            return dependency();
        }
        if matches!(api.status, 400 | 401 | 410)
            && (api.code == "unauthenticated" || TOKEN_REFUSALS.contains(&api.code.as_str()))
        {
            return refused_by_iam(AppError::unauthenticated(), api);
        }
        return match api.status {
            // IAM's `code` is the contract; an idempotency conflict is one whatever its status.
            status if status == 409 || api.is_idempotency_conflict() => AppError::conflict(
                "IAM rejected a reused or conflicting operation.",
                "Retry the original request with its original idempotency key, or log in again.",
            )
            .with_details(json!({"iam_code": api.code})),
            400 => AppError::invalid(
                format!("IAM rejected the request: {}.", api.message),
                "Check the values sent (see `spotify docs api`); a malformed token needs a fresh `spotify login '<SLT>'`.",
            )
            .with_details(json!({"iam_code": api.code, "request_id": api.request_id})),
            403 | 404 => AppError::forbidden(
                format!("IAM refused: {} ({}).", api.message, api.code),
                "Check the app's scopes and your consent; log in again if scopes changed.",
            )
            .with_details(json!({"iam_code": api.code, "request_id": api.request_id})),
            422 => AppError::invalid(
                format!("IAM rejected the request: {}.", api.message),
                "Check the input.",
            ),
            429 => AppError::rate_limited(),
            status => {
                // Status, code and IAM's request id only: bodies and messages may echo input.
                tracing::warn!(
                    dependency = "iam",
                    status,
                    code = %api.code,
                    iam_request_id = ?api.request_id,
                    "IAM answered with an unexpected error"
                );
                dependency()
            }
        };
    }
    if matches!(error, silicon_iam_client::Error::RateLimited { .. }) {
        return AppError::rate_limited();
    }
    tracing::warn!(
        dependency = "iam",
        failure = failure_kind(error),
        status = ?unstructured_status(error),
        iam_request_id = ?error.request_id(),
        "IAM request failed"
    );
    dependency()
}

/// The kind of a non-envelope failure, for logs (transport and decode texts may echo input).
fn failure_kind(error: &silicon_iam_client::Error) -> &'static str {
    match error {
        silicon_iam_client::Error::Transport(e) if e.is_timeout() => "timeout",
        silicon_iam_client::Error::Transport(_) => "transport",
        silicon_iam_client::Error::Decode(_) => "decode",
        silicon_iam_client::Error::UnstructuredResponse { .. } => "unstructured_response",
        silicon_iam_client::Error::ResponseTooLarge { .. } => "response_too_large",
        silicon_iam_client::Error::ApiVersionUnsupported { .. } => "api_version_unsupported",
        silicon_iam_client::Error::Invalid(_) => "invalid_request",
        _ => "other",
    }
}

fn unstructured_status(error: &silicon_iam_client::Error) -> Option<u16> {
    match error {
        silicon_iam_client::Error::UnstructuredResponse { status, .. } => Some(*status),
        _ => None,
    }
}

/// IAM refused an SLT login. A refusal of the SLT itself (including a malformed-request 400 or an
/// expired-SLT 410) is `slt_rejected`; the app's credential, idempotency and rate limits keep
/// their own mapping.
fn login_error(error: &silicon_iam_client::Error) -> AppError {
    match error.api() {
        Some(api) if token_refused(api) || (api.status == 401 && api.code == "unauthenticated") => {
            refused_by_iam(AppError::slt_rejected(), api)
        }
        _ => map_error(error),
    }
}

/// IAM refused a refresh. Clients delete the saved session on a 401, so only an explicit refusal
/// of the refresh token (a token code, or 410 Gone) becomes `unauthenticated`. A plain 400 or 422
/// means the request itself was malformed, which an IAM-issued refresh token never causes; it
/// must not wipe every saved session, so it stays `invalid_input`.
fn refresh_error(error: &silicon_iam_client::Error) -> AppError {
    match error.api() {
        Some(api) if api.status == 410 && token_refused(api) => {
            refused_by_iam(AppError::unauthenticated(), api)
        }
        _ => map_error(error),
    }
}

/// RFC 7009: revoking a token IAM does not recognise (unknown, malformed, expired or already
/// revoked) is a success, since nothing it could authorize remains. A rejected app credential,
/// an idempotency conflict or a rate limit is still an error.
fn revocation(result: Result<(), silicon_iam_client::Error>) -> AppResult<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) => match error.api() {
            Some(api) if token_refused(api) => {
                tracing::info!(
                    status = api.status,
                    code = %api.code,
                    "IAM did not recognise the token to revoke; logout is complete"
                );
                Ok(())
            }
            _ => Err(map_error(&error)),
        },
    }
}

/// Whether `value` has the shape of an IAM token with `prefix` (`oat_`, `ort_`).
pub(crate) fn well_formed(value: &str, prefix: &str) -> bool {
    value.starts_with(prefix)
        && value.len() > prefix.len()
        && value.len() <= 8192
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
}

fn validate_token(value: &str, prefix: &str) -> AppResult<()> {
    if !well_formed(value, prefix) {
        return Err(AppError::unauthenticated());
    }
    Ok(())
}

fn mutation(operation: &str, key: &str) -> AppResult<Mutation> {
    // Namespaced digest: stable for the caller's key, never exposes tokens.
    let digest = blake3::hash(format!("spotify-auth:{operation}:{key}").as_bytes()).to_hex();
    let key = IdempotencyKey::parse(digest.as_str()).map_err(|_| {
        AppError::invalid(
            "Invalid idempotency key.",
            "Send 16-255 visible ASCII characters.",
        )
    })?;
    Ok(Mutation::with_key(key))
}

fn actor_type(kind: Option<&models::ApplicationAuthorizationActorType>) -> AppResult<&'static str> {
    match kind.ok_or_else(AppError::unauthenticated)? {
        models::ApplicationAuthorizationActorType::Carbon => Ok("carbon"),
        models::ApplicationAuthorizationActorType::Silicon => Ok("silicon"),
        models::ApplicationAuthorizationActorType::Other(_) => Err(AppError::unauthenticated()),
    }
}

impl Iam {
    /// Production adapter.
    ///
    /// # Errors
    /// Invalid IAM URL or credentials.
    pub fn new(settings: &Settings) -> anyhow::Result<Self> {
        Self::build(settings, settings.app_secret.clone(), None)
    }

    fn build(
        settings: &Settings,
        secret: SecretString,
        environment: Option<(EnvironmentKey, Uuid)>,
    ) -> anyhow::Result<Self> {
        let mut builder = Client::builder(settings.iam_url.as_str())?
            .credential(Credential::Application {
                app_id: settings.app_id.clone(),
                secret,
            })
            .timeout(settings.iam_timeout)
            .telemetry(settings.telemetry)
            .user_agent(format!("silicon-spotify/{}", env!("CARGO_PKG_VERSION")));
        let environment_id = environment.as_ref().map(|(_, id)| *id);
        if let Some((key, _)) = environment {
            builder = builder.environment(key);
        }
        let keyring = WebhookSecretKeyring::new(
            settings.webhook_key_version,
            WebhookSecret::new(settings.webhook_secret.expose_secret().to_owned())
                .map_err(|e| anyhow::anyhow!("SPOTIFY_IAM_WEBHOOK_SECRET: {e}"))?,
        )
        .map_err(|e| anyhow::anyhow!("webhook key version: {e}"))?;
        Ok(Self {
            client: builder.build()?,
            obo: crate::obo::Store::open(&settings.database_path, &settings.encryption_key)?,
            generation: "production".into(),
            app_id: settings.app_id.clone(),
            environment_id,
            verifier: Arc::new(WebhookVerifier::new(keyring)),
        })
    }

    /// Resolves a testing plane from its application secret alone (verified by IAM).
    ///
    /// # Errors
    /// `unauthenticated` for a malformed or unknown secret; `dependency_unavailable` otherwise.
    pub async fn discover(settings: &Settings, secret: &str) -> AppResult<Self> {
        if !secret.starts_with("ask_") || secret.len() != 47 {
            return Err(AppError::new(
                axum::http::StatusCode::UNAUTHORIZED,
                "invalid_testing_secret",
                "X-Testing-Environment-Key must be this app's 47-character ask_… secret inside an IAM testing environment.",
                "Select a plane with `spotify testing use --app-secret-file -`.",
            ));
        }
        let secret = SecretString::from(secret.to_owned());
        let mut adapter = Self::build(settings, secret.clone(), None).map_err(|_| dependency())?;
        adapter.client = adapter
            .client
            .with_testing_application(&settings.app_id, secret.expose_secret())
            .map_err(|e| map_error(&e))?;
        let context = adapter
            .client
            .applications()
            .testing_context()
            .await
            .map_err(|e| map_error(&e))?;
        if context.application.app_id != settings.app_id || context.environment_id.is_nil() {
            return Err(AppError::unauthenticated());
        }
        adapter.environment_id = Some(context.environment_id);
        let environment = context.environment.ok_or_else(dependency)?;
        adapter.generation =
            json!([environment.key_generation, environment.cleaned_at]).to_string();
        Ok(adapter)
    }

    async fn bearer_context(&self, token: &SecretString, org_id: &str) -> AppResult<AuthContext> {
        validate_token(token.expose_secret(), "oat_")?;
        let inspected = self
            .client
            .oauth()
            .introspect(
                &models::TokenIntrospectionRequest {
                    token: token.expose_secret().to_owned(),
                    token_type_hint: Some(
                        models::TokenIntrospectionRequestTokenTypeHint::AccessToken,
                    ),
                },
                Some(org_id),
            )
            .await
            .map_err(|e| map_error(&e))?;
        if !inspected.active {
            return Err(AppError::unauthenticated());
        }
        let snapshot = inspected.authorization.ok_or_else(dependency)?;
        let kind = actor_type(snapshot.actor_type.as_ref())?;
        let scopes: BTreeSet<String> = snapshot.scopes.iter().cloned().collect();
        let public_id = snapshot.public_id.clone().ok_or_else(|| {
            AppError::forbidden(
                "IAM did not disclose your identity to spotify-cli (self.identity.read).",
                "Log in again and approve the requested scopes: iam silicon-login --app-id spotify --grant-org <org> --approve-scopes",
            )
        })?;
        if inspected.audience.as_deref() != Some(self.app_id.as_str())
            || inspected.client_id.as_deref() != Some(self.app_id.as_str())
            || inspected.org_id.as_deref() != Some(org_id)
            || snapshot.org_id != org_id
            || snapshot.audience != self.app_id
            || snapshot.testing_environment_id != self.environment_id
            || inspected
                .public_id
                .as_ref()
                .is_some_and(|id| *id != public_id)
            || !(if kind == "silicon" {
                public_id.starts_with("si:")
            } else {
                public_id.starts_with("c:")
            })
        {
            return Err(AppError::unauthenticated());
        }
        Ok(AuthContext {
            actor: Actor {
                kind: kind.to_owned(),
                public_id,
            },
            org_id: org_id.to_owned(),
            membership_id: snapshot.membership_id,
            session_id: inspected.session_id,
            scopes,
            token: token.clone(),
        })
    }

    async fn validated(&self, response: models::OAuthTokenResponse) -> AppResult<AppSession> {
        validate_token(&response.access_token, "oat_")?;
        validate_token(&response.refresh_token, "ort_")?;
        if response.token_type.as_str() != Some("Bearer") || response.expires_in <= 0 {
            return Err(dependency());
        }
        let grants = self
            .client
            .oauth()
            .authorizations(&response.access_token)
            .await
            .map_err(|e| map_error(&e))?
            .ok_or_else(AppError::unauthenticated)?;
        let org_id = response
            .org_id
            .clone()
            .filter(|o| !o.is_empty())
            .ok_or_else(AppError::unauthenticated)?;
        if grants.len() != 1
            || grants[0].org_id != org_id
            || grants[0].audience != self.app_id
            || grants[0].testing_environment_id != self.environment_id
        {
            return Err(AppError::unauthenticated());
        }
        let org_ids = vec![org_id.clone()];
        let context = self
            .bearer_context(&SecretString::from(response.access_token.clone()), &org_id)
            .await?;
        if let Some(actor) = &response.actor
            && (actor.public_id != context.actor.public_id
                || serde_json::to_value(&actor.type_field).ok() != Some(json!(context.actor.kind)))
        {
            return Err(AppError::unauthenticated());
        }
        Ok(AppSession {
            access_token: response.access_token,
            refresh_token: response.refresh_token,
            token_type: "Bearer",
            expires_in: response.expires_in,
            scope: context.scopes.iter().cloned().collect::<Vec<_>>().join(" "),
            actor: context.actor,
            org_id,
            org_ids,
            testing_environment_id: self.environment_id,
            testing_generation: self.environment_id.map(|_| self.generation.clone()),
        })
    }
}

#[async_trait]
impl Identity for Iam {
    async fn login(&self, slt: &SecretString, key: &str) -> AppResult<AppSession> {
        let value = slt.expose_secret();
        if value.is_empty() || value.len() > 8192 || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(AppError::slt_rejected());
        }
        let response = self
            .client
            .oauth()
            .login(&self.app_id, value, &mutation("login", key)?)
            .await
            .map_err(|e| login_error(&e))?;
        self.validated(response).await
    }

    async fn refresh(&self, token: &SecretString, key: &str) -> AppResult<AppSession> {
        validate_token(token.expose_secret(), "ort_")?;
        let response = self
            .client
            .oauth()
            .refresh(
                &self.app_id,
                token.expose_secret(),
                &mutation("refresh", key)?,
            )
            .await
            .map_err(|e| refresh_error(&e))?;
        self.validated(response).await
    }

    async fn logout(&self, token: &SecretString, key: &str) -> AppResult<()> {
        let (prefix, hint) = if token.expose_secret().starts_with("ort_") {
            (
                "ort_",
                models::OAuthRevocationRequestTokenTypeHint::RefreshToken,
            )
        } else {
            (
                "oat_",
                models::OAuthRevocationRequestTokenTypeHint::AccessToken,
            )
        };
        if !well_formed(token.expose_secret(), prefix) {
            // IAM never issued a token of this shape, so nothing it authorizes can remain (RFC 7009).
            return Ok(());
        }
        revocation(
            self.client
                .oauth()
                .revoke(
                    &models::OAuthRevocationRequest {
                        token: token.expose_secret().to_owned(),
                        token_type_hint: Some(hint),
                    },
                    &mutation("logout", key)?,
                )
                .await,
        )
    }

    async fn authenticate(&self, token: &SecretString, org_id: &str) -> AppResult<AuthContext> {
        self.bearer_context(token, org_id).await
    }

    async fn ting_access(
        &self,
        context: &AuthContext,
        endpoint: TingEndpoint,
        body: &[u8],
        attempt_key: &str,
    ) -> AppResult<TingAccess> {
        crate::obo::access(self, context, endpoint, body, attempt_key).await
    }

    async fn ting_authorize(&self, context: &AuthContext, key: &str) -> AppResult<Value> {
        crate::obo::start(self, context, key).await
    }

    async fn ting_authorization(&self, context: &AuthContext, id: &str) -> AppResult<Value> {
        crate::obo::status(self, context, id).await
    }

    async fn ting_complete(&self, context: &AuthContext, id: &str, code: &str) -> AppResult<Value> {
        crate::obo::complete(self, context, id, code).await
    }

    fn verify_webhook(
        &self,
        headers: &axum::http::HeaderMap,
        body: &[u8],
    ) -> AppResult<models::WebhookEvent> {
        let delivery = self
            .verifier
            .verify(headers, body)
            .map_err(|error| {
                tracing::warn!(reason = %error, key_version = ?headers.get("x-silicon-iam-key-version"), "IAM webhook rejected");
                AppError::new(
                    axum::http::StatusCode::UNAUTHORIZED,
                    "webhook_unverified",
                    format!("The webhook delivery did not verify: {error}."),
                    "Only Silicon IAM deliveries signed with this application's webhook secret are accepted.",
                )
            })?;
        if delivery.is_testing() != self.environment_id.is_some() {
            return Err(AppError::forbidden(
                "Webhook plane mismatch.",
                "Production and testing deliveries never mix.",
            ));
        }
        if let Some(key) = self.client.environment() {
            delivery
                .verify_testing_environment(key)
                .map_err(|_| {
                    AppError::forbidden(
                        "Webhook testing environment mismatch.",
                        "Only deliveries for this backend's own IAM testing environment are accepted; check the application's webhook in that environment.",
                    )
                })?;
        }
        Ok(delivery.event().clone())
    }

    fn environment_id(&self) -> Option<Uuid> {
        self.environment_id
    }
}

/// Extracts the discovery fields a CLI may rely on.
#[must_use]
pub fn discovery(settings: &Settings, environment: Option<Uuid>) -> Value {
    json!({
        "app_id": settings.app_id,
        "org_id": silicon_spotify_client::OWNER_ORG,
        "api_version": "v1",
        "iam_url": settings.iam_url.as_str().trim_end_matches('/'),
        "ting_url": settings.ting_url.as_str().trim_end_matches('/'),
        "testing_environment_id": environment,
        "ting_types": silicon_spotify_client::TING_TYPES.iter().map(|(t, _)| *t).collect::<Vec<_>>(),
        "docs_url": silicon_spotify_client::DOCS_URL,
        "repository_url": silicon_spotify_client::REPOSITORY,
        "rust_package": "silicon-spotify-client",
        "version": env!("CARGO_PKG_VERSION"),
    })
}
