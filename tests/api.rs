//! Backend HTTP contract tests (real router, fake IAM and Ting; the real IAM adapter against a
//! stand-in IAM server for how IAM refusals map).

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Body;
use axum::http::{HeaderMap, Request, StatusCode};
use axum::response::IntoResponse as _;
use http_body_util::BodyExt as _;
use secrecy::SecretString;
use serde_json::{Value, json};
use silicon_spotify::dev::{FakeIam, FakeTing};
use silicon_spotify::identity::{Iam, Identity as _};
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
    let (status, _, value) = send(app, request.body(body).expect("request")).await;
    (status, value)
}

/// Sends a prepared request; every response must carry a request id and a JSON (or empty) body.
async fn send(app: &App, request: Request<Body>) -> (StatusCode, HeaderMap, Value) {
    let response = app.router.clone().oneshot(request).await.expect("response");
    let status = response.status();
    let headers = response.headers().clone();
    assert!(
        headers.contains_key("x-request-id"),
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
    if status.is_client_error() || status.is_server_error() {
        assert!(
            !value["error"]["code"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{status}: errors are the JSON envelope: {value}"
        );
        assert!(
            !value["error"]["hint"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{status}: every error has a hint: {value}"
        );
    }
    (status, headers, value)
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
async fn login_never_registers_or_requests_feature_consent() {
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
    assert_eq!(session["ting"]["subscribed"], false);
    assert_eq!(
        app.ting.recipients.lock().expect("lock").as_slice(),
        Vec::<String>::new()
    );
    let (status, value) = call(
        &app,
        "POST",
        "/api/v1/auth/login",
        &[("idempotency-key", KEY)],
        Some(json!({"slt": "garbage"})),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "401 keeps older CLIs working"
    );
    assert_eq!(value["error"]["code"], "slt_rejected");
    assert!(
        value["error"]["hint"]
            .as_str()
            .expect("hint")
            .contains("iam silicon-login --app-id spotify"),
        "{value}"
    );
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
    // Revoking a token nobody recognises is a success (RFC 7009), whatever its shape.
    for token in ["ort_garbage_qa", "oat_garbage_qa", "garbage", ""] {
        let (status, value) = call(
            &app,
            "POST",
            "/api/v1/auth/logout",
            &[("idempotency-key", KEY)],
            Some(json!({"token": token})),
        )
        .await;
        assert_eq!(status, StatusCode::NO_CONTENT, "{token}: {value}");
    }
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
    assert_eq!(value["feature_consent"], "separate");
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
    // A bearer that cannot be an access token is refused before the organization is asked for.
    for bad in [
        "Bearer garbage",
        "Bearer oat_",
        "Bearer oat_a b",
        "Basic abc",
    ] {
        let (status, value) = call(
            &app,
            "GET",
            "/api/v1/auth/me",
            &[("authorization", bad)],
            None,
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{bad}: {value}");
        assert_eq!(value["error"]["code"], "unauthenticated");
    }
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
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/ting/subscription",
        &[
            ("authorization", bearer.as_str()),
            ("x-org-id", "tos"),
            ("idempotency-key", KEY),
        ],
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
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
async fn missing_feature_consent_does_not_fail_login() {
    let app = app();
    app.iam.denied_ting.store(true, Ordering::SeqCst);
    let session = login(&app, "si:erin").await;
    assert_eq!(session["ting"]["subscribed"], false);
    assert!(session["ting"]["error"].is_null());
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

#[tokio::test]
async fn telemetry_batches_over_64_kib_are_refused_not_dropped() {
    let app = app();
    let big = "x".repeat(70 * 1024);
    let body = json!({"table": "spotifyclidaemon", "events": [{"id": "1", "note": big}]});
    let (status, value) = call(&app, "POST", "/api/v1/telemetry", &[], Some(body.clone())).await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{value}");
    assert_eq!(value["error"]["code"], "payload_too_large");
    assert!(
        value["error"]["hint"]
            .as_str()
            .expect("hint")
            .contains("smaller batches")
    );
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/telemetry",
        &[("x-spotify-telemetry", "off")],
        Some(body),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "opted-out callers are still skipped"
    );
    // The website can read the refusal: it carries the same CORS headers as a 204.
    let web = json!({"table": "spotifyfrontendevents", "events": [{"id": "1", "note": "x".repeat(70 * 1024)}]});
    let (status, headers, _) = send(
        &app,
        Request::builder()
            .method("POST")
            .uri("/api/v1/telemetry")
            .header("content-type", "application/json")
            .header("origin", "http://localhost:4321")
            .body(Body::from(web.to_string()))
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
    assert_eq!(
        headers
            .get("access-control-allow-origin")
            .expect("cors on refusals"),
        "http://localhost:4321"
    );
    // Other refusals name a next step too (`send` asserts every error has a hint).
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/telemetry",
        &[],
        Some(json!({"table": "spotifyclidaemon", "events": []})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _) = call(
        &app,
        "POST",
        "/api/v1/auth/logout",
        &[("idempotency-key", "short")],
        Some(json!({"token": "ort_x"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn framework_errors_use_the_json_envelope() {
    let app = app();
    // 405 keeps `Allow` and says which method to use.
    let (status, headers, value) = send(
        &app,
        Request::builder()
            .method("GET")
            .uri("/api/v1/auth/login")
            .body(Body::empty())
            .expect("request"),
    )
    .await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(value["error"]["code"], "method_not_allowed");
    assert!(
        value["error"]["hint"]
            .as_str()
            .expect("hint")
            .contains("POST")
    );
    assert_eq!(headers.get("allow").expect("allow"), "POST");
    assert_eq!(
        headers.get("content-type").expect("content type"),
        "application/json"
    );
    // 413 from the 512 KiB body limit, declared up front or discovered while reading.
    let big = vec![b'a'; 2 * 1024 * 1024];
    for declared in [true, false] {
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/v1/reports")
            .header("content-type", "application/json")
            .header("idempotency-key", KEY);
        if declared {
            request = request.header("content-length", big.len());
        }
        let (status, _, value) = send(
            &app,
            request.body(Body::from(big.clone())).expect("request"),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "declared={declared}");
        assert_eq!(value["error"]["code"], "payload_too_large");
        assert_eq!(value["error"]["retryable"], false);
    }
    let (status, value) = call(&app, "DELETE", "/api/v1/nope", &[], None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(value["error"]["code"], "not_found");
}

/// The real IAM adapter against a stand-in IAM that answers every call with one fixed response.
struct StandIn {
    iam: Iam,
    calls: Arc<AtomicUsize>,
}

async fn stand_in(status: u16, code: &str) -> StandIn {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let code = code.to_owned();
    let router = axum::Router::new().fallback(move || {
        counter.fetch_add(1, Ordering::SeqCst);
        let code = code.clone();
        async move {
            let status = StatusCode::from_u16(status).expect("status");
            if status.is_success() {
                return status.into_response();
            }
            (
                status,
                axum::Json(json!({"error": {"code": code, "message": "refused by the stand-in"}})),
            )
                .into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let address = listener.local_addr().expect("address");
    tokio::spawn(async move { axum::serve(listener, router).await });
    let mut settings = silicon_spotify::dev::settings();
    settings.iam_url = url::Url::parse(&format!("http://{address}")).expect("url");
    StandIn {
        iam: Iam::new(&settings).expect("adapter"),
        calls,
    }
}

const REFRESH: &str = "ort_0123456789abcdef0123456789abcdef";

#[tokio::test]
async fn logout_treats_unrecognised_tokens_as_revoked() {
    let token = SecretString::from(REFRESH);
    for (status, code) in [
        (200, ""),
        (400, "invalid_request"),
        (400, "invalid_grant"),
        (401, "token_revoked"),
        (410, "token_expired"),
        (422, "validation_failed"),
    ] {
        let iam = stand_in(status, code).await;
        let result = iam.iam.logout(&token, KEY).await;
        assert!(result.is_ok(), "{status} {code}: {result:?}");
        assert_eq!(iam.calls.load(Ordering::SeqCst), 1);
    }
    // Problems that are not about the token stay errors.
    for (status, code, expected, http) in [
        (401, "invalid_client", "dependency_unavailable", 503),
        (400, "invalid_client", "dependency_unavailable", 503),
        (409, "idempotency_conflict", "conflict", 409),
        (503, "service_unavailable", "dependency_unavailable", 503),
    ] {
        let iam = stand_in(status, code).await;
        let error = iam
            .iam
            .logout(&token, KEY)
            .await
            .expect_err("still an error");
        assert_eq!(error.code, expected, "{status} {code}");
        assert_eq!(error.status.as_u16(), http, "{status} {code}");
    }
    // A token IAM never issues is not sent to IAM at all.
    let iam = stand_in(500, "boom").await;
    for token in ["garbage", "ort_", "ort_has space", ""] {
        let result = iam.iam.logout(&SecretString::from(token), KEY).await;
        assert!(result.is_ok(), "{token}: {result:?}");
    }
    assert_eq!(iam.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn refused_exchanges_name_the_token() {
    // Login: any refusal of the SLT itself names the SLT. Refresh: only an explicit refusal of the
    // refresh token is a 401 (clients delete the session on 401); a malformed-request 400 or 422
    // is not, so a request-shape problem on IAM's side never wipes saved sessions.
    for (status, code, refresh) in [
        (400, "invalid_grant", "unauthenticated"),
        (400, "invalid_request", "invalid_input"),
        (401, "unauthenticated", "unauthenticated"),
        (401, "refresh_token_reuse", "unauthenticated"),
        (410, "slt_expired", "unauthenticated"),
        (422, "validation_failed", "invalid_input"),
    ] {
        let iam = stand_in(status, code).await;
        let error = iam
            .iam
            .login(&SecretString::from("oac_0123456789abcdef"), KEY)
            .await
            .expect_err("refused");
        assert_eq!(error.code, "slt_rejected", "{status} {code}");
        assert_eq!(error.status, StatusCode::UNAUTHORIZED);
        let error = iam
            .iam
            .refresh(&SecretString::from(REFRESH), KEY)
            .await
            .expect_err("refused");
        assert_eq!(error.code, refresh, "{status} {code}");
        assert_eq!(
            error.status == StatusCode::UNAUTHORIZED,
            refresh == "unauthenticated",
            "{status} {code}"
        );
    }
    // An idempotency conflict is a conflict whatever status carries it, never a token refusal.
    for status in [409, 422] {
        let iam = stand_in(status, "idempotency_conflict").await;
        let login = iam
            .iam
            .login(&SecretString::from("oac_0123456789abcdef"), KEY)
            .await
            .expect_err("conflict");
        let refresh = iam
            .iam
            .refresh(&SecretString::from(REFRESH), KEY)
            .await
            .expect_err("conflict");
        let logout = iam
            .iam
            .logout(&SecretString::from(REFRESH), KEY)
            .await
            .expect_err("conflict");
        for error in [login, refresh, logout] {
            assert_eq!(error.code, "conflict", "{status}");
            assert_eq!(error.status, StatusCode::CONFLICT);
        }
    }
    let iam = stand_in(401, "invalid_client").await;
    let error = iam
        .iam
        .login(&SecretString::from("oac_0123456789abcdef"), KEY)
        .await
        .expect_err("refused");
    assert_eq!(
        error.code, "dependency_unavailable",
        "a rejected app secret is the backend's problem, not the SLT's"
    );
    let iam = stand_in(429, "rate_limited").await;
    let error = iam
        .iam
        .login(&SecretString::from("oac_0123456789abcdef"), KEY)
        .await
        .expect_err("refused");
    assert_eq!(error.code, "rate_limited");
    let iam = stand_in(418, "teapot").await;
    let error = iam
        .iam
        .refresh(&SecretString::from(REFRESH), KEY)
        .await
        .expect_err("refused");
    assert_eq!(error.code, "dependency_unavailable");
}
