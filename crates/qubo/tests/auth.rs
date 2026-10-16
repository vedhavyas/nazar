//! OTP login wire tests.
//!
//! Fixture provenance (spec §3.7). `otp_request.json` / `otp_verify.json`
//! carry the response shapes the reference client parses (qubopy `auth.py`)
//! and the stored session file's real shape; all values are synthesized.
//! `error_otp_001.json`'s code is the wrong-OTP code recorded in the APK
//! decompile (`Y4.java:1850`); its message text is synthesized.
//! `error_rate_limit.json` is captured whole - the gateway's own words on
//! 2026-10-15 when the resend window was exhausted. The envelope shape
//! (`message` as an array) is live-captured. What these tests pin is that
//! whatever code and message the gateway sends survive into `QuboError`.
//!
//! `otp_verify_two.json` is the verify shape for a number that maps to two
//! accounts (the response is a per-number LIST); its values are synthesized.
//!
//! `gm_login.json` carries the GM v4 login's response keys
//! (docs/qubo-google-login.md §3; live-verified 2026-10-14 that the route
//! returns a refreshToken), and `refresh.json` the refresh response's
//! (spec §3.2); both bodies' values are synthesized.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::Session;
use qubo::auth::{
    AccountProfile, phone_login, refresh_session, request_otp, select_profile, verify_google,
    verify_otp,
};
use qubo::client::{BASE_URL, QuboClient, ServiceProvider};
use qubo::error::QuboError;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const REQUEST_PATH: &str = "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/login";
const VERIFY_PATH: &str = "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/verify";
const GM_PATH: &str = "/sms/api/v4/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/login";
const REFRESH_PATH: &str =
    "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/users/user-uuid-1/auth/refresh";

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
async fn verify_otp_sends_the_pending_login_session_and_returns_the_profiles() {
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
    let profiles = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("the code verifies");

    let payload = fixture(include_str!("fixtures/otp_verify.json"));
    let profile = &payload[0];
    assert_eq!(profiles.len(), 1, "the number maps to one account");
    let got = &profiles[0];
    assert_eq!(
        got.access_token, profile["accessToken"],
        "access token comes from the profile"
    );
    assert_eq!(got.uuid, profile["uuid"], "uuid comes from the profile");
    assert_eq!(
        got.email.as_deref(),
        profile["email"].as_str(),
        "email comes from the profile"
    );
    assert_eq!(
        got.name.as_deref(),
        profile["name"].as_str(),
        "name comes from the profile"
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
    let profiles = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("a single profile object verifies");
    assert_eq!(profiles.len(), 1, "the object form is a one-profile list");
    assert_eq!(
        profiles[0].uuid,
        profile_uuid(),
        "the profile is read from the object form"
    );
}

#[tokio::test]
async fn verify_returns_every_profile_the_number_maps_to() {
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
        include_str!("fixtures/otp_verify_two.json"),
    )
    .await;

    let client = client_for(&server);
    request_otp(&client, "9876543210")
        .await
        .expect("request ok");
    let profiles = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("the code verifies");

    let fixture = fixture(include_str!("fixtures/otp_verify_two.json"));
    let entries = fixture.as_array().expect("a profile array");
    assert_eq!(
        profiles.len(),
        2,
        "both accounts come back, not just the first"
    );
    for (got, entry) in profiles.iter().zip(entries) {
        assert_eq!(got.uuid, entry["uuid"], "each profile keeps its uuid");
        assert_eq!(
            got.email.as_deref(),
            entry["email"].as_str(),
            "each profile keeps its email"
        );
        assert_eq!(
            got.access_token, entry["accessToken"],
            "each profile keeps its own token"
        );
    }
}

#[test]
fn select_profile_picks_the_asked_account_by_uuid() {
    let entries = fixture(include_str!("fixtures/otp_verify_two.json"));
    let profiles: Vec<AccountProfile> =
        serde_json::from_value(entries).expect("the fixture parses");

    let chosen = select_profile(&profiles, &profiles[1].uuid).expect("the second account resolves");
    assert_eq!(chosen.uuid, profiles[1].uuid, "the pick is keyed by uuid");
    assert_eq!(
        chosen.access_token, profiles[1].access_token,
        "the whole profile rides the pick"
    );

    let first = select_profile(&profiles, &profiles[0].uuid).expect("the first account resolves");
    assert_eq!(
        first.email, profiles[0].email,
        "each uuid resolves to itself"
    );
}

#[test]
fn select_profile_refuses_a_uuid_that_is_not_a_candidate() {
    let entries = fixture(include_str!("fixtures/otp_verify_two.json"));
    let profiles: Vec<AccountProfile> =
        serde_json::from_value(entries).expect("the fixture parses");

    let err = select_profile(&profiles, "00000000-0000-0000-0000-000000000000")
        .expect_err("a uuid nobody offered cannot be chosen");
    assert!(matches!(err, QuboError::Auth { .. }), "{err:?}");
}

/// The mint's wire shape lands from the live capture; until then the seam
/// is a typed refusal, never a silent fallback to the verify token.
#[tokio::test]
async fn the_phone_mint_refuses_until_the_wire_shape_is_pinned() {
    let server = MockServer::start().await;
    let client = client_for(&server);
    let profile = AccountProfile {
        access_token: "token-1".to_owned(),
        uuid: "user-uuid-1".to_owned(),
        name: None,
        email: None,
        refresh_token: None,
    };

    let err = phone_login(&client, &profile)
        .await
        .expect_err("the mint refuses until it is pinned");

    match err {
        QuboError::Auth { message, .. } => {
            assert_eq!(
                message, "mint call not yet pinned",
                "the refusal names the seam"
            );
        }
        other => panic!("expected Auth, got {other:?}"),
    }
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "nothing is sent while the shape is unpinned"
    );
}

#[tokio::test]
async fn verify_refuses_a_different_phone_than_the_pending_code() {
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

    let err = verify_otp(&client, "9000000000", "123456")
        .await
        .expect_err("a different number cannot spend the code");
    match err {
        QuboError::Auth { message, .. } => {
            assert!(
                !message.contains("9876543210") && !message.contains("9000000000"),
                "the refusal echoes neither number: {message}"
            );
        }
        other => panic!("expected Auth, got {other:?}"),
    }

    let profiles = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("the code still works for its own number");
    assert_eq!(profiles[0].uuid, profile_uuid(), "the right number logs in");
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

    let profiles = verify_otp(&client, "9876543210", "123456")
        .await
        .expect("the retry with the same login session succeeds");
    assert_eq!(profiles[0].uuid, profile_uuid(), "the retry logs in");
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

/// Feeds a real Google ID token to the GM login. Ignored by default; run
/// deliberately with the app's own token (docs/qubo-google-login.md) in
/// `QUBO_ID_TOKEN`.
#[tokio::test]
#[ignore = "live: exchanges a real Google ID token; run with QUBO_ID_TOKEN set"]
async fn live_verify_google_exchanges_the_apps_own_token() {
    let id_token = std::env::var("QUBO_ID_TOKEN").expect("set QUBO_ID_TOKEN from the app");
    let base = BASE_URL.parse().expect("the gateway URL parses");
    let client = QuboClient::new(base, ServiceProvider::default());
    let session = verify_google(&client, &id_token)
        .await
        .expect("the gateway accepts the app's token");
    assert!(
        session.refresh_token.is_some(),
        "the GM route is what makes the session durable"
    );
}

#[tokio::test]
async fn verify_google_exchanges_the_id_token_for_a_durable_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(GM_PATH))
        .and(query_param("system", "GM"))
        .and(query_param("type", "RELEASE"))
        .and(header("source", "ANDROID"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/gm_login.json")),
        )
        .mount(&server)
        .await;
    let client = client_for(&server);

    let session = verify_google(&client, "the-id-token")
        .await
        .expect("the login");

    assert_eq!(session.access_token, "gm-access-token");
    assert_eq!(session.uuid, "user-uuid-1");
    assert_eq!(
        session.refresh_token.as_deref(),
        Some("gm-refresh-token"),
        "the refresh token is what makes the session durable"
    );
    let requests = server.received_requests().await.expect("recorded requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("a json body");
    assert_eq!(body["accessToken"], json!("the-id-token"));
    assert_eq!(body["username"], json!(""));
    assert_eq!(body["password"], json!(""));
    assert!(
        body["deviceAttribute"].is_string(),
        "the attribute rides the body: {body}"
    );
}

#[tokio::test]
async fn a_refresh_spends_both_tokens_and_takes_the_fresh_pair() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .and(header("authorization", "Bearer gm-access-token"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/refresh.json")),
        )
        .mount(&server)
        .await;
    let client = client_for(&server);
    let stored = Session {
        access_token: "gm-access-token".to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
        refresh_token: Some("gm-refresh-token".to_owned()),
    };

    let refreshed = refresh_session(&client, &stored)
        .await
        .expect("the refresh");

    assert_eq!(refreshed.access_token, "gm-access-token-2");
    assert_eq!(
        refreshed.refresh_token.as_deref(),
        Some("gm-refresh-token-2")
    );
    assert_eq!(refreshed.uuid, "user-uuid-1", "the account does not move");
    let requests = server.received_requests().await.expect("recorded requests");
    let body: Value = serde_json::from_slice(&requests[0].body).expect("a json body");
    assert_eq!(body["accessToken"], json!("gm-access-token"));
    assert_eq!(body["refreshToken"], json!("gm-refresh-token"));
}

#[tokio::test]
async fn a_refresh_without_a_rotation_keeps_the_stored_token() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .respond_with(
            // Spec §3.2: refreshToken is optional, and the old one is kept.
            ResponseTemplate::new(200)
                .set_body_string(json!({ "accessToken": "gm-access-token-2" }).to_string()),
        )
        .mount(&server)
        .await;
    let client = client_for(&server);
    let stored = Session {
        access_token: "gm-access-token".to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
        refresh_token: Some("gm-refresh-token".to_owned()),
    };

    let refreshed = refresh_session(&client, &stored)
        .await
        .expect("the refresh");

    assert_eq!(refreshed.access_token, "gm-access-token-2");
    assert_eq!(
        refreshed.refresh_token.as_deref(),
        Some("gm-refresh-token"),
        "no rotation means the stored token stays"
    );
}

#[tokio::test]
async fn a_refresh_without_a_token_is_refused_locally() {
    let server = MockServer::start().await;
    let client = client_for(&server);
    let stored = Session {
        access_token: "gm-access-token".to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
        refresh_token: None,
    };

    let failure = refresh_session(&client, &stored)
        .await
        .expect_err("nothing to spend");

    assert!(matches!(failure, QuboError::Auth { .. }), "{failure:?}");
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "no call is made without a token to spend"
    );
}
