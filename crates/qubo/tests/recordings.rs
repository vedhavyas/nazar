//! The recording calendar: the days the cloud holds footage for, as the
//! wire carries them (a bare array of DD-MM-YYYY strings, unordered).
//!
//! Fixture provenance: `calendar.json` is the captured body from a live
//! session (2026-10-16, docs/qubo-vod-verification.md) - the days are
//! deliberately out of order, as the cloud sent them.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;

use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::client::QuboClient;
use qubo::devices::Device;
use qubo::error::QuboError;
use qubo::recordings::recorded_days;
use qubo::session::Session;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const CALENDAR_PATH: &str = "/content-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/calendar";

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
    }
}

fn device() -> Device {
    Device {
        device_id: DEVICE_ID.into(),
        name: "Cam360".into(),
        device_type: "HCP01".into(),
        unit_id: UNIT.into(),
        entity_id: "default".into(),
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
