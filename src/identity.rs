//! Silicon Accounts sessions and Ting proofs, with recipient authority bound to an account UUID.
use crate::config::Settings;
use crate::error::{AppError, AppResult};
use async_trait::async_trait;
use secrecy::{ExposeSecret as _, SecretString};
use serde::Serialize;
use serde_json::{Value, json};
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Actor {
    #[serde(rename = "type")]
    pub kind: String,
    pub public_id: String,
    pub uuid: String,
}
#[derive(Clone, Debug, Serialize)]
pub struct AppSession {
    pub access_token: String,
    pub refresh_token: String,
    pub refresh_token_expires_at: Option<String>,
    pub token_type: &'static str,
    pub expires_in: i64,
    pub scope: String,
    pub actor: Actor,
}
#[derive(Clone, Debug)]
pub struct AuthContext {
    pub actor: Actor,
    pub membership_id: String,
    pub scopes: BTreeSet<String>,
    pub token: SecretString,
}
impl AuthContext {
    pub fn has(&self, scope: &str) -> bool {
        self.scopes.contains(scope)
    }
}
#[derive(Clone, Copy, Debug)]
pub enum TingEndpoint {
    Register,
    Send,
}
impl TingEndpoint {
    pub fn id(self) -> &'static str {
        match self {
            Self::Register => "subscriptions.register",
            Self::Send => "tings.send",
        }
    }
    pub fn path(self) -> &'static str {
        match self {
            Self::Register => "/v1/subscriptions",
            Self::Send => "/v1/tings",
        }
    }
}
pub struct TingAccess {
    pub actor: Actor,
    pub token: SecretString,
}
#[async_trait]
pub trait Identity: Send + Sync {
    async fn login(&self, slt: &SecretString, key: &str) -> AppResult<AppSession>;
    async fn code(
        &self,
        code: &SecretString,
        redirect_uri: &str,
        verifier: &SecretString,
    ) -> AppResult<AppSession>;
    async fn refresh(&self, token: &SecretString, key: &str) -> AppResult<AppSession>;
    async fn logout(&self, token: &SecretString, key: &str) -> AppResult<()>;
    async fn authenticate(&self, token: &SecretString) -> AppResult<AuthContext>;
    async fn ting_access(
        &self,
        context: &AuthContext,
        endpoint: TingEndpoint,
        body: &[u8],
        attempt_key: &str,
    ) -> AppResult<TingAccess>;
}
pub struct Accounts {
    http: reqwest::Client,
    base: url::Url,
    app_id: String,
    secret: SecretString,
    cache: crate::auth_cache::Store,
    exchange_lock: tokio::sync::Mutex<()>,
}
pub(crate) fn well_formed(value: &str) -> bool {
    !value.is_empty() && value.len() <= 8192 && value.bytes().all(|b| b.is_ascii_graphic())
}
fn required(v: &Value, field: &str) -> AppResult<String> {
    v.get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| AppError::dependency("accounts"))
}
fn actor(kind: String, id: String, uuid: String) -> AppResult<Actor> {
    if uuid::Uuid::parse_str(&uuid).is_err()
        || !(kind == "carbon" && id.starts_with("c:") || kind == "silicon" && id.starts_with("si:"))
    {
        return Err(AppError::unauthenticated());
    }
    Ok(Actor {
        kind,
        public_id: id,
        uuid,
    })
}
impl Accounts {
    pub fn new(settings: &Settings) -> anyhow::Result<Self> {
        Ok(Self {
            http: reqwest::Client::builder()
                .timeout(settings.accounts_timeout)
                .redirect(reqwest::redirect::Policy::none())
                .user_agent(concat!("silicon-spotify/", env!("CARGO_PKG_VERSION")))
                .build()?,
            base: settings.accounts_url.clone(),
            app_id: settings.app_id.clone(),
            secret: settings.app_secret.clone(),
            cache: crate::auth_cache::Store::open(
                &settings.database_path,
                &settings.encryption_key,
            )?,
            exchange_lock: tokio::sync::Mutex::new(()),
        })
    }
    fn request(&self, path: &str) -> AppResult<reqwest::RequestBuilder> {
        Ok(self
            .http
            .post(self.base.join(path).map_err(AppError::internal)?)
            .basic_auth(&self.app_id, Some(self.secret.expose_secret())))
    }
    async fn response(&self, request: reqwest::RequestBuilder) -> AppResult<Value> {
        let response = request
            .send()
            .await
            .map_err(|_| AppError::dependency("accounts"))?;
        let status = response.status();
        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            return Err(AppError::rate_limited());
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|_| AppError::dependency("accounts"))?;
        if bytes.len() > 1024 * 1024 {
            return Err(AppError::dependency("accounts"));
        }
        let value: Value = if bytes.is_empty() {
            json!({})
        } else {
            serde_json::from_slice(&bytes).map_err(|_| AppError::dependency("accounts"))?
        };
        if status.is_success() {
            return Ok(value);
        }
        let code = value
            .get("error")
            .and_then(Value::as_str)
            .or_else(|| value.pointer("/error/code").and_then(Value::as_str))
            .unwrap_or("accounts_error");
        if matches!(code, "invalid_client" | "unauthorized_client") {
            return Err(AppError::dependency("accounts")
                .with_details(json!({"dependency": "accounts", "accounts_code": code})));
        }
        if matches!(
            code,
            "invalid_grant"
                | "invalid_token"
                | "token_expired"
                | "token_revoked"
                | "refresh_token_reuse"
        ) {
            return Err(AppError::unauthenticated());
        }
        match status.as_u16() {
            401 => Err(AppError::unauthenticated()),
            403 => Err(AppError::forbidden(
                "Silicon Accounts denied this operation.",
                "Check the signed-in account's access to Spotify and Ting.",
            )),
            429 => Err(AppError::rate_limited()),
            _ => Err(AppError::dependency("accounts")
                .with_details(json!({"accounts_code": code, "status": status.as_u16()}))),
        }
    }
    async fn exchange(
        &self,
        grant: &str,
        field: &str,
        token: &SecretString,
        extra: &[(&str, &str)],
    ) -> AppResult<AppSession> {
        if !well_formed(token.expose_secret()) {
            return Err(AppError::unauthenticated());
        }
        // Accounts rotates refresh tokens without replay support. Reserve before sending and
        // replay our encrypted result; an uncertain exchange must never reuse a refresh token.
        let _lock = self.exchange_lock.lock().await;
        let mut form = {
            let mut form = url::form_urlencoded::Serializer::new(String::new());
            form.append_pair("grant_type", grant)
                .append_pair(field, token.expose_secret());
            for (name, value) in extra {
                form.append_pair(name, value);
            }
            form.finish()
        };
        let mut visited = BTreeSet::new();
        let value = loop {
            let key = blake3::hash(form.as_bytes()).to_hex().to_string();
            if !visited.insert(key.clone()) {
                return Err(AppError::dependency("accounts"));
            }
            if let Some(value) = self.cache.cached(&key)? {
                if value["expires_in"].as_i64().unwrap_or(0) > 0 {
                    self.authenticate(&SecretString::from(required(&value, "access_token")?))
                        .await?;
                    break value;
                }
                // Recover a lost response after its access token expires by following the
                // saved replacement refresh token. Every consumed token has its own cache row.
                let expiry = value["refresh_token_expires_at"].as_str().and_then(|s| {
                    time::OffsetDateTime::parse(s, &time::format_description::well_known::Rfc3339)
                        .ok()
                });
                if expiry.is_none_or(|expiry| expiry <= time::OffsetDateTime::now_utc()) {
                    return Err(AppError::unauthenticated());
                }
                let refresh = required(&value, "refresh_token")?;
                form = url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "refresh_token")
                    .append_pair("refresh_token", &refresh)
                    .finish();
                continue;
            }
            let request = self
                .request("/v1/oauth/token")?
                .header("content-type", "application/x-www-form-urlencoded")
                .body(form);
            self.cache.reserve(&key)?;
            let value = match self.response(request).await {
                Ok(value) => value,
                Err(error) => {
                    // These responses reject the request before consuming its one-use token.
                    // Transport errors and unknown upstream failures remain reserved.
                    if error.code == "rate_limited"
                        || matches!(
                            error
                                .details
                                .as_ref()
                                .and_then(|d| d["accounts_code"].as_str()),
                            Some("invalid_client" | "unauthorized_client")
                        )
                    {
                        self.cache.release(&key)?;
                    }
                    return Err(error);
                }
            };
            self.cache.complete(&key, &value)?;
            break value;
        };
        let account = value
            .get("account")
            .ok_or_else(|| AppError::dependency("accounts"))?;
        Ok(AppSession {
            access_token: required(&value, "access_token")?,
            refresh_token: required(&value, "refresh_token")?,
            refresh_token_expires_at: value
                .get("refresh_token_expires_at")
                .and_then(Value::as_str)
                .map(str::to_owned),
            token_type: "Bearer",
            expires_in: value
                .get("expires_in")
                .and_then(Value::as_i64)
                .filter(|n| *n > 0)
                .ok_or_else(AppError::unauthenticated)?,
            scope: value
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned(),
            actor: actor(
                required(account, "kind")?,
                required(account, "id")?,
                required(account, "uuid")?,
            )?,
        })
    }
}
#[async_trait]
impl Identity for Accounts {
    async fn login(&self, slt: &SecretString, _key: &str) -> AppResult<AppSession> {
        self.exchange("urn:silicon:params:oauth:grant-type:slt", "slt", slt, &[])
            .await
    }
    async fn code(
        &self,
        code: &SecretString,
        redirect_uri: &str,
        verifier: &SecretString,
    ) -> AppResult<AppSession> {
        self.exchange(
            "authorization_code",
            "code",
            code,
            &[
                ("redirect_uri", redirect_uri),
                ("code_verifier", verifier.expose_secret()),
            ],
        )
        .await
    }
    async fn refresh(&self, token: &SecretString, _key: &str) -> AppResult<AppSession> {
        self.exchange("refresh_token", "refresh_token", token, &[])
            .await
    }
    async fn logout(&self, token: &SecretString, _key: &str) -> AppResult<()> {
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token.expose_secret())
            .finish();
        self.response(
            self.request("/v1/oauth/revoke")?
                .header("content-type", "application/x-www-form-urlencoded")
                .body(form),
        )
        .await
        .map(drop)
    }
    async fn authenticate(&self, token: &SecretString) -> AppResult<AuthContext> {
        if !well_formed(token.expose_secret()) {
            return Err(AppError::unauthenticated());
        }
        let form = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token.expose_secret())
            .finish();
        let inspected = self
            .response(
                self.request("/v1/oauth/introspect")?
                    .header("content-type", "application/x-www-form-urlencoded")
                    .body(form),
            )
            .await?;
        let now = time::OffsetDateTime::now_utc().unix_timestamp();
        if inspected["active"] != true
            || inspected["aud"].as_str() != Some(&self.app_id)
            || inspected["exp"].as_i64().is_none_or(|exp| exp <= now)
        {
            return Err(AppError::unauthenticated());
        }
        let subject = required(&inspected, "sub")?;
        let membership = required(&inspected, "membership_id")?;
        if membership != format!("{}:{subject}", self.app_id) {
            return Err(AppError::unauthenticated());
        }
        Ok(AuthContext {
            actor: actor(
                required(&inspected, "kind")?,
                required(&inspected, "id")?,
                subject,
            )?,
            membership_id: membership,
            scopes: inspected["scope"]
                .as_str()
                .unwrap_or("")
                .split_whitespace()
                .map(str::to_owned)
                .collect(),
            token: token.clone(),
        })
    }
    async fn ting_access(
        &self,
        context: &AuthContext,
        endpoint: TingEndpoint,
        _body: &[u8],
        _attempt_key: &str,
    ) -> AppResult<TingAccess> {
        let (path, kind) = match endpoint {
            TingEndpoint::Register => ("/v1/proofs/user-verification", "user_verification"),
            TingEndpoint::Send => ("/v1/proofs/app-verification", "app_verification"),
        };
        let mut request =
            json!({"receiving_app": "ting", "scopes": [endpoint.id()], "access_ttl_seconds": 600});
        if matches!(endpoint, TingEndpoint::Register) {
            request["subject_token"] = json!(context.token.expose_secret());
        }
        let value = self.response(self.request(path)?.json(&request)).await?;
        if value["kind"] != kind
            || value["issuing_app"] != self.app_id
            || value["receiving_app"] != "ting"
            || !value["scopes"]
                .as_array()
                .is_some_and(|scopes| scopes.iter().any(|scope| scope == endpoint.id()))
            || matches!(endpoint, TingEndpoint::Register)
                && value.pointer("/user/uuid").and_then(Value::as_str)
                    != Some(context.actor.uuid.as_str())
        {
            return Err(AppError::dependency("accounts"));
        }
        Ok(TingAccess {
            actor: context.actor.clone(),
            token: SecretString::from(required(&value, "proof_token")?),
        })
    }
}
pub fn discovery(settings: &Settings) -> Value {
    json!({"app_id": settings.app_id, "owner": "si:tos", "accounts_url": settings.accounts_url.as_str(), "apps_url": "https://apps.teamofsilicons.com", "developers_url": "https://developers.teamofsilicons.com", "login_method": "short_lived_token"})
}
