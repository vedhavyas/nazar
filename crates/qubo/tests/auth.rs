//! OTP login wire tests.
//!
//! Fixture provenance (spec §3.7). `otp_request.json` / `otp_verify.json`
//! carry the response shapes the reference client parses (qubopy `auth.py`)
//! and the stored session file's real shape; all values are synthesized.
//! `error_otp_001.json`'s code is the wrong-OTP code recorded in the APK
//! decompile (`Y4.java:1850`). `error_rate_limit.json`'s code was observed
//! live 2026-10-13 as the OTP resend rate limit (reported window: "try after
//! 15 minutes"). Both fixtures' message text is synthesized; the envelope
//! shape (`message` as an array) is live-captured. What these tests pin is
//! that whatever code and message the gateway sends survive into `QuboError`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::auth::{request_otp, verify_otp};
use qubo::client::{BASE_URL, QuboClient, ServiceProvider};
use qubo::error::QuboError;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const REQUEST_PATH: &str = "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/login";
const VERIFY_PATH: &str = "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/verify";

fn client_for(server: &MockServer) -> QuboClient {
    let base = server
        .uri()
        .parse()
        .expect("mock server uri parses as a URL");
    QuboClient::new(base, SP.into())
}

fn fixture(source: &str) -> Value {
    serde_json::from_str(source).expect("fixture is JSON")
}

async fn mount_json(server: &MockServer, route: &str, status: u16, body: &str) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
        .mount(server)
        .await;
}

fn login_uuid() -> String {
    fixture(include_str!("fixtures/otp_request.json"))["uuid"]
        .as_str()
        .expect("fixture carries the login session id")
        .to_owned()
}

#[tokio::test]
async fn request_otp_posts_the_phone_for_a_code() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        REQUEST_PATH,
        200,
        include_str!("fixtures/otp_request.json"),
    )
    .await;

    let client = client_for(&server);
    request_otp(&client, "9876543210")
        .await
        .expect("the gateway accepts the request");

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "one request reached the mock");
    let body = fixture(&String::from_utf8_lossy(&requests[0].body));
    assert_eq!(body["phone"], "9876543210", "the phone is sent");
    assert_eq!(body["isdCode"], "+91", "the ISD code is sent");
    assert_eq!(body["deviceAttribute"], "Nazar", "the device label is sent");
}

#[tokio::test]
async fn verify_otp_sends_the_pending_login_session_and_returns_the_session() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        REQUEST_PATH,
        200,
        include_str!("fixtures/otp_request.json"),
    )
    .await;
    mount_json(
        &server,
        VERIFY_PATH,
        200,
        include_str!("fixtures/otp_verify.json"),
    )
    .await;

    let client = client_for(&server);
    request_otp(&client, "9876543210")
        .await
        .expect("request ok");
    let session = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("the code verifies");

    let profiles = fixture(include_str!("fixtures/otp_verify.json"));
    let profile = &profiles[0];
    assert_eq!(
        session.access_token, profile["accessToken"],
        "access token comes from the profile"
    );
    assert_eq!(session.uuid, profile["uuid"], "uuid comes from the profile");
    assert_eq!(
        session.email.as_deref(),
        profile["email"].as_str(),
        "email comes from the profile"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let verify_request = requests
        .iter()
        .find(|request| request.url.path() == VERIFY_PATH)
        .expect("the verify request was sent");
    let body = fixture(&String::from_utf8_lossy(&verify_request.body));
    assert_eq!(
        body["uuid"],
        login_uuid(),
        "the login session id from the request step is sent"
    );
    assert_eq!(body["otp"], "123456", "the code is sent");
    assert_eq!(body["phone"], "9876543210", "the phone is sent");
}

#[tokio::test]
async fn verify_accepts_a_single_profile_object() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        REQUEST_PATH,
        200,
        include_str!("fixtures/otp_request.json"),
    )
    .await;
    let single = {
        let profiles = fixture(include_str!("fixtures/otp_verify.json"));
        serde_json::to_string(&profiles[0]).expect("the profile serializes")
    };
    mount_json(&server, VERIFY_PATH, 200, &single).await;

    let client = client_for(&server);
    request_otp(&client, "9876543210")
        .await
        .expect("request ok");
    let session = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("a single profile object verifies");
    assert_eq!(
        session.uuid,
        profile_uuid(),
        "the profile is read from the object form"
    );
}

#[tokio::test]
async fn a_wrong_code_preserves_the_api_code() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        REQUEST_PATH,
        200,
        include_str!("fixtures/otp_request.json"),
    )
    .await;
    mount_json(
        &server,
        VERIFY_PATH,
        400,
        include_str!("fixtures/error_otp_001.json"),
    )
    .await;

    let client = client_for(&server);
    request_otp(&client, "9876543210")
        .await
        .expect("request ok");
    let err = verify_otp(&client, "9876543210", "000000")
        .await
        .expect_err("a wrong code is an error");

    match err {
        QuboError::Api { code, .. } => {
            assert_eq!(code, "ERR_OTP_001", "the API code is preserved");
        }
        other => panic!("expected Api, got {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_verify_can_be_retried_with_the_same_login_session() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        REQUEST_PATH,
        200,
        include_str!("fixtures/otp_request.json"),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(VERIFY_PATH))
        .respond_with(
            ResponseTemplate::new(400).set_body_string(include_str!("fixtures/error_otp_001.json")),
        )
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path(VERIFY_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/otp_verify.json")),
        )
        .with_priority(2)
        .mount(&server)
        .await;

    let client = client_for(&server);
    request_otp(&client, "9876543210")
        .await
        .expect("request ok");
    verify_otp(&client, "9876543210", "000000")
        .await
        .expect_err("the first attempt fails");

    let session = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("the retry with the same login session succeeds");
    assert_eq!(session.uuid, profile_uuid(), "the retry logs in");
}

fn profile_uuid() -> String {
    fixture(include_str!("fixtures/otp_verify.json"))[0]["uuid"]
        .as_str()
        .expect("fixture carries the account uuid")
        .to_owned()
}

#[tokio::test]
async fn an_error_message_surfaces_verbatim() {
    let server = MockServer::start().await;
    mount_json(
        &server,
        REQUEST_PATH,
        200,
        include_str!("fixtures/otp_request.json"),
    )
    .await;
    mount_json(
        &server,
        VERIFY_PATH,
        429,
        include_str!("fixtures/error_rate_limit.json"),
    )
    .await;

    let client = client_for(&server);
    request_otp(&client, "9876543210")
        .await
        .expect("request ok");
    let err = verify_otp(&client, "9876543210", "123456")
        .await
        .expect_err("a rejected verify is an error");

    let expected = fixture(include_str!("fixtures/error_rate_limit.json"))["message"][0]
        .as_str()
        .expect("fixture message")
        .to_owned();
    match err {
        QuboError::Api { code, message } => {
            assert_eq!(code, "ERR_SM_057", "the API code is preserved");
            assert_eq!(message, expected, "the server's message surfaces verbatim");
        }
        other => panic!("expected Api, got {other:?}"),
    }
}

/// Sends a real SMS. Ignored by default; run deliberately with the family
/// account's number in `QUBO_PHONE`.
#[tokio::test]
#[ignore = "live: sends a real OTP SMS; run with QUBO_PHONE set"]
async fn live_request_otp_reaches_the_gateway() {
    let phone = std::env::var("QUBO_PHONE").expect("set QUBO_PHONE to the account's phone number");
    let base = BASE_URL.parse().expect("the gateway URL parses");
    let client = QuboClient::new(base, ServiceProvider::default());
    request_otp(&client, &phone)
        .await
        .expect("the gateway accepts the OTP request");
}
