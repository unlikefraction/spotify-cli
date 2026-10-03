//! Real IAM SDK against a contract stand-in: separate consent, durable retries and context binding.
use axum::{
    Json, Router,
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode, Uri},
    response::{IntoResponse, Response},
};
use secrecy::{ExposeSecret as _, SecretString};
use serde_json::{Value, json};
use silicon_spotify::{
    config::Settings,
    identity::{Actor, AuthContext, Iam, Identity as _, TingEndpoint},
};
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

#[derive(Default)]
struct Mock {
    requests: Vec<(String, String, Value)>,
    authorization_failures: usize,
    exchange_failures: usize,
    refresh_failures: usize,
    token_error: Option<(u16, &'static str)>,
    short_lived: bool,
    malformed: Option<&'static str>,
    selected: Option<&'static str>,
    status: Option<&'static str>,
    login_org: Option<&'static str>,
    login_kind: Option<&'static str>,
    additional_org: bool,
}
const AUTH_ID: &str = "f0b73dad-0056-4a52-b151-d4086e4c1366";
fn future(seconds: i64) -> String {
    (time::OffsetDateTime::now_utc() + time::Duration::seconds(seconds))
        .format(&time::format_description::well_known::Rfc3339)
        .expect("timestamp")
}
fn consent(status: &str) -> Value {
    json!({"id":AUTH_ID,"app_id":"spotify","app_name":"Spotify","actor":{"type":"silicon","public_id":"si:caller"},"org_id":"callerorg","status":status,"version":1,"expires_at":future(3600),"endpoints":[],"authorization_url":"https://iam.teamofsilicons.com/obo/authorize"})
}
fn pair(endpoint: &str, mock: &Mock, refreshed: bool) -> Value {
    json!({"grant_id":if endpoint=="tings.send"{"a5d58f97-78ae-4262-81b8-01a250f36f8c"}else{"00b73dad-0056-4a52-b151-d4086e4c1366"},
        "access_token":if refreshed{"oba_rotated_abcdef"}else{"oba_original_abcdef"},"refresh_token":if refreshed{"obr_rotated_abcdef"}else{"obr_original_abcdef"},
        "token_type":"Bearer","expires_in":if mock.short_lived&&!refreshed{30}else{3600},"expires_at":future(if mock.short_lived&&!refreshed{30}else{3600}),
        "audience":if mock.malformed==Some("audience"){"unapproved"}else{"ting"},"endpoint_id":endpoint,"org_id":"providerorg",
        "actor":{"type":if mock.malformed==Some("actor"){"carbon"}else{"silicon"},"public_id":mock.selected.unwrap_or("si:provider")},"scope":format!("obo:ting:{endpoint}")})
}
fn refusal(status: u16, code: &str) -> Response {
    (
        StatusCode::from_u16(status).expect("status"),
        Json(json!({"error":{"code":code,"message":"mock refusal"}})),
    )
        .into_response()
}
async fn route(
    State(mock): State<Arc<Mutex<Mock>>>,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let path = uri.path().to_owned();
    let value: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let key = headers
        .get("idempotency-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_owned();
    let mut m = mock.lock().expect("mock");
    m.requests.push((path.clone(), key, value.clone()));
    if path.ends_with("/oauth/revoke") {
        return StatusCode::NO_CONTENT.into_response();
    }
    if path.ends_with("/app-auth/tokens") {
        return Json(json!({"access_token":"oat_login_abcdef","refresh_token":"ort_login_abcdef","token_type":"Bearer","expires_in":1800,"scope":"self.identity.read","actor":{"type":m.login_kind.unwrap_or("silicon"),"public_id":"si:caller"},"org_id":m.login_org})).into_response();
    }
    if path.ends_with("/oauth/introspect") {
        let snapshot = json!({"actor_type":"silicon","public_id":"si:caller","organization_id":AUTH_ID,"org_id":"callerorg","membership_id":"si:caller[callerorg]","membership_version":1,"authorization_epoch":1,"audience":"spotify","testing_environment_id":null,"scopes":["self.identity.read"],"org_role":null,"tags":null});
        let snapshots = if m.additional_org {
            vec![snapshot.clone(), {
                let mut other = snapshot.clone();
                other["org_id"] = json!("another");
                other
            }]
        } else {
            vec![snapshot.clone()]
        };
        return Json(json!({"active":true,"audience":"spotify","client_id":"spotify","org_id":"callerorg","public_id":"si:caller","membership_id":"si:caller[callerorg]","authorization":if m.additional_org{Value::Null}else{snapshot},"authorizations":snapshots})).into_response();
    }
    if path.ends_with("/authorizations") {
        assert!(headers.get("authorization").is_some());
        if m.authorization_failures > 0 {
            m.authorization_failures -= 1;
            return refusal(503, "unavailable");
        }
        return Json(consent("pending")).into_response();
    }
    if path.ends_with(AUTH_ID) {
        return Json(consent(m.status.unwrap_or("approved"))).into_response();
    }
    if path.ends_with("/tokens") {
        if let Some((status, code)) = m.token_error {
            return refusal(status, code);
        }
        let refresh = value["refresh_token"].is_string();
        if refresh && m.refresh_failures > 0 {
            m.refresh_failures -= 1;
            return refusal(503, "unavailable");
        }
        if !refresh && m.exchange_failures > 0 {
            m.exchange_failures -= 1;
            m.status = Some("exchanged");
            return refusal(503, "unavailable");
        }
        let items = if refresh {
            vec![pair("tings.send", &m, true)]
        } else {
            vec![
                pair("subscriptions.register", &m, false),
                pair(
                    if m.malformed == Some("duplicate") {
                        "subscriptions.register"
                    } else {
                        "tings.send"
                    },
                    &m,
                    false,
                ),
            ]
        };
        return Json(json!({"items":items})).into_response();
    }
    refusal(404, "unexpected_route")
}
struct Setup {
    mock: Arc<Mutex<Mock>>,
    settings: Settings,
    iam: Iam,
    _temp: tempfile::TempDir,
}
async fn setup() -> Setup {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let mock = Arc::new(Mutex::new(Mock::default()));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    let router = Router::new().fallback(route).with_state(Arc::clone(&mock));
    tokio::spawn(async move { axum::serve(listener, router).await.expect("serve") });
    let temp = tempfile::tempdir().expect("temp");
    let mut settings = silicon_spotify::dev::settings();
    settings.iam_url = url::Url::parse(&format!("http://{address}")).expect("url");
    settings.database_path = temp.path().join("state.sqlite");
    let iam = Iam::new(&settings).expect("iam");
    Setup {
        mock,
        settings,
        iam,
        _temp: temp,
    }
}
fn context() -> AuthContext {
    AuthContext {
        actor: Actor {
            kind: "silicon".into(),
            public_id: "si:caller".into(),
        },
        org_id: "callerorg".into(),
        membership_id: "si:caller[callerorg]".into(),
        session_id: Some(Uuid::new_v4()),
        scopes: BTreeSet::from(["self.identity.read".into()]),
        token: SecretString::from("oat_subject_abcdef"),
    }
}
async fn approved(s: &Setup, key: &str) -> String {
    let started = s.iam.ting_authorize(&context(), key).await.expect("start");
    let id = started["request_id"].as_str().expect("id").to_owned();
    s.iam
        .ting_complete(&context(), &id, "one-use-code")
        .await
        .expect("complete");
    id
}
const KEY: &str = "spotify-test-start-0001";

#[tokio::test]
async fn consent_is_independent_reusable_and_sealed_across_restart() {
    let s = setup().await;
    let ctx = context();
    assert_eq!(
        s.iam
            .ting_access(&ctx, TingEndpoint::Send, b"body", "operation")
            .await
            .err()
            .expect("missing")
            .code,
        "reconsent_required"
    );
    let id = approved(&s, KEY).await;
    let view = s
        .iam
        .ting_complete(&ctx, &id, "one-use-code")
        .await
        .expect("replay");
    assert!(view.to_string().find("oba_").is_none());
    let iam = Iam::new(&s.settings).expect("restart");
    for _ in 0..2 {
        let token = iam
            .ting_access(&ctx, TingEndpoint::Send, b"body", "operation")
            .await
            .expect("access");
        assert_eq!(token.token.expose_secret(), "oba_original_abcdef");
        assert_eq!(token.actor.public_id, "si:provider");
        assert_eq!(token.org_id, "providerorg");
    }
    iam.logout(
        &SecretString::from("ort_login_abcdef"),
        "spotify-test-logout-001",
    )
    .await
    .expect("logout");
    assert!(
        iam.ting_access(&ctx, TingEndpoint::Send, b"body", "operation")
            .await
            .is_ok(),
        "logout preserves durable consent"
    );
    assert_eq!(
        s.mock
            .lock()
            .expect("mock")
            .requests
            .iter()
            .filter(|(p, _, _)| p.ends_with("/tokens"))
            .count(),
        1
    );
    let conn = rusqlite::Connection::open(&s.settings.database_path).expect("db");
    let bytes: Vec<u8> = conn
        .query_row("SELECT credentials FROM obo_roots LIMIT 1", [], |r| {
            r.get(0)
        })
        .expect("ciphertext");
    assert!(!String::from_utf8_lossy(&bytes).contains("obr_"));
    let mut other = ctx.clone();
    other.org_id = "another".into();
    assert!(iam.ting_authorization(&other, &id).await.is_err());
    other = ctx.clone();
    other.actor.public_id = "si:other".into();
    assert!(
        iam.ting_access(&other, TingEndpoint::Send, b"body", "operation")
            .await
            .is_err()
    );
    assert!(
        iam.ting_access(&ctx, TingEndpoint::Send, b"changed", "operation")
            .await
            .is_err(),
        "payload cannot change under an operation key"
    );
}

#[tokio::test]
async fn uncertain_start_and_exchange_keep_exact_request_keys() {
    let s = setup().await;
    s.mock.lock().expect("mock").authorization_failures = 1;
    assert!(s.iam.ting_authorize(&context(), KEY).await.is_err());
    let iam = Iam::new(&s.settings).expect("restart");
    let mut fresh = context();
    fresh.token = SecretString::from("oat_fresh_session");
    let result = iam.ting_authorize(&fresh, KEY).await.expect("retry");
    let id = result["request_id"].as_str().expect("id");
    s.mock.lock().expect("mock").exchange_failures = 1;
    assert!(iam.ting_complete(&fresh, id, "one-use-code").await.is_err());
    let iam = Iam::new(&s.settings).expect("restart");
    assert!(
        iam.ting_complete(&fresh, id, "different-code")
            .await
            .is_err()
    );
    iam.ting_complete(&fresh, id, "one-use-code")
        .await
        .expect("code retry");
    let mock = s.mock.lock().expect("mock");
    let starts: Vec<_> = mock
        .requests
        .iter()
        .filter(|(p, _, _)| p.ends_with("/authorizations"))
        .collect();
    assert_eq!(starts.len(), 2);
    assert_eq!(starts[0].1, starts[1].1);
    assert_eq!(starts[0].2, starts[1].2);
    assert_eq!(starts[1].2["subject_token"], "oat_subject_abcdef");
    let exchanges: Vec<_> = mock
        .requests
        .iter()
        .filter(|(p, _, _)| p.ends_with("/tokens"))
        .collect();
    assert_eq!(exchanges.len(), 2);
    assert_eq!(exchanges[0].1, exchanges[1].1);
    assert_eq!(exchanges[0].2, exchanges[1].2);
}

#[tokio::test]
async fn refresh_is_durable_and_cannot_change_destination() {
    let s = setup().await;
    s.mock.lock().expect("mock").short_lived = true;
    approved(&s, KEY).await;
    s.mock.lock().expect("mock").refresh_failures = 1;
    assert!(
        s.iam
            .ting_access(&context(), TingEndpoint::Send, b"body", "operation")
            .await
            .is_err()
    );
    let iam = Iam::new(&s.settings).expect("restart");
    assert_eq!(
        iam.ting_access(&context(), TingEndpoint::Send, b"body", "operation")
            .await
            .expect("retry")
            .token
            .expose_secret(),
        "oba_rotated_abcdef"
    );
    {
        let mock = s.mock.lock().expect("mock");
        let refresh: Vec<_> = mock
            .requests
            .iter()
            .filter(|(_, _, v)| v["refresh_token"].is_string())
            .collect();
        assert_eq!(refresh.len(), 2);
        assert_eq!(refresh[0].1, refresh[1].1);
        assert_eq!(refresh[0].2, refresh[1].2);
    }
    s.mock.lock().expect("mock").selected = Some("si:changed");
    approved(&s, "spotify-test-start-0002").await;
    assert_eq!(
        s.iam
            .ting_access(&context(), TingEndpoint::Send, b"body", "operation")
            .await
            .err()
            .expect("bound")
            .code,
        "conflict"
    );
}

#[tokio::test]
async fn declined_changed_revoked_and_malformed_authority_preserve_login() {
    for malformed in ["audience", "actor", "duplicate"] {
        let s = setup().await;
        s.mock.lock().expect("mock").malformed = Some(malformed);
        let value = s.iam.ting_authorize(&context(), KEY).await.expect("start");
        assert!(
            s.iam
                .ting_complete(
                    &context(),
                    value["request_id"].as_str().expect("id"),
                    "code"
                )
                .await
                .is_err()
        );
        assert!(
            s.iam
                .ting_access(&context(), TingEndpoint::Send, b"body", "operation")
                .await
                .is_err()
        );
    }
    for (status, code, expected) in [(403, "obo_grant_revoked", 403), (412, "graph_changed", 412)] {
        let s = setup().await;
        s.mock.lock().expect("mock").short_lived = true;
        approved(&s, KEY).await;
        s.mock.lock().expect("mock").token_error = Some((status, code));
        let error = s
            .iam
            .ting_access(&context(), TingEndpoint::Send, b"body", "operation")
            .await
            .err()
            .expect("refused");
        assert_eq!(error.status.as_u16(), expected);
        assert_eq!(error.code, "reconsent_required");
    }
    let s = setup().await;
    let v = s.iam.ting_authorize(&context(), KEY).await.expect("start");
    s.mock.lock().expect("mock").status = Some("declined");
    assert_eq!(
        s.iam
            .ting_complete(&context(), v["request_id"].as_str().expect("id"), "code")
            .await
            .expect_err("declined")
            .status,
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn login_requires_exactly_one_org_and_matching_principal_kind() {
    for (org, extra, kind, ok) in [
        (None, false, None, false),
        (Some("other"), false, None, false),
        (Some("callerorg"), true, None, false),
        (Some("callerorg"), false, Some("carbon"), false),
        (Some("callerorg"), false, None, true),
    ] {
        let s = setup().await;
        {
            let mut mock = s.mock.lock().expect("mock");
            mock.login_org = org;
            mock.additional_org = extra;
            mock.login_kind = kind;
        }
        let result = s
            .iam
            .login(&SecretString::from("oac_login_abcdef"), KEY)
            .await;
        assert_eq!(
            result.is_ok(),
            ok,
            "org={org:?} extra={extra} kind={kind:?}: {result:?}"
        );
    }
}
