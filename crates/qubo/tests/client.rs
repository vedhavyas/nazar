//! Wire tests for the client core: platform headers and error mapping.
//!
//! `fixtures/error_auth_002.json` is the 401 body captured live on
//! 2026-10-15 (docs/qubo-vod-verification.md).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

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
