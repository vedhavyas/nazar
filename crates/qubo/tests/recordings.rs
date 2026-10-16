//! The recording calendar and VOD minting: the days the cloud holds
//! footage for, and the playback session a clip's instant buys.
//!
//! Fixture provenance: `calendar.json` and `mint_vod.json` are captured
//! from a live session (2026-10-16, docs/qubo-vod-verification.md); the
//! days are deliberately out of order, as the cloud sent them, and the
//! playback token is redacted.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::client::QuboClient;
use qubo::devices::Device;
use qubo::error::QuboError;
use qubo::recordings::{mint_vod, recorded_days};
use qubo::session::Session;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const CALENDAR_PATH: &str = "/content-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/calendar";
const VOD_PATH: &str = "/stream-manager/api/v2/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";

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

fn device() -> Device {
    Device {
        device_id: DEVICE_ID.into(),
        name: "Cam360".into(),
        device_type: "HCP01".into(),
        model: "HCP01".into(),
        unit_id: UNIT.into(),
        entity_id: "default".into(),
        controller_id: None,
    }
}

async fn mount_calendar(server: &MockServer, status: u16, body: &str) {
    Mock::given(method("GET"))
        .and(path(CALENDAR_PATH))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
        .mount(server)
        .await;
}

#[tokio::test]
async fn recorded_days_sorts_the_calendar_and_asks_the_documented_parameters() {
    let server = MockServer::start().await;
    mount_calendar(&server, 200, include_str!("fixtures/calendar.json")).await;

    let days = recorded_days(
        &client_for(&server),
        &session(),
        &device(),
        1_792_089_000_000,
        1_792_175_400_000,
    )
    .await
    .expect("the calendar answers");

    assert_eq!(
        days,
        vec!["13-10-2026", "14-10-2026", "15-10-2026", "16-10-2026"],
        "the days come back oldest first, whatever order the cloud sent"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let query: HashMap<String, String> = requests[0]
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        query.get("fromTimeStamp").map(String::as_str),
        Some("1792089000000"),
        "the window's start is sent, in milliseconds"
    );
    assert_eq!(
        query.get("toTimeStamp").map(String::as_str),
        Some("1792175400000"),
        "and its end"
    );
    assert_eq!(
        query.get("timezone").map(String::as_str),
        Some("Asia/Kolkata"),
        "the household's own day decides what a day is"
    );
}

#[tokio::test]
async fn a_vod_mint_asks_for_the_instant_and_carries_the_url_back() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(VOD_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/mint_vod.json")),
        )
        .mount(&server)
        .await;

    let client = client_for(&server);
    let mint = mint_vod(&client, &session(), &device(), 1_792_089_000_000)
        .await
        .expect("the mint succeeds");

    let payload: Value =
        serde_json::from_str(include_str!("fixtures/mint_vod.json")).expect("fixture is JSON");
    assert_eq!(
        mint.url, payload["streamURL"],
        "the URL comes from streamURL"
    );
    assert_eq!(
        mint.session_id, payload["appSessionId"],
        "the session id comes from appSessionId, not sessionId"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    let query: HashMap<String, String> = requests[0]
        .url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    assert_eq!(
        query.get("timestamp").map(String::as_str),
        Some("1792089000000"),
        "the clip's own instant is asked for"
    );
    assert_eq!(
        query.get("type").map(String::as_str),
        Some("high"),
        "playback asks for the high class"
    );
    assert_eq!(
        query.get("streamSourceCamera").map(String::as_str),
        Some("primary"),
        "the primary camera source is requested"
    );
    assert_eq!(
        query.get("singleRecord").map(String::as_str),
        Some("0"),
        "single recordings are what a timeline seeks in"
    );

    let body: Value = serde_json::from_slice(&requests[0].body).expect("the body is JSON");
    assert_eq!(
        body["userAppID"],
        device().device_id,
        "the camera's own id is sent as userAppID, as the live mint does"
    );
}

#[tokio::test]
async fn recorded_days_sort_by_year_month_day_not_the_wire_order() {
    let server = MockServer::start().await;
    // A window that crosses a month boundary, as the web's 30-day window
    // routinely does: sorting by day-of-month alone would interleave the
    // months.
    mount_calendar(
        &server,
        200,
        r#"["02-11-2026","30-10-2026","31-10-2026","01-11-2026"]"#,
    )
    .await;

    let days = recorded_days(&client_for(&server), &session(), &device(), 0, 1)
        .await
        .expect("the calendar answers");

    assert_eq!(
        days,
        vec!["30-10-2026", "31-10-2026", "01-11-2026", "02-11-2026"],
        "the days sort by (year, month, day), the fields in the order that decides"
    );
}

#[tokio::test]
async fn a_refused_calendar_carries_its_envelope() {
    let server = MockServer::start().await;
    mount_calendar(&server, 401, include_str!("fixtures/error_auth_002.json")).await;

    let err = recorded_days(&client_for(&server), &session(), &device(), 0, 1)
        .await
        .expect_err("a refused calendar is an error");

    assert!(
        matches!(err, QuboError::Auth { .. }),
        "the auth envelope is read as auth: {err:?}"
    );
}
