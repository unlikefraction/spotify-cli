//! Silicon IAM integration (official SDK, `silicon-iam-client` 4.0.0).
//!
//! Mirrors the reviewed patterns of other IAM apps: sessions are validated against IAM's own
//! authorization snapshot, every bearer is introspected live with its organization, OBO proofs are
//! minted per request and bound to the exact body bytes, and testing planes are selected only by a
//! verified testing application secret, never by falling back to production.

use std::collections::BTreeSet;
use std::sync::Arc;

use async_trait::async_trait;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;
use serde_json::{Value, json};
use silicon_iam_client::{
    Client, Credential, EnvironmentKey, IdempotencyKey, Mutation, WebhookSecret,
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

    /// Delegated scope that must be in the user's token.
    #[must_use]
    pub fn scope(self) -> String {
        format!("obo:ting:{}", self.id())
    }
}

/// A single-use OBO proof plus Ting's testing headers (testing planes only).
pub struct Proof {
    /// `Authorization: Bearer <proof>`.
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
    /// Mints a Ting proof bound to `body`.
    async fn ting_proof(
        &self,
        context: &AuthContext,
        endpoint: TingEndpoint,
        body: &[u8],
        attempt_key: &str,
    ) -> AppResult<Proof>;
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
    client: Client,
    app_id: String,
    environment_id: Option<Uuid>,
    verifier: Arc<WebhookVerifier>,
}

fn dependency() -> AppError {
    AppError::dependency("iam")
}

fn map_error(error: &silicon_iam_client::Error) -> AppError {
    if let Some(api) = error.api() {
        if api.code == "invalid_client" {
            tracing::error!(
                "IAM rejected the application credential (invalid_client): check SPOTIFY_IAM_APP_SECRET"
            );
            return dependency();
        }
        if matches!(api.status, 400 | 401 | 410)
            && matches!(
                api.code.as_str(),
                "invalid_grant" | "refresh_token_reuse" | "unauthenticated" | "invalid_token"
            )
        {
            return AppError::unauthenticated()
                .with_details(json!({"iam_code": api.code, "request_id": api.request_id}));
        }
        return match api.status {
            403 | 404 => AppError::forbidden(
                format!("IAM refused: {} ({}).", api.message, api.code),
                "Check the app's scopes and your consent; log in again if scopes changed.",
            )
            .with_details(json!({"iam_code": api.code, "request_id": api.request_id})),
            409 => AppError::conflict(
                "IAM rejected a reused or conflicting operation.",
                "Retry the original request with its original idempotency key, or log in again.",
            )
            .with_details(json!({"iam_code": api.code})),
            422 => AppError::invalid(
                format!("IAM rejected the request: {}.", api.message),
                "Check the input.",
            ),
            429 => AppError::rate_limited(),
            _ => dependency(),
        };
    }
    if matches!(error, silicon_iam_client::Error::RateLimited { .. }) {
        return AppError::rate_limited();
    }
    tracing::warn!(dependency = "iam", "IAM request failed");
    dependency()
}

fn validate_token(value: &str, prefix: &str) -> AppResult<()> {
    if !value.starts_with(prefix)
        || value.len() <= prefix.len()
        || value.len() > 8192
        || !value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
    {
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
            || !(public_id.starts_with("si:") || public_id.starts_with("c:"))
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
        let mut org_ids: Vec<String> = Vec::new();
        for grant in &grants {
            if grant.audience != self.app_id || grant.testing_environment_id != self.environment_id
            {
                return Err(AppError::unauthenticated());
            }
            if !org_ids.contains(&grant.org_id) {
                org_ids.push(grant.org_id.clone());
            }
        }
        org_ids.sort();
        let org_id = response
            .org_id
            .clone()
            .filter(|o| org_ids.contains(o))
            .or_else(|| org_ids.first().cloned())
            .ok_or_else(|| {
                AppError::forbidden(
                    "The login shared no organization with spotify-cli.",
                    "Mint the SLT with --grant-org <org> (for Silicons: --grant-org \"$SILICON_ORG\").",
                )
            })?;
        let context = self
            .bearer_context(&SecretString::from(response.access_token.clone()), &org_id)
            .await?;
        if let Some(actor) = &response.actor
            && actor.public_id != context.actor.public_id
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
        })
    }
}

#[async_trait]
impl Identity for Iam {
    async fn login(&self, slt: &SecretString, key: &str) -> AppResult<AppSession> {
        let value = slt.expose_secret();
        if value.is_empty() || value.len() > 8192 || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(AppError::unauthenticated());
        }
        let response = self
            .client
            .oauth()
            .login(&self.app_id, value, &mutation("login", key)?)
            .await
            .map_err(|e| map_error(&e))?;
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
            .map_err(|e| map_error(&e))?;
        self.validated(response).await
    }

    async fn logout(&self, token: &SecretString, key: &str) -> AppResult<()> {
        let hint = if token.expose_secret().starts_with("ort_") {
            validate_token(token.expose_secret(), "ort_")?;
            models::OAuthRevocationRequestTokenTypeHint::RefreshToken
        } else {
            validate_token(token.expose_secret(), "oat_")?;
            models::OAuthRevocationRequestTokenTypeHint::AccessToken
        };
        self.client
            .oauth()
            .revoke(
                &models::OAuthRevocationRequest {
                    token: token.expose_secret().to_owned(),
                    token_type_hint: Some(hint),
                },
                &mutation("logout", key)?,
            )
            .await
            .map_err(|e| map_error(&e))
    }

    async fn authenticate(&self, token: &SecretString, org_id: &str) -> AppResult<AuthContext> {
        self.bearer_context(token, org_id).await
    }

    async fn ting_proof(
        &self,
        context: &AuthContext,
        endpoint: TingEndpoint,
        body: &[u8],
        attempt_key: &str,
    ) -> AppResult<Proof> {
        if !context.has(&endpoint.scope()) || !context.has("self.identity.read") {
            return Err(AppError::new(
                axum::http::StatusCode::FORBIDDEN,
                "reconsent_required",
                format!(
                    "Your spotify-cli session lacks the `{}` and `self.identity.read` scopes Ting needs.",
                    endpoint.scope()
                ),
                "Log in again approving all scopes: iam silicon-login --app-id spotify --grant-org <org> --approve-scopes, then spotify login '<SLT>'. (Refresh never adds scopes.)",
            ));
        }
        let catalog = self
            .client
            .obo()
            .endpoints(TING)
            .await
            .map_err(|e| map_error(&e))?;
        let definition = catalog
            .endpoints
            .iter()
            .find(|e| e.endpoint_id == endpoint.id())
            .ok_or_else(|| {
                AppError::forbidden(
                    format!("Ting does not publish `{}` to this app.", endpoint.id()),
                    "The app's external scopes may not be approved yet.",
                )
            })?;
        if definition.path != endpoint.path()
            || !definition
                .metadata
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
        {
            return Err(dependency());
        }
        let key = IdempotencyKey::parse(attempt_key)
            .map_err(|_| AppError::internal("bad OBO attempt key"))?;
        let proof = self
            .client
            .obo()
            .exchange_signed(
                &models::OboExchangeRequest {
                    org_id: Some(context.org_id.clone()),
                    subject_token: context.token.expose_secret().to_owned(),
                    audience: TING.to_owned(),
                    endpoint_id: endpoint.id().to_owned(),
                    metadata: json!({}),
                    request: models::OboExchangeRequestBinding {
                        method: "POST".to_owned(),
                        body_sha256: silicon_iam_client::api::obo::body_sha256(body),
                    },
                },
                &catalog,
                &Mutation::with_key(key),
            )
            .await
            .map_err(|e| map_error(&e))?;
        if !(1..=60).contains(&proof.expires_in)
            || proof.access_proof.is_empty()
            || proof.access_proof.len() > 16_384
        {
            return Err(dependency());
        }
        let testing = match (self.environment_id, proof.testing_context) {
            (None, None) => None,
            (Some(_), Some(testing)) if testing.app_id == TING => Some((
                SecretString::from(testing.app_secret),
                SecretString::from(testing.iam_test_key),
            )),
            // A production request must never receive test credentials, and a test request never
            // falls back to production Ting.
            _ => {
                return Err(AppError::forbidden(
                    "IAM returned a testing context that does not match this plane.",
                    "Report it with `spotify report`.",
                ));
            }
        };
        Ok(Proof {
            token: SecretString::from(proof.access_proof),
            testing,
        })
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
                .map_err(|_| AppError::forbidden("Webhook testing environment mismatch.", ""))?;
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
