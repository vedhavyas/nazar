//! Stream mint wire tests.
//!
//! Fixture provenance (spec §6): the mint request shape (path, query
//! parameters, `userAppID` body) is ported from qubopy and confirmed by the
//! app's own path constants; `mint_live.json` is synthesized - the captured
//! live mint (2026-10-13) proved the `rtsps://` URL but its raw response was
//! never recorded, so the `streamURL` key is recorded and the `sessionId` key
//! is assumed.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::client::QuboClient;
use qubo::devices::Device;
use qubo::error::QuboError;
use qubo::session::Session;
use qubo::stream::{Quality, mint_live};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";

fn client_for(server: &MockServer) -> QuboClient {
    let base = server
        .uri()
        .parse()
        .expect("mock server uri parses as a URL");
    QuboClient::new(base, SP.into())
}

fn session() -> Session {
    Session {
        access_token: "token-abc".into(),
        uuid: "user-uuid-1".into(),
        email: None,
        refresh_token: None,
    }
}

fn device(device_type: &str) -> Device {
    device_with(DEVICE_ID, device_type)
}

fn device_with(device_id: &str, device_type: &str) -> Device {
    Device {
        device_id: device_id.into(),
        name: "Hall".into(),
        device_type: device_type.into(),
        // The mint carries the type; the model is not part of its wire
        // shape, so the helper leaves it unset.
        model: String::new(),
        unit_id: UNIT.into(),
        entity_id: "default".into(),
        controller_id: None,
    }
}

fn fixture(source: &str) -> Value {
    serde_json::from_str(source).expect("fixture is JSON")
}

async fn mount_mint(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
        .mount(server)
        .await;
}

fn query_of(request: &wiremock::Request) -> HashMap<String, String> {
    request
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect()
}

#[tokio::test]
async fn mint_live_returns_the_url_and_asks_with_the_documented_parameters() {
    let server = MockServer::start().await;
    mount_mint(&server, 200, include_str!("fixtures/mint_live.json")).await;

    let client = client_for(&server);
    let mint = mint_live(&client, &session(), &device("HCP01"), Quality::High)
        .await
        .expect("the mint succeeds");

    let payload = fixture(include_str!("fixtures/mint_live.json"));
    assert_eq!(
        mint.url, payload["streamURL"],
        "the URL comes from streamURL"
    );
    assert_eq!(
        mint.session_id, payload["sessionId"],
        "the session id comes from sessionId"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let query = query_of(&requests[0]);
    assert_eq!(
        query.get("timestamp").map(String::as_str),
        Some("NOW"),
        "timestamp=NOW is sent"
    );
    assert_eq!(
        query.get("type").map(String::as_str),
        Some("high"),
        "type carries the quality"
    );
    assert_eq!(
        query.get("streamSourceCamera").map(String::as_str),
        Some("primary"),
        "the primary camera source is requested"
    );
    assert_eq!(
        query.get("deviceType").map(String::as_str),
        Some("HCP01"),
        "the device's own type is sent"
    );

    let body = fixture(&String::from_utf8_lossy(&requests[0].body));
    assert_eq!(
        body["userAppID"], DEVICE_ID,
        "the camera's own id is sent as userAppID: the gateway names the wowza stream after it"
    );
}

/// Two cameras must never mint under one source id: the gateway names the
/// wowza stream after `userAppID`, so a shared id gives every camera one
/// shared picture (the hub's all-tiles-one-camera defect, 2026-10-16).
#[tokio::test]
async fn two_cameras_mint_with_distinct_source_ids() {
    const SECOND_ID: &str = "c0000000-0000-4000-8000-000000000002";
    let server = MockServer::start().await;
    mount_mint(&server, 200, include_str!("fixtures/mint_live.json")).await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH.replace(DEVICE_ID, SECOND_ID)))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/mint_live.json")),
        )
        .mount(&server)
        .await;

    let client = client_for(&server);
    mint_live(
        &client,
        &session(),
        &device_with(DEVICE_ID, "HCP01"),
        Quality::High,
    )
    .await
    .expect("the first camera mints");
    mint_live(
        &client,
        &session(),
        &device_with(SECOND_ID, "HCP01"),
        Quality::High,
    )
    .await
    .expect("the second camera mints");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 2, "two mints went out");
    let sources: Vec<String> = requests
        .iter()
        .map(|request| {
            fixture(&String::from_utf8_lossy(&request.body))["userAppID"]
                .as_str()
                .expect("a string userAppID")
                .to_owned()
        })
        .collect();
    assert_eq!(
        sources,
        vec![DEVICE_ID.to_owned(), SECOND_ID.to_owned()],
        "each mint names its own camera, so the two wowza streams cannot collide"
    );
}

#[tokio::test]
async fn mint_live_defaults_the_device_type() {
    let server = MockServer::start().await;
    mount_mint(&server, 200, include_str!("fixtures/mint_live.json")).await;

    let client = client_for(&server);
    mint_live(&client, &session(), &device(""), Quality::Low)
        .await
        .expect("the mint succeeds");

    let requests = server.received_requests().await.expect("recorded requests");
    let query = query_of(&requests[0]);
    assert_eq!(
        query.get("deviceType").map(String::as_str),
        Some("ptzeCamera"),
        "an untyped device mints with the default deviceType"
    );
    assert_eq!(
        query.get("type").map(String::as_str),
        Some("low"),
        "type carries the requested quality"
    );
}

#[tokio::test]
async fn mint_live_maps_a_server_error_to_http() {
    let server = MockServer::start().await;
    mount_mint(&server, 500, "").await;

    let client = client_for(&server);
    let err = mint_live(&client, &session(), &device("HCP01"), Quality::High)
        .await
        .expect_err("a 500 is an error");

    match err {
        QuboError::Http { status, .. } => {
            assert_eq!(status, Some(500), "the response status is kept");
        }
        other => panic!("expected Http, got {other:?}"),
    }
}

#[tokio::test]
async fn mint_live_maps_a_rejection_body_to_api() {
    let server = MockServer::start().await;
    mount_mint(
        &server,
        400,
        include_str!("fixtures/error_mint_rejected.json"),
    )
    .await;

    let client = client_for(&server);
    let err = mint_live(&client, &session(), &device("HCP01"), Quality::High)
        .await
        .expect_err("a rejected mint is an error");

    let payload = fixture(include_str!("fixtures/error_mint_rejected.json"));
    match err {
        QuboError::Api { code, message } => {
            assert_eq!(code, payload["code"], "the API code is preserved");
            assert_eq!(
                message, payload["message"][0],
                "the server's message surfaces verbatim"
            );
        }
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn a_mint_without_a_stream_url_is_a_parse_error() {
    let server = MockServer::start().await;
    mount_mint(&server, 200, "{}").await;

    let client = client_for(&server);
    let err = mint_live(&client, &session(), &device("HCP01"), Quality::High)
        .await
        .expect_err("a mint with no URL is an error");

    assert!(
        matches!(err, QuboError::Parse(_)),
        "expected Parse, got {err:?}"
    );
}
