//! The notification service: bursts that are one entry, transitions that
//! are two, air that is only worth telling past the line, a store that
//! outlives a restart, and stills that come from a pipeline already running.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt as _;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{QuboClient, SessionStore};
use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::http::{AppState, Wiring, app};
use server::motion::MotionOrder;
use server::mqtt::UnitEvent;
use server::notify::{AQI_ALERT_AT, COALESCE, NotificationStore, Snapshots};
use server::supervisor::{Pacing, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const CAMERA: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";

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

fn store_in(dir: &std::path::Path) -> Arc<NotificationStore> {
    Arc::new(NotificationStore::open(
        dir.join("notifications.json"),
        COALESCE,
    ))
}

fn motion(device: &str) -> UnitEvent {
    UnitEvent::Motion {
        device_id: device.to_owned(),
        image: None,
    }
}

fn liveness(device: &str, online: bool) -> UnitEvent {
    UnitEvent::Liveness {
        device_id: device.to_owned(),
        online,
    }
}

fn aqi(device: &str, value: u32) -> UnitEvent {
    UnitEvent::Aqi {
        device_id: device.to_owned(),
        value,
    }
}

#[test]
fn a_motion_burst_is_one_notification() {
    let dir = temp_dir("notify-burst");
    let store = store_in(&dir);
    let start = Instant::now();

    let first = store.push(&motion("hall"), start);
    assert!(first.is_some(), "the first sighting is told");

    for secs in [1, 2, 3] {
        let again = store.push(&motion("hall"), start + Duration::from_secs(secs));
        assert!(
            again.is_none(),
            "a sighting inside the window is the same episode"
        );
    }
    assert_eq!(store.list().len(), 1, "one episode, one entry");

    // A window past the last sighting, a fresh burst is a fresh entry.
    let later = store.push(
        &motion("hall"),
        start + Duration::from_secs(3) + COALESCE + Duration::from_secs(1),
    );
    assert!(
        later.is_some(),
        "a sighting past the window is a new episode"
    );
    assert_eq!(store.list().len(), 2, "two episodes, two entries");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn offline_and_online_are_each_told() {
    let dir = temp_dir("notify-liveness");
    let store = store_in(&dir);
    let start = Instant::now();

    assert!(
        store.push(&liveness("hall", false), start).is_some(),
        "a camera going away is told"
    );
    assert!(
        store
            .push(&liveness("hall", true), start + Duration::from_secs(1))
            .is_some(),
        "and so is its return, right after: those are transitions, not a burst"
    );
    let kinds: Vec<Value> = store
        .list()
        .iter()
        .map(|entry| serde_json::to_value(&entry.kind).expect("kind"))
        .collect();
    assert_eq!(
        kinds,
        vec![json!({ "kind": "online" }), json!({ "kind": "offline" })],
        "newest first"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn air_is_told_only_past_the_line() {
    let dir = temp_dir("notify-aqi");
    let store = store_in(&dir);
    let start = Instant::now();

    assert!(
        store
            .push(&aqi("purifier", AQI_ALERT_AT - 1), start)
            .is_none(),
        "a reading under the line is not worth telling"
    );
    assert!(
        store.push(&aqi("purifier", AQI_ALERT_AT), start).is_some(),
        "at the line it is"
    );
    let entries = store.list();
    assert_eq!(entries.len(), 1);
    assert_eq!(
        serde_json::to_value(&entries[0].kind).expect("kind"),
        json!({ "kind": "aqi", "value": AQI_ALERT_AT }),
        "and the reading is carried"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_store_survives_a_restart() {
    let dir = temp_dir("notify-restart");
    {
        let store = store_in(&dir);
        store.push(&motion("hall"), Instant::now());
    }
    let reopened = store_in(&dir);
    let entries = reopened.list();
    assert_eq!(entries.len(), 1, "the entry outlived its process");
    assert_eq!(entries[0].device_id, "hall");
    assert_eq!(
        serde_json::to_value(&entries[0].kind).expect("kind"),
        json!({ "kind": "motion" })
    );

    // A new push after the restart still gets its own id.
    let next = reopened.push(&liveness("hall", false), Instant::now());
    let next = next.expect("a transition after the restart is told");
    assert_ne!(next.id, entries[0].id, "ids do not repeat across a restart");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_ring_forgets_the_oldest() {
    let dir = temp_dir("notify-ring");
    let store = store_in(&dir);
    let now = Instant::now();

    for n in 0..250 {
        // A different camera each time, so nothing coalesces.
        assert!(store.push(&motion(&format!("cam-{n}")), now).is_some());
    }
    let entries = store.list();
    assert_eq!(entries.len(), 200, "the ring holds two hundred");
    assert_eq!(entries[0].device_id, "cam-249", "newest first");
    assert_eq!(
        entries[199].device_id, "cam-50",
        "the oldest forty are gone"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

struct Harness {
    base: String,
    qubo: MockServer,
    go2rtc: common::FakeApi,
    store: Arc<NotificationStore>,
    supervisor: Supervisor,
    /// The bus the service follows: events go in here, not into the store.
    events: EventBus,
}

async fn harness(store: Arc<NotificationStore>) -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    let data_dir = temp_dir("notify-harness");
    // The stills come from the same fake go2rtc the streams do.
    let snapshots = Snapshots::new(Go2rtc::connect(go2rtc.base.clone()), data_dir.join("snaps"));
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
    server::notify::start(&events, Arc::clone(&store), snapshots.clone());

    let state = AppState::new(
        "test",
        Wiring {
            sessions,
            qubo: client,
            supervisor: supervisor.clone(),
            go2rtc_api: go2rtc.base.to_string(),
            events: events.clone(),
            motion: Arc::new(Mutex::new(MotionOrder::new())),
            notifications: Arc::clone(&store),
            snapshots: snapshots.clone(),
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.expect("serves");
    });
    Harness {
        base,
        qubo,
        go2rtc,
        store,
        supervisor,
        events,
    }
}

async fn mount_discovery(server: &MockServer) {
    for (route, body) in [
        (UNITS_PATH, json!([{ "unitUUID": UNIT, "name": "Home" }])),
        (
            DEVICES_PATH,
            json!({ "data": [{ "deviceUUID": CAMERA, "deviceName": "Hall", "deviceType": "HCP01" }] }),
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
async fn a_still_comes_from_a_pipeline_already_running() {
    let dir = temp_dir("notify-snaps");
    // A short window, so the second sighting is its own episode.
    let store = Arc::new(NotificationStore::open(
        dir.join("notifications.json"),
        Duration::from_millis(100),
    ));
    let h = harness(Arc::clone(&store)).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    Mock::given(method("POST"))
        .and(path(format!(
            "/stream-manager/api/v1/sp/{SP}/stream/unit/{UNIT}/device/{CAMERA}"
        )))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({
                    "streamURL": "rtsps://wowza2.platform.quboworld.com:443/x/cam?token=t",
                    "sessionId": "s1",
                })
                .to_string(),
            ),
        )
        .mount(&h.qubo)
        .await;

    // No pipeline yet: a motion is still told, with no still.
    h.events.publish(motion(CAMERA));
    let told = common::eventually(|| async { !h.store.list().is_empty() }).await;
    assert!(told, "the service told it");
    assert_eq!(
        h.store.list()[0].snapshot_url,
        None,
        "nothing is grabbed from a camera that is not streaming"
    );

    // With a live stream, the frame is taken and served.
    let browser = client();
    let started = browser
        .post(format!("{}/api/cameras/{CAMERA}/live", h.base))
        .send()
        .await
        .expect("watch");
    assert_eq!(started.status(), reqwest::StatusCode::OK);
    let live = common::eventually(|| async {
        h.go2rtc
            .puts()
            .iter()
            .any(|(name, _)| name == &format!("{CAMERA}-high"))
    })
    .await;
    assert!(live, "the stream reached go2rtc");

    // A configured but not running pipeline is not a live one: no still
    // may be grabbed from it.
    h.go2rtc.set_producers(false);
    tokio::time::sleep(Duration::from_millis(150)).await;
    h.events.publish(motion(CAMERA));
    let told = common::eventually(|| async { h.store.list().len() == 2 }).await;
    assert!(told, "the second episode is told");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        h.store.list()[0].snapshot_url,
        None,
        "a stream with no producer is not a pipeline that is running"
    );

    // Running again: a fresh episode, past the window, and its still is
    // taken.
    h.go2rtc.set_producers(true);
    tokio::time::sleep(Duration::from_millis(150)).await;
    h.events.publish(motion(CAMERA));
    let grabbed = common::eventually(|| async {
        h.store
            .list()
            .first()
            .is_some_and(|entry| entry.snapshot_url.is_some())
    })
    .await;
    assert!(grabbed, "a still was resolved for the running pipeline");

    let entry = h.store.list().first().cloned().expect("the entry");
    let url = entry.snapshot_url.expect("a still");
    let still = browser
        .get(format!("{}{url}", h.base))
        .send()
        .await
        .expect("the still");
    assert_eq!(still.status(), reqwest::StatusCode::OK);
    assert_eq!(
        still
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok()),
        Some("image/jpeg"),
        "served as a picture"
    );
    assert_eq!(
        still.bytes().await.expect("bytes").len(),
        4,
        "and it is the frame go2rtc gave"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_fetchable_image_on_the_event_wins() {
    let dir = temp_dir("notify-event-image");
    let store = store_in(&dir);
    let h = harness(Arc::clone(&store)).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.events.publish(UnitEvent::Motion {
        device_id: CAMERA.to_owned(),
        image: Some("https://cdn.example/still.jpg".to_owned()),
    });
    let attached = common::eventually(|| async {
        h.store.list().first().is_some_and(|entry| {
            entry.snapshot_url.as_deref() == Some("https://cdn.example/still.jpg")
        })
    })
    .await;
    assert!(attached, "the feed's own image is used as it stands");
    assert!(
        h.go2rtc.puts().is_empty(),
        "and no pipeline was started for it"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn the_bell_reads_the_list_and_hears_the_socket() {
    let dir = temp_dir("notify-http");
    let store = store_in(&dir);
    let h = harness(Arc::clone(&store)).await;

    let url = format!("{}/api/events", h.base.replace("http://", "ws://"));
    let (mut socket, _) =
        tokio_tungstenite::connect_async(url.into_client_request().expect("a ws request"))
            .await
            .expect("the socket opens");
    tokio::time::sleep(Duration::from_millis(50)).await;

    h.store.push(&liveness(CAMERA, false), Instant::now());
    h.store.announce(
        &h.store
            .list()
            .first()
            .cloned()
            .expect("the entry is in the list"),
    );

    let mut heard = None;
    while heard.is_none() {
        let Some(Ok(message)) = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("a frame arrives")
        else {
            break;
        };
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            let frame: Value = serde_json::from_str(&text).expect("json");
            if frame["type"] == json!("notification") {
                heard = Some(frame);
            }
        }
    }
    let heard = heard.expect("a notification frame");
    assert_eq!(
        heard["notification"]["kind"],
        json!("offline"),
        "the frame carries the notification whole"
    );
    assert!(
        heard["notification"]["id"].is_string(),
        "with the id the still is served under"
    );

    let listed: Value = client()
        .get(format!("{}/api/notifications", h.base))
        .send()
        .await
        .expect("the list")
        .json()
        .await
        .expect("json");
    assert_eq!(
        listed["notifications"][0]["kind"],
        json!("offline"),
        "and the list reads the same entry"
    );

    let missing = client()
        .get(format!("{}/api/snaps/never-made", h.base))
        .send()
        .await
        .expect("a still that does not exist");
    assert_eq!(missing.status(), reqwest::StatusCode::NOT_FOUND);
    let _ = std::fs::remove_dir_all(&dir);
}
