//! Wire tests for the client core: platform headers and error mapping.
//!
//! `fixtures/error_auth_002.json` is the 401 body captured live on
//! 2026-10-15 (docs/qubo-vod-verification.md).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::auth::AccountProfile;
use qubo::client::QuboClient;
use qubo::error::QuboError;
use qubo::session::Session;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";

fn session() -> Session {
    Session {
        access_token: "token-abc".into(),
        uuid: "user-uuid-1".into(),
        email: None,
        refresh_token: None,
    }
}

fn client_for(server: &MockServer) -> QuboClient {
    let base = server
        .uri()
        .parse()
        .expect("mock server uri parses as a URL");
    QuboClient::new(base, SP.into())
}

#[tokio::test]
async fn authenticated_requests_carry_the_platform_headers() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(UNITS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let body = client
        .get_json(UNITS_PATH, Some(&session()))
        .await
        .expect("mock answers 200");
    assert_eq!(body, json!([]), "the response body is passed through");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "one request reached the mock");
    let headers = &requests[0].headers;
    let expect_header = |name: &str, value: &str| {
        assert_eq!(
            headers.get(name).map(|value| value.to_str().unwrap()),
            Some(value),
            "header {name}"
        );
    };
    expect_header("Authorization", "Bearer token-abc");
    expect_header("Subscriber-Key", "token-abc");
    expect_header("User-UUID", "user-uuid-1");
    expect_header("Token-Type", "USER");
    expect_header("Source", "Android");
    expect_header("App-Id", "com.hero.iot");
    expect_header("Source-Device-Id", client.device_id());
    expect_header("Content-Type", "application/json");
}

#[tokio::test]
async fn a_401_with_the_api_envelope_maps_to_auth() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(UNITS_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = client
        .get_json(UNITS_PATH, Some(&session()))
        .await
        .expect_err("401 is an error");

    match err {
        QuboError::Auth { code, message } => {
            assert_eq!(
                code.as_deref(),
                Some("ERR_AUTH_002"),
                "the envelope's code is preserved"
            );
            assert_eq!(
                message, "User Token expired, Access denied",
                "an array message is flattened"
            );
        }
        other => panic!("expected Auth, got {other:?}"),
    }
}

#[tokio::test]
async fn malformed_json_maps_to_parse_without_panicking() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(UNITS_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string("<html>not json</html>"))
        .mount(&server)
        .await;

    let client = client_for(&server);
    let err = client
        .get_json(UNITS_PATH, Some(&session()))
        .await
        .expect_err("unparseable body is an error");

    assert!(
        matches!(err, QuboError::Parse(_)),
        "expected Parse, got {err:?}"
    );
}

fn profile(uuid: &str) -> AccountProfile {
    AccountProfile {
        access_token: format!("token-{uuid}"),
        uuid: uuid.to_owned(),
        name: None,
        email: Some(format!("{uuid}@example.com")),
        refresh_token: None,
    }
}

#[test]
fn a_pick_is_claimed_once_and_released_by_its_guard() {
    let base = "http://127.0.0.1:9/"
        .parse()
        .expect("a URL to carry, never to call");
    let client = QuboClient::new(base, SP.into());
    client.set_pending_profiles(vec![profile("u-1"), profile("u-2")]);

    let picked = client
        .claim_pending_profile("u-1")
        .expect("the first pick claims");
    assert_eq!(picked.profile().uuid, "u-1", "the pick resolves by uuid");

    let racing = client
        .claim_pending_profile("u-2")
        .expect_err("a claimed slot refuses the racing pick");
    match racing {
        QuboError::Auth { message, .. } => assert!(
            message.contains("in flight"),
            "the refusal says a mint is already running: {message}"
        ),
        other => panic!("expected Auth, got {other:?}"),
    }

    drop(picked); // the mint failed, or the request was abandoned
    let retry = client
        .claim_pending_profile("u-2")
        .expect("the guard's drop opens the slot again");
    assert_eq!(
        retry.profile().uuid,
        "u-2",
        "the retry takes the other account"
    );
}

#[test]
fn a_stale_pick_never_releases_a_newer_picker() {
    let base = "http://127.0.0.1:9/"
        .parse()
        .expect("a URL to carry, never to call");
    let client = QuboClient::new(base, SP.into());
    client.set_pending_profiles(vec![profile("u-1"), profile("u-2")]);
    let slow = client
        .claim_pending_profile("u-1")
        .expect("the first pick claims");

    // A newer verify replaces the picker while the first mint is out.
    client.set_pending_profiles(vec![profile("u-3"), profile("u-4")]);
    let newer = client
        .claim_pending_profile("u-3")
        .expect("the newer picker claims");

    drop(slow); // the slow mint finally fails
    let held = client
        .claim_pending_profile("u-4")
        .expect_err("the newer claim still holds");
    match held {
        QuboError::Auth { message, .. } => assert!(
            message.contains("in flight"),
            "the newer claim is untouched by the stale release: {message}"
        ),
        other => panic!("expected Auth, got {other:?}"),
    }

    drop(newer);
    let retry = client
        .claim_pending_profile("u-4")
        .expect("the newer guard's drop opens its own slot");
    assert_eq!(retry.profile().uuid, "u-4", "the newer slot reopens");
}
