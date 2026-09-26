//! Backend HTTP contract tests (real router, fake IAM and Ting).

use std::sync::Arc;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt as _;
use serde_json::{Value, json};
use silicon_spotify::dev::{FakeIam, FakeTing};
use tower::ServiceExt as _;

struct App {
    router: axum::Router,
    ting: Arc<FakeTing>,
    iam: Arc<FakeIam>,
}

fn app() -> App {
    let ting = Arc::new(FakeTing::default());
    let iam = Arc::new(FakeIam::default());
    let state = silicon_spotify::dev::state(Arc::clone(&ting), Arc::clone(&iam)).expect("state");
    App {
        router: silicon_spotify::api::router(state),
        ting,
        iam,
    }
}

async fn call(
    app: &App,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder().method(method).uri(path);
    for (name, value) in headers {
        request = request.header(*name, *value);
    }
    let body = match body {
        Some(value) => {
            request = request.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .router
        .clone()
        .oneshot(request.body(body).expect("request"))
        .await
        .expect("response");
    let status = response.status();
    assert!(
        response.headers().contains_key("x-request-id"),
        "every response carries a request id"
    );
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("body")
        .to_bytes();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).expect("json body")
    };
    (status, value)
}

const KEY: &str = "spotify-test-key-0000000001";

async fn login(app: &App, slt: &str) -> Value {
    let (status, value) = call(
        app,
        "POST",
        "/api/v1/auth/login",
        &[("idempotency-key", KEY)],
        Some(json!({"slt": slt})),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{value}");
    value
}

#[tokio::test]
async fn health_and_discovery() {
    let app = app();
    let (status, value) = call(&app, "GET", "/healthz", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["status"], "ok");
    let (status, value) = call(&app, "GET", "/api/v1/iam", &[], None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["app_id"], "spotify");
    assert_eq!(value["ting_types"][0], "spotify.trigger.fired");
    let (status, value) = call(&app, "GET", "/nope", &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(value["error"]["code"], "not_found");
}

#[tokio::test]
async fn login_registers_the_ting_recipient() {
    let app = app();
    let (status, value) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[],
        Some(json!({"slt": "oac_x"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "idempotency key is required"
    );
    assert_eq!(value["error"]["code"], "invalid_input");
    let session = login(&app, "si:alice").await;
    assert_eq!(session["actor"]["public_id"], "si:alice");
    assert_eq!(session["org_id"], "tos");
    assert!(
        session["access_token"]
            .as_str()
            .expect("token")
            .starts_with("oat_")
    );
    assert_eq!(session["ting"]["subscribed"], true);
    assert_eq!(
        app.ting.recipients.lock().expect("lock").as_slice(),
        ["si:alice"]
    );
    let (status, value) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[("idempotency-key", KEY)],
        Some(json!({"slt": "garbage"})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(value["error"]["code"], "unauthenticated");
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[("idempotency-key", KEY)],
        Some(json!({"slt": "oac_x", "extra": 1})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "unknown fields are rejected"
    );
}

#[tokio::test]
async fn refresh_rotates_and_logout_revokes() {
    let app = app();
    let session = login(&app, "oac_abc").await;
    let old = session["refresh_token"]
        .as_str()
        .expect("refresh")
        .to_owned();
    let (status, rotated) = call(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        &[("idempotency-key", KEY)],
        Some(json!({"refresh_token": old})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_ne!(rotated["refresh_token"], session["refresh_token"]);
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        &[("idempotency-key", KEY)],
        Some(json!({"refresh_token": old})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "a consumed refresh token is refused"
    );
    let token = rotated["refresh_token"].as_str().expect("r").to_owned();
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/auth/logout",
        &[("idempotency-key", KEY)],
        Some(json!({"token": token})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/auth/refresh",
        &[("idempotency-key", KEY)],
        Some(json!({"refresh_token": token})),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn me_checks_bearer_and_org() {
    let app = app();
    let session = login(&app, "si:bob").await;
    let bearer = format!("Bearer {}", session["access_token"].as_str().expect("t"));
    let (status, value) = call(
        &app,
        "GET",
        "/api/v1/auth/me",
        &[("authorization", &bearer), ("x-org-id", "tos")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(value["authenticated"], true);
    assert_eq!(value["ting_ready"], true);
    let (status, value) = call(
        &app,
        "GET",
        "/api/v1/auth/me",
        &[("authorization", &bearer)],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        value["error"]["message"]
            .as_str()
            .expect("m")
            .contains("X-Org-ID")
    );
    let (status, _) = call(
        &app,
        "GET",
        "/api/v1/auth/me",
        &[("authorization", "Bearer oat_nope"), ("x-org-id", "tos")],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn tings_are_sent_for_the_verified_actor_only() {
    let app = app();
    let session = login(&app, "si:carol").await;
    let bearer = format!("Bearer {}", session["access_token"].as_str().expect("t"));
    let headers = [("authorization", bearer.as_str()), ("x-org-id", "tos")];
    let body = json!({"type": "spotify.trigger.fired", "key": "si:carol/trg_1/1/fired", "data": {"trigger": {"id": "trg_1"}}, "metadata": {"isi": "planner"}});
    let (status, accepted) =
        call(&app, "POST", "/api/v1/tings", &headers, Some(body.clone())).await;
    assert_eq!(status, StatusCode::OK, "{accepted}");
    assert!(accepted["id"].as_str().expect("id").starts_with("msg_"));
    assert_eq!(accepted["replayed"], false);
    let sent = app.ting.sent.lock().expect("lock").clone();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0]["for"], "si:carol",
        "recipient comes from the session"
    );
    assert_eq!(sent[0]["org_id"], "tos");
    assert_eq!(sent[0]["metadata"]["isi"], "planner");
    let (_, replay) = call(&app, "POST", "/api/v1/tings", &headers, Some(body)).await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(replay["id"], accepted["id"]);
    // Someone else's key prefix, unknown type, or a smuggled `for` are refused.
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/tings",
        &headers,
        Some(json!({"type": "spotify.trigger.fired", "key": "si:mallory/x", "data": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/tings",
        &headers,
        Some(json!({"type": "spotify.other.thing", "key": "si:carol/x", "data": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(&app, "POST", "/api/v1/tings", &headers, Some(json!({"type": "spotify.trigger.fired", "key": "si:carol/x", "data": {}, "for": "si:mallory"}))).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn ting_refusals_pass_through_with_hints() {
    let app = app();
    let session = login(&app, "si:dan").await;
    let bearer = format!("Bearer {}", session["access_token"].as_str().expect("t"));
    *app.ting.refuse.lock().expect("lock") = Some("recipient_not_registered".into());
    let (status, value) = call(
        &app,
        "POST",
        "/api/v1/tings",
        &[("authorization", &bearer), ("x-org-id", "tos")],
        Some(json!({"type": "spotify.trigger.fired", "key": "si:dan/k", "data": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(value["error"]["code"], "recipient_not_registered");
    assert!(
        value["error"]["hint"]
            .as_str()
            .expect("hint")
            .contains("spotify ting register")
    );
}

#[tokio::test]
async fn sessions_without_ting_scopes_need_reconsent() {
    let app = app();
    *app.iam.scopes.lock().expect("lock") = Some("self.identity.read".into());
    let session = login(&app, "si:erin").await;
    assert_eq!(session["ting"]["subscribed"], false);
    assert_eq!(session["ting"]["error"]["code"], "reconsent_required");
    let bearer = format!("Bearer {}", session["access_token"].as_str().expect("t"));
    let (status, value) = call(
        &app,
        "POST",
        "/api/v1/tings",
        &[("authorization", &bearer), ("x-org-id", "tos")],
        Some(json!({"type": "spotify.trigger.fired", "key": "si:erin/k", "data": {}})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(value["error"]["code"], "reconsent_required");
}

#[tokio::test]
async fn reports_are_stored_idempotently_and_validated() {
    let app = app();
    let body = json!({"message": "seek drifts two seconds on podcasts", "pr": "https://github.com/unlikefraction/spotify-cli/pull/42"});
    let (status, first) = call(
        &app,
        "POST",
        "/api/v1/reports",
        &[("idempotency-key", KEY)],
        Some(body.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{first}");
    assert_eq!(first["status"], "stored");
    let (_, second) = call(
        &app,
        "POST",
        "/api/v1/reports",
        &[("idempotency-key", KEY)],
        Some(body),
    )
    .await;
    assert_eq!(first["id"], second["id"]);
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/reports",
        &[("idempotency-key", "spotify-test-key-0000000002")],
        Some(json!({"message": "some bug text here", "pr": "https://example.com/pull/1"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn telemetry_gateway_validates_tables() {
    let app = app();
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/telemetry",
        &[],
        Some(json!({"table": "spotifybackend", "events": [{}]})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::BAD_REQUEST,
        "the backend table is not relayable"
    );
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/telemetry",
        &[],
        Some(json!({"table": "spotifyclidaemon", "events": [{"id": "1"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/telemetry",
        &[("x-spotify-telemetry", "off")],
        Some(json!({"table": "nope", "events": []})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "opted-out callers are skipped before validation"
    );
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/telemetry",
        &[("origin", "https://evil.example")],
        Some(json!({"table": "spotifyfrontendevents", "events": [{"id": "1"}]})),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
