//! Recordings browsing: the days come from the cloud calendar, the day's
//! segments from the house's own motion memory - no cloud clip-list
//! endpoint exists (measured 2026-10-16, docs/qubo-vod-verification.md).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{QuboClient, SessionStore};
use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::http::{AppState, Wiring, app};
use server::motion::MotionOrder;
use server::notify::{COALESCE, NotificationStore, Snapshots};
use server::supervisor::{Pacing, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const CAMERA: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const OTHER: &str = "9a8b7c6d-5e4f-4a3b-8c2d-1e0f9a8b7c6d";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const CALENDAR_PATH: &str = "/content-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/calendar";

/// Midnight IST, 16 October 2026, in milliseconds: the day every clip in
/// these tests belongs to.
const DAY_START: i64 = 1_792_089_000_000;
const DAY_END: i64 = 1_792_175_400_000;

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nazar-{name}-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    dir
}

/// A store already holding the given ledger, as a restart would find it.
fn store_with(entries: &Value) -> Arc<NotificationStore> {
    let dir = temp_dir("recordings-store");
    let path = dir.join("notifications.json");
    let ledger = json!({ "entries": entries, "sequence": 9 });
    std::fs::write(&path, ledger.to_string()).expect("the ledger writes");
    Arc::new(NotificationStore::open(path, COALESCE))
}

fn entry(id: &str, device: &str, at: i64, kind: &str, snapshot: Option<&str>) -> Value {
    let mut value = json!({
        "id": id,
        "device_id": device,
        "at": at,
        "snapshot_url": snapshot,
        "kind": kind,
    });
    if kind == "aqi" {
        value["value"] = json!(70);
    }
    value
}

struct Harness {
    base: String,
    qubo: MockServer,
}

async fn harness(store: Arc<NotificationStore>) -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    let data_dir = temp_dir("recordings-harness");
    let sessions = SessionStore::new(data_dir.join("session.json"));
    sessions
        .save(&qubo::Session {
            access_token: "token-abc".to_owned(),
            uuid: "user-uuid-1".to_owned(),
            email: None,
        })
        .expect("a stored session");
    let events = EventBus::new();
    let client = QuboClient::new(qubo.uri().parse().expect("a url"), SP.into());
    let supervisor = Supervisor::new(
        client.clone(),
        sessions.clone(),
        Go2rtc::connect(go2rtc.base.clone()),
        Pacing {
            remint: Duration::from_secs(30),
            retry_base: Duration::from_secs(30),
            retry_max: Duration::from_secs(30),
            discovery_retry: Duration::from_secs(30),
            stream_idle: Duration::from_secs(120),
        },
    );
    let snapshots = Snapshots::new(Go2rtc::connect(go2rtc.base.clone()), data_dir.join("snaps"));
    let state = AppState::new(
        "test",
        Wiring {
            sessions,
            qubo: client,
            supervisor,
            go2rtc_api: go2rtc.base.to_string(),
            events,
            motion: Arc::new(Mutex::new(MotionOrder::new())),
            notifications: store,
            snapshots,
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.expect("serves");
    });
    Harness { base, qubo }
}

async fn mount_discovery(server: &MockServer) {
    for (route, body) in [
        (UNITS_PATH, json!([{ "unitUUID": UNIT, "name": "Home" }])),
        (
            DEVICES_PATH,
            json!({ "data": [
                { "deviceUUID": CAMERA, "deviceName": "Hall", "deviceType": "HCP01" },
                { "deviceUUID": OTHER, "deviceName": "Gate", "deviceType": "HCP01" },
            ] }),
        ),
    ] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(ResponseTemplate::new(200).set_body_string(body.to_string()))
            .mount(server)
            .await;
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("a client")
}

#[tokio::test]
async fn the_days_come_from_the_calendar() {
    let h = harness(store_with(&json!([]))).await;
    mount_discovery(&h.qubo).await;
    // The timezone is part of the ask: without it the day is not the
    // household's day.
    Mock::given(method("GET"))
        .and(path(CALENDAR_PATH))
        .and(query_param("timezone", "Asia/Kolkata"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                include_str!("../../qubo/tests/fixtures/calendar.json").to_owned(),
            ),
        )
        .mount(&h.qubo)
        .await;

    let listed: Value = client()
        .get(format!(
            "{}/api/cameras/{CAMERA}/recordings/days?from={DAY_START}&to={DAY_END}",
            h.base
        ))
        .send()
        .await
        .expect("the days")
        .json()
        .await
        .expect("json");

    assert_eq!(
        listed["days"],
        json!(["13-10-2026", "14-10-2026", "15-10-2026", "16-10-2026"]),
        "the days come back oldest first"
    );
}

#[tokio::test]
async fn the_clips_are_the_houses_own_motions_for_that_day() {
    let h = harness(store_with(&json!([
        entry(
            "m1",
            CAMERA,
            DAY_START + 7_200_000,
            "motion",
            Some("/api/snaps/m1")
        ),
        entry("m2", CAMERA, DAY_END - 60_000, "motion", None),
        entry("m3", CAMERA, DAY_START - 60_000, "motion", None),
        entry("m4", OTHER, DAY_START + 3_600_000, "motion", None),
        entry("m5", CAMERA, DAY_START + 3_600_000, "offline", None),
        // The two exact instants: midnight itself belongs to the day, the
        // next midnight does not.
        entry("m6", CAMERA, DAY_START, "motion", None),
        entry("m7", CAMERA, DAY_END, "motion", None),
    ])))
    .await;
    mount_discovery(&h.qubo).await;

    let listed: Value = client()
        .get(format!(
            "{}/api/cameras/{CAMERA}/recordings?day=16-10-2026",
            h.base
        ))
        .send()
        .await
        .expect("the clips")
        .json()
        .await
        .expect("json");

    assert_eq!(
        listed["clips"],
        json!([
            { "id": "m2", "start_ms": DAY_END - 60_000, "snapshot_url": null },
            { "id": "m1", "start_ms": DAY_START + 7_200_000, "snapshot_url": "/api/snaps/m1" },
            { "id": "m6", "start_ms": DAY_START, "snapshot_url": null },
        ]),
        "the day's own motions, newest first: not the neighbours' day, \
         another camera's, or the coming and going of the camera itself"
    );
}

#[tokio::test]
async fn a_day_that_is_not_a_day_is_refused() {
    let h = harness(store_with(&json!([]))).await;
    mount_discovery(&h.qubo).await;

    let bad = client()
        .get(format!(
            "{}/api/cameras/{CAMERA}/recordings?day=2026-10-16",
            h.base
        ))
        .send()
        .await
        .expect("the answer");
    assert_eq!(
        bad.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "a day the calendar never sent is a 400, not an empty list"
    );

    let missing = client()
        .get(format!("{}/api/cameras/{CAMERA}/recordings", h.base))
        .send()
        .await
        .expect("the answer");
    assert_eq!(missing.status(), reqwest::StatusCode::BAD_REQUEST);
}
