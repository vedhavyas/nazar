//! P2P signaling wire tests: the allocation routes, the device-access-token
//! preflight, and the SDK identity headers the signaling leg sends
//! (`docs/qubo-protocol.md` 2 and 5.2).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::QuboClient;
use qubo::p2p::signaling::{allocate_credentials, allocate_p2p, fetch_device_access_token};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const CAMERA: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const SELF: &str = "d0000000-0000-4000-8000-000000000002";
const CONTROLLER: &str = "d0000000-0000-4000-8000-000000000001";
const V3_PATH: &str = "/stream-manager/api/v3/sp/d10e4bfb0153496e8e8bb955f7ebe413/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/p2p";
const V2_PATH: &str = "/stream-manager/api/v2/sp/d10e4bfb0153496e8e8bb955f7ebe413/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices/pp2p/credentials";
const TOKEN_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/d0000000-0000-4000-8000-000000000002/device-access-token";

fn session() -> qubo::Session {
    qubo::Session {
        access_token: "token-abc".to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
    }
}

/// The signaling leg's client: the SDK's identity headers ride every call.
fn signaling_client(server: &MockServer) -> QuboClient {
    QuboClient::new(server.uri().parse().expect("a url"), SP.into()).with_headers(vec![
        (
            "Login-Device-Name".to_owned(),
            "MobileApp_<mobile-id>".to_owned(),
        ),
        ("User-Agent".to_owned(), "libcurl-agent".to_owned()),
    ])
}

fn request_body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).expect("a json body")
}

#[tokio::test]
async fn the_v3_allocation_carries_the_sdk_identity_headers() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(V3_PATH))
        .and(header("Login-Device-Name", "MobileApp_<mobile-id>"))
        .and(header("User-Agent", "libcurl-agent"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({
                    "appSessionId": "d0000000-0000-4000-8000-000000000014",
                    "aesKey": "82458EA37F4740361F47B4E68946546F",
                    "iv": "A3B6C1B75EEB65ACF59802CE5A6995B3",
                    "serverIP": "13.204.183.153",
                    "serverPort": 56329,
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let client = signaling_client(&server);
    let allocation = allocate_p2p(&client, &session(), UNIT, CAMERA, SELF, false)
        .await
        .expect("the allocation succeeds")
        .expect("an allocation body");

    assert_eq!(
        allocation.app_session_id, "d0000000-0000-4000-8000-000000000014",
        "the response's appSessionId is the session"
    );
    assert_eq!(
        allocation.aes_key, "82458EA37F4740361F47B4E68946546F",
        "the response's aesKey is kept verbatim"
    );
    assert_eq!(
        allocation.aes_iv, "A3B6C1B75EEB65ACF59802CE5A6995B3",
        "the response's iv alias is read"
    );
    assert_eq!(
        allocation.server_ip.as_deref(),
        Some("13.204.183.153"),
        "the relay endpoint is read"
    );
    assert_eq!(
        allocation.server_port,
        Some(56329),
        "the relay port is read"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let request = &requests[0];
    let login = request
        .headers
        .get("login-device-name")
        .and_then(|value| value.to_str().ok());
    assert_eq!(
        login,
        Some("MobileApp_<mobile-id>"),
        "Login-Device-Name rides the signaling call"
    );
    let query: Vec<(String, String)> = request
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert!(
        query.contains(&("isPTT".to_owned(), "false".to_owned())),
        "isPTT=false rides the query"
    );
    assert_eq!(
        request_body(request),
        json!({ "userAppID": SELF, "appSessionId": "", "isPTT": false }),
        "the request body is the reference's: an empty appSessionId goes out"
    );
}

#[tokio::test]
async fn the_v2_allocation_reads_the_wrapped_data_array() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(V2_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({
                    "data": [{
                        "appSessionId": "d0000000-0000-4000-8000-000000000014",
                        "aesKey": "82458EA37F4740361F47B4E68946546F",
                        "aesIV": "A3B6C1B75EEB65ACF59802CE5A6995B3",
                    }],
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;

    let client = signaling_client(&server);
    let allocation = allocate_credentials(&client, &session(), UNIT, CAMERA, SELF)
        .await
        .expect("the call succeeds")
        .expect("an allocation record");

    assert_eq!(
        allocation.app_session_id, "d0000000-0000-4000-8000-000000000014",
        "the record inside the data array is read"
    );
    assert_eq!(
        allocation.aes_iv, "A3B6C1B75EEB65ACF59802CE5A6995B3",
        "the v2 spelling aesIV is read"
    );
    assert_eq!(
        allocation.server_ip, None,
        "a record without a relay endpoint has none"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        request_body(&requests[0]),
        json!({ "userAppID": SELF, "devices": [CAMERA] }),
        "the v2 body names the camera in its devices list"
    );
}

#[tokio::test]
async fn the_device_token_preflight_reads_the_plausible_keys() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(json!({ "device_access_token": "tok" }).to_string()),
        )
        .mount(&server)
        .await;

    let client = signaling_client(&server);
    let token = fetch_device_access_token(&client, &session(), UNIT, SELF, CONTROLLER)
        .await
        .expect("no transport error");
    assert_eq!(
        token.as_deref(),
        Some("tok"),
        "the snake-case spelling is read"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        request_body(&requests[0]),
        json!({ "src": SELF, "dest": CONTROLLER }),
        "src is the self device and dest the controllerUUID"
    );

    // A body without any token key is None, not an error.
    let bare = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(TOKEN_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .mount(&bare)
        .await;
    let client = signaling_client(&bare);
    let token = fetch_device_access_token(&client, &session(), UNIT, SELF, CONTROLLER)
        .await
        .expect("no transport error");
    assert!(token.is_none(), "a body with no token key yields None");
}
