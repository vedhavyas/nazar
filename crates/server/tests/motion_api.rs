//! The motion API: the server's hero decision, the quality a stream is
//! asked for, and the event socket the home follows.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

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
use server::supervisor::{Pacing, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const HALL: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const GATE: &str = "7d2b4a11-9c3e-4f5a-b6d7-8e9f0a1b2c03";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    base: String,
    go2rtc: common::FakeApi,
    events: EventBus,
}

async fn harness() -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    let data_dir = std::env::temp_dir().join(format!(
        "nazar-motion-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&data_dir).expect("data dir");
    let sessions = SessionStore::new(data_dir.join("session.json"));
    sessions
        .save(&qubo::Session {
            access_token: "token-abc".to_owned(),
            uuid: "user-uuid-1".to_owned(),
            email: None,
        })
        .expect("a stored session");

    mount_json(
        &qubo,
        UNITS_PATH,
        json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string(),
    )
    .await;
    mount_json(
        &qubo,
        DEVICES_PATH,
        json!({ "data": [
            { "deviceUUID": HALL, "deviceName": "Hall", "deviceType": "HCP01" },
            { "deviceUUID": GATE, "deviceName": "Gate", "deviceType": "HCP01" }
        ] })
        .to_string(),
    )
    .await;
    Mock::given(method("POST"))
        .and(path(format!("{MINT_PATH}/device/{HALL}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(json!({
            "streamURL": format!("rtsps://wowza2.platform.quboworld.com:443/x/{HALL}?token=t"),
            "sessionId": "s1",
        })
        .to_string()))
        .mount(&qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(format!("{MINT_PATH}/device/{GATE}")))
        .respond_with(ResponseTemplate::new(200).set_body_string(json!({
            "streamURL": format!("rtsps://wowza2.platform.quboworld.com:443/x/{GATE}?token=t"),
            "sessionId": "s2",
        })
        .to_string()))
        .mount(&qubo)
        .await;

    let client = QuboClient::new(qubo.uri().parse().expect("a url"), SP.into());
    let supervisor = Supervisor::new(
        client.clone(),
        sessions.clone(),
        Go2rtc::connect(go2rtc.base.clone()),
        Pacing {
            remint: Duration::from_secs(30),
            retry_base: Duration::from_millis(50),
            retry_max: Duration::from_millis(200),
            discovery_retry: Duration::from_secs(30),
            stream_idle: Duration::from_secs(120),
        },
    );
    let events = EventBus::new();
    let motion = Arc::new(Mutex::new(MotionOrder::new()));
    server::motion::follow(&events, Arc::clone(&motion), &supervisor);
    let state = AppState::new(
        "test",
        Wiring {
            sessions,
            qubo: client,
            supervisor,
            go2rtc_api: go2rtc.base.to_string(),
            events: events.clone(),
            motion,
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.expect("serves");
    });
    Harness {
        base,
        go2rtc,
        events,
    }
}

async fn mount_json(server: &MockServer, route: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("a client")
}

#[tokio::test]
async fn the_hero_is_the_camera_that_moved() {
    let h = harness().await;
    let browser = client();
    // The camera list is filled lazily by the first gated call.
    let listed = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);

    let before = browser
        .get(format!("{}/api/motion/hero", h.base))
        .send()
        .await
        .expect("hero");
    assert_eq!(
        before.json::<Value>().await.expect("json"),
        json!({ "hero": null }),
        "an empty house has no hero"
    );

    h.events.publish(UnitEvent::Motion {
        device_id: GATE.to_owned(),
    });

    let mut hero = None;
    for _ in 0..50 {
        let response = browser
            .get(format!("{}/api/motion/hero", h.base))
            .send()
            .await
            .expect("hero");
        let body: Value = response.json().await.expect("json");
        if body["hero"] == json!(GATE) {
            hero = Some(body);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        hero,
        Some(json!({ "hero": GATE })),
        "the hero is the server's decision, fed by the event bus"
    );
}

#[tokio::test]
async fn the_hero_follows_motion_after_a_burst_past_the_bus() {
    let h = harness().await;
    let browser = client();
    let listed = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);

    // A burst the follower cannot drain (its bus holds 256): it must come
    // back and keep electing, not die on the first overflow.
    for _ in 0..400 {
        h.events.publish(UnitEvent::Liveness {
            device_id: HALL.to_owned(),
            online: true,
        });
    }
    // A motion from something the account does not have (or that is not a
    // camera) must not take the tile.
    for device in ["not-a-camera", "d0000000-0000-4000-8000-000000000003"] {
        h.events.publish(UnitEvent::Motion {
            device_id: device.to_owned(),
        });
    }
    tokio::time::sleep(Duration::from_millis(100)).await;
    let untouched: Value = browser
        .get(format!("{}/api/motion/hero", h.base))
        .send()
        .await
        .expect("hero")
        .json()
        .await
        .expect("json");
    assert_eq!(
        untouched,
        json!({ "hero": null }),
        "only a camera the account has can be the hero"
    );

    h.events.publish(UnitEvent::Motion {
        device_id: GATE.to_owned(),
    });

    let mut hero = None;
    for _ in 0..50 {
        let body: Value = browser
            .get(format!("{}/api/motion/hero", h.base))
            .send()
            .await
            .expect("hero")
            .json()
            .await
            .expect("json");
        if body["hero"] == json!(GATE) {
            hero = Some(body);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        hero,
        Some(json!({ "hero": GATE })),
        "motion after an overflow still elects"
    );
}

#[tokio::test]
async fn a_stream_is_asked_for_by_quality() {
    let h = harness().await;
    let browser = client();

    let high = browser
        .post(format!("{}/api/cameras/{HALL}/live", h.base))
        .send()
        .await
        .expect("live");
    assert_eq!(
        high.json::<Value>().await.expect("json"),
        json!({ "stream": format!("{HALL}-high") }),
        "the default stream is the hero's high one"
    );

    let low = browser
        .post(format!("{}/api/cameras/{GATE}/live?quality=low", h.base))
        .send()
        .await
        .expect("live low");
    assert_eq!(
        low.json::<Value>().await.expect("json"),
        json!({ "stream": format!("{GATE}-low") }),
        "a tile asks for the low stream"
    );

    let names: Vec<String> = h
        .go2rtc
        .puts()
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert!(
        names.contains(&format!("{HALL}-high")) && names.contains(&format!("{GATE}-low")),
        "both streams reached go2rtc: {names:?}"
    );

    let refused = browser
        .post(format!("{}/api/cameras/{GATE}/live?quality=ultra", h.base))
        .send()
        .await
        .expect("live ultra");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::BAD_REQUEST,
        "a quality that is not one of the two is refused"
    );
}

#[tokio::test]
async fn the_event_socket_carries_the_house() {
    let h = harness().await;
    let url = format!("{}/api/events", h.base.replace("http://", "ws://"));
    let (mut socket, _) =
        tokio_tungstenite::connect_async(url.into_client_request().expect("a ws request"))
            .await
            .expect("the socket opens");
    // Give the subscription a moment to land before publishing.
    tokio::time::sleep(Duration::from_millis(50)).await;

    h.events.publish(UnitEvent::Motion {
        device_id: HALL.to_owned(),
    });
    h.events.publish(UnitEvent::Liveness {
        device_id: GATE.to_owned(),
        online: false,
    });

    let mut seen = Vec::new();
    while seen.len() < 2 {
        let Some(Ok(message)) = tokio::time::timeout(Duration::from_secs(2), socket.next())
            .await
            .expect("a frame arrives")
        else {
            break;
        };
        if let tokio_tungstenite::tungstenite::Message::Text(text) = message {
            seen.push(serde_json::from_str::<Value>(&text).expect("json"));
        }
    }
    assert_eq!(
        seen,
        vec![
            json!({ "type": "motion", "device_id": HALL }),
            json!({ "type": "liveness", "device_id": GATE, "online": false }),
        ],
        "the socket carries every event as it happens"
    );
}
