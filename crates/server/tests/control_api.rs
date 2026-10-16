//! Control API tests: PTZ and privacy, where the shadow is the truth and
//! the camera's ack is not.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::http::{AppState, ControlTiming, Wiring, app};
use server::motion::MotionOrder;
use server::notify::{NotificationStore, Snapshots};
use server::supervisor::{Pacing, Supervisor};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use qubo::{QuboClient, SessionStore};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const ENTITY: &str = "30000000-0000-4000-8000-000000000001";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const SHADOW_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/entities/30000000-0000-4000-8000-000000000001/devices/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/shadow";
const COMMAND_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/entities/30000000-0000-4000-8000-000000000001/devices/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/command";

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    base: String,
    qubo: MockServer,
}

async fn harness() -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    let data_dir = std::env::temp_dir().join(format!(
        "nazar-control-{}-{}",
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
            refresh_token: None,
        })
        .expect("a stored session");
    let client = QuboClient::new(qubo.uri().parse().expect("a url"), SP.into());
    let supervisor = Supervisor::new(
        client.clone(),
        sessions.clone(),
        Go2rtc::connect(go2rtc.base.clone()).expect("a client"),
        Pacing {
            remint: Duration::from_secs(30),
            retry_base: Duration::from_secs(30),
            retry_max: Duration::from_secs(30),
            discovery_retry: Duration::from_secs(30),
            stream_idle: Duration::from_secs(120),
            connect_grace: Duration::from_millis(50),
            connect_window: Duration::from_millis(300),
            liveness_poll: Duration::from_millis(50),
            touch_ttl: Duration::from_millis(300),
            warm_idle: Duration::from_secs(60),
            arm_timeout: Duration::from_secs(15),
        },
    );
    let mut state = AppState::new(
        "test",
        Wiring {
            sessions,
            qubo: client,
            supervisor,
            go2rtc_api: go2rtc.base.to_string(),
            events: EventBus::new(),
            motion: Arc::new(Mutex::new(MotionOrder::new())),
            notifications: Arc::new(NotificationStore::open(
                data_dir.join("notifications.json"),
                Duration::from_secs(30),
            )),
            episodes: common::episodes_in(&data_dir),
            snapshots: Snapshots::new(
                Go2rtc::connect(go2rtc.base.clone()).expect("a client"),
                data_dir.join("snaps"),
            ),
        },
    );
    // The poll window is a knob so a write that never lands does not cost
    // the suite two real seconds.
    state.control = ControlTiming {
        poll_every: Duration::from_millis(10),
        window: Duration::from_millis(80),
    };

    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.expect("serves");
    });
    Harness { base, qubo }
}

/// Answer shadow reads with `bodies` in order, repeating the last one.
struct Bodies {
    bodies: Vec<String>,
    calls: Arc<AtomicUsize>,
}

impl Respond for Bodies {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        let at = self
            .calls
            .fetch_add(1, Ordering::SeqCst)
            .min(self.bodies.len() - 1);
        ResponseTemplate::new(200).set_body_string(self.bodies[at].clone())
    }
}

async fn mount_discovery(server: &MockServer) {
    mount_json(
        server,
        UNITS_PATH,
        json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string(),
    )
    .await;
    mount_json(
        server,
        DEVICES_PATH,
        json!({ "data": [{ "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "HCP01", "entityUUID": ENTITY }] })
            .to_string(),
    )
    .await;
}

async fn mount_json(server: &MockServer, route: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

/// Serve shadow reads from `bodies` and accept the command write.
async fn mount_control(server: &MockServer, bodies: Vec<String>) {
    Mock::given(method("GET"))
        .and(path(SHADOW_PATH))
        .respond_with(Bodies {
            bodies,
            calls: Arc::new(AtomicUsize::new(0)),
        })
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(path(COMMAND_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string("SUCCESS_UM_006"))
        .mount(server)
        .await;
}

/// A shadow carrying the pan/tilt position and the lens state.
fn shadow(ptz: &str, camera: &str) -> String {
    format!(
        r#"{{"services":[{{"service":"panTiltControl","attributes":{{"horizontalVerticalPostion":{{"value":"{ptz}"}}}}}},{{"service":"cameraFunction","attributes":{{"state":"{camera}"}}}}]}}"#
    )
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("a client")
}

async fn command_bodies(server: &MockServer) -> Vec<Value> {
    server
        .received_requests()
        .await
        .expect("the requests")
        .iter()
        .filter(|request| request.method.as_str() == "PUT")
        .map(|request| serde_json::from_slice(&request.body).expect("a json body"))
        .collect()
}

#[tokio::test]
async fn ptz_moves_by_the_step_and_reports_the_shadow_position() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    mount_control(
        &h.qubo,
        vec![shadow("10,10", "enable"), shadow("20,15", "enable")],
    )
    .await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "ok": true, "position": { "x": 20, "y": 15 } }),
        "the answer is the shadow's position, not the ack"
    );

    let bodies = command_bodies(&h.qubo).await;
    assert_eq!(bodies.len(), 1, "one step is one write");
    assert_eq!(
        bodies[0]["payload"]["devices"]["services"]["panTiltControl"]["attributes"]["horizontalVerticalPostion"],
        json!("20,15"),
        "the step is added to the position the camera reported"
    );
}

#[tokio::test]
async fn a_slow_camera_is_given_the_window_to_land() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    mount_control(
        &h.qubo,
        vec![
            shadow("10,10", "enable"),
            shadow("10,10", "enable"),
            shadow("20,15", "enable"),
        ],
    )
    .await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");

    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "ok": true, "position": { "x": 20, "y": 15 } }),
        "a camera that lands on the target a moment later still moved"
    );
}

#[tokio::test]
async fn a_move_the_camera_never_takes_is_not_ok() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    mount_control(&h.qubo, vec![shadow("10,10", "enable")]).await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "ok": false, "reason": "not applied", "position": { "x": 10, "y": 10 } }),
        "a shadow that never moved is a failed move, whatever the ack said"
    );
}

#[tokio::test]
async fn two_steps_at_once_do_not_lose_one_another() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;

    // A camera that reports where it actually is: the shadow reflects the
    // last write, as the real one does. A mock with canned answers cannot
    // tell a lost update from an applied one.
    let position = Arc::new(Mutex::new((0_i64, 0_i64)));
    Mock::given(method("GET"))
        .and(path(SHADOW_PATH))
        .respond_with({
            let position = Arc::clone(&position);
            move |_request: &Request| {
                let at = *position.lock().expect("the camera");
                // A real shadow read is not instant; the delay is what
                // makes two steps actually overlap when nothing serializes
                // them.
                ResponseTemplate::new(200)
                    .set_body_string(shadow(&format!("{},{}", at.0, at.1), "enable"))
                    .set_delay(Duration::from_millis(100))
            }
        })
        .mount(&h.qubo)
        .await;
    Mock::given(method("PUT"))
        .and(path(COMMAND_PATH))
        .respond_with({
            let position = Arc::clone(&position);
            move |request: &Request| {
                let body: Value = serde_json::from_slice(&request.body).expect("the command body");
                let target = body["payload"]["devices"]["services"]["panTiltControl"]["attributes"]
                    ["horizontalVerticalPostion"]
                    .as_str()
                    .expect("a target");
                let (x, y) = target.split_once(',').expect("x,y");
                *position.lock().expect("the camera") =
                    (x.parse().expect("x"), y.parse().expect("y"));
                ResponseTemplate::new(200).set_body_string("SUCCESS_UM_006")
            }
        })
        .mount(&h.qubo)
        .await;

    // Two browsers, two connections: one client would queue the second
    // request behind the first and the race could never show.
    let first = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 0 }))
        .send();
    let second = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 0 }))
        .send();
    let (first, second) = tokio::join!(first, second);
    for response in [first, second] {
        let body: Value = response.expect("the call").json().await.expect("json");
        assert_eq!(body["ok"], json!(true), "each step was applied: {body}");
    }

    assert_eq!(
        *position.lock().expect("the camera"),
        (20, 0),
        "the second step added to what the first left behind, not to the same base"
    );
}

#[tokio::test]
async fn a_step_beyond_the_cap_is_refused() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    mount_control(&h.qubo, vec![shadow("10,10", "enable")]).await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 100_000, "dy": 0 }))
        .send()
        .await
        .expect("the call");

    assert_eq!(response.status(), reqwest::StatusCode::BAD_REQUEST);
    let body: Value = response.json().await.expect("json");
    assert_eq!(
        body["error"],
        json!("a step is at most 60 units"),
        "the refusal says what the limit is"
    );
    assert!(
        command_bodies(&h.qubo).await.is_empty(),
        "nothing reaches the camera"
    );

    // One at the cap still goes through.
    let at_cap = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 60, "dy": 0 }))
        .send()
        .await
        .expect("the call");
    assert_eq!(at_cap.status(), reqwest::StatusCode::OK);
    assert_eq!(
        command_bodies(&h.qubo).await.len(),
        1,
        "a step at the cap is a step"
    );
}

#[tokio::test]
async fn a_camera_without_a_reported_position_cannot_be_stepped() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("GET"))
        .and(path(SHADOW_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"services":[{"service":"cameraFunction","attributes":{"state":"enable"}}]}"#,
        ))
        .mount(&h.qubo)
        .await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");

    assert_eq!(
        response.status(),
        reqwest::StatusCode::BAD_GATEWAY,
        "a step needs a place to step from"
    );
}

#[tokio::test]
async fn privacy_round_trips_through_the_shadow() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    mount_control(&h.qubo, vec![shadow("10,10", "disable")]).await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/privacy", h.base))
        .json(&json!({ "on": true }))
        .send()
        .await
        .expect("the call");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "ok": true, "privacy": true }),
        "privacy reads back from camera_enabled, not from the ack"
    );

    let bodies = command_bodies(&h.qubo).await;
    assert_eq!(
        bodies[0]["payload"]["devices"]["services"]["cameraFunction"]["attributes"]["state"],
        json!("disable"),
        "privacy on is cameraFunction.state = disable"
    );
}

#[tokio::test]
async fn privacy_that_never_lands_is_not_ok() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    mount_control(&h.qubo, vec![shadow("10,10", "enable")]).await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/privacy", h.base))
        .json(&json!({ "on": true }))
        .send()
        .await
        .expect("the call");

    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "ok": false, "reason": "not applied", "privacy": false }),
        "the camera still reports its lens open, so the write did not land"
    );
}

#[tokio::test]
async fn a_camera_that_never_reports_privacy_is_unknown_not_off() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("GET"))
        .and(path(SHADOW_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"services":[{"service":"panTiltControl","attributes":{"horizontalVerticalPostion":{"value":"0,0"}}}]}"#,
        ))
        .mount(&h.qubo)
        .await;
    Mock::given(method("PUT"))
        .and(path(COMMAND_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string("SUCCESS_UM_006"))
        .mount(&h.qubo)
        .await;

    let response = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/privacy", h.base))
        .json(&json!({ "on": true }))
        .send()
        .await
        .expect("the call");

    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "ok": false, "reason": "not applied", "privacy": null }),
        "a camera that says nothing about its lens is unknown, never off"
    );
}

#[tokio::test]
async fn the_lens_state_reads_from_the_shadow() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    mount_control(&h.qubo, vec![shadow("10,10", "disable")]).await;

    let response = client()
        .get(format!("{}/api/cameras/{DEVICE_ID}/privacy", h.base))
        .send()
        .await
        .expect("the call");

    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "privacy": true }),
        "what the camera reports is what the screen shows on arrival"
    );
}

#[tokio::test]
async fn a_camera_silent_about_its_lens_reads_unknown() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("GET"))
        .and(path(SHADOW_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"services":[{"service":"panTiltControl","attributes":{"horizontalVerticalPostion":{"value":"0,0"}}}]}"#,
        ))
        .mount(&h.qubo)
        .await;

    let response = client()
        .get(format!("{}/api/cameras/{DEVICE_ID}/privacy", h.base))
        .send()
        .await
        .expect("the call");

    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "privacy": null }),
        "silence is not a lens state"
    );
}

#[tokio::test]
async fn an_unknown_camera_is_not_found() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;

    let response = client()
        .post(format!(
            "{}/api/cameras/0f1e2d3c-4b5a-6978-8a9b-0c1d2e3f4a5b/ptz",
            h.base
        ))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");

    assert_eq!(response.status(), reqwest::StatusCode::NOT_FOUND);
}
