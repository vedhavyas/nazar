//! Fast-lane wiring: the cloud path is the floor (a failed or over-bound
//! arm, or a session that ended without media, falls back to the mint),
//! PTZ rides the engine while a session is armed, and one fast-lane
//! session serves a camera however many of its streams ask.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{Quality, QuboClient, Session, SessionStore};
use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::http::{AppState, ControlTiming, Wiring, app};
use server::motion::MotionOrder;
use server::notify::{NotificationStore, Snapshots};
use server::supervisor::{LaneSession, Pacing, StreamState, Supervisor};
use tokio::net::TcpListener;

use common::{FakeApi, FakeLane};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const ENTITY: &str = "30000000-0000-4000-8000-000000000001";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const SECOND_CAMERA: &str = "7d2b4a11-9c3e-4f5a-b6d7-8e9f0a1b2c03";
const CONTROLLER: &str = "d0000000-0000-4000-8000-000000000001";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const SHADOW_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/entities/30000000-0000-4000-8000-000000000001/devices/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/shadow";
const COMMAND_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/entities/30000000-0000-4000-8000-000000000001/devices/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/command";

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    base: String,
    qubo: MockServer,
    go2rtc: FakeApi,
    supervisor: Supervisor,
    lane: Arc<FakeLane>,
}

/// A harness with the fast lane installed for `prefer`; `remint` is the
/// loop's re-mint cadence and `lane_cooldown` how long a media-silent
/// session benches the lane (both long, unless a test watches the retry).
async fn harness(
    lane: FakeLane,
    prefer: Vec<String>,
    arm_timeout: Duration,
    remint: Duration,
    lane_cooldown: Duration,
) -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = FakeApi::start().await;
    let data_dir = std::env::temp_dir().join(format!(
        "nazar-fast-lane-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&data_dir).expect("data dir");

    let sessions = SessionStore::new(data_dir.join("session.json"));
    sessions
        .save(&Session {
            access_token: "token-abc".to_owned(),
            uuid: "user-uuid-1".to_owned(),
            email: None,
            refresh_token: None,
        })
        .expect("a stored session");
    let client = QuboClient::new(qubo.uri().parse().expect("a url"), SP.into());
    let lane = Arc::new(lane);
    let supervisor = Supervisor::new(
        client.clone(),
        sessions.clone(),
        Go2rtc::connect(go2rtc.base.clone()).expect("a client"),
        Pacing {
            remint,
            retry_base: Duration::from_millis(20),
            retry_max: Duration::from_millis(200),
            discovery_retry: Duration::from_secs(30),
            stream_idle: Duration::from_secs(120),
            connect_grace: Duration::from_millis(50),
            connect_window: Duration::from_millis(300),
            liveness_poll: Duration::from_millis(50),
            touch_ttl: Duration::from_millis(300),
            arm_timeout,
            lane_cooldown,
        },
    )
    .with_fast_lane(
        Arc::clone(&lane) as Arc<dyn server::supervisor::Lane>,
        prefer,
    );

    let mut state = AppState::new(
        "test",
        Wiring {
            sessions,
            qubo: client,
            supervisor: supervisor.clone(),
            go2rtc_api: go2rtc.base.to_string(),
            events: EventBus::new(),
            motion: Arc::new(std::sync::Mutex::new(MotionOrder::new())),
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
    state.control = ControlTiming {
        poll_every: Duration::from_millis(10),
        window: Duration::from_millis(80),
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.expect("serves");
    });

    mount_discovery(&qubo).await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({
                "streamURL": format!("rtsps://wowza2.platform.quboworld.com:443/hero-iot-live/{DEVICE_ID}_CAMERA_VOD0?token=cloud"),
                "sessionId": "session-1",
            })
            .to_string(),
        ))
        .mount(&qubo)
        .await;
    supervisor.refresh_devices().await.expect("discovery");

    Harness {
        base,
        qubo,
        go2rtc,
        supervisor,
        lane,
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
        json!({ "data": [
            { "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "HCP01", "entityUUID": ENTITY, "controllerUUID": CONTROLLER },
            { "deviceUUID": SECOND_CAMERA, "deviceName": "Gate", "deviceType": "HCP01", "entityUUID": ENTITY },
        ] })
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

/// A shadow carrying the pan/tilt position, then the same write twice more.
fn shadow(ptz: &str) -> String {
    format!(
        r#"{{"services":[{{"service":"panTiltControl","attributes":{{"horizontalVerticalPostion":{{"value":"{ptz}"}}}}}}]}}"#
    )
}

async fn mount_control(server: &MockServer, positions: Vec<&str>) {
    let bodies: Vec<String> = positions.into_iter().map(shadow).collect();
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_mock = Arc::clone(&calls);
    Mock::given(method("GET"))
        .and(path(SHADOW_PATH))
        .respond_with(move |_request: &wiremock::Request| {
            let at = calls_mock
                .fetch_add(1, Ordering::SeqCst)
                .min(bodies.len() - 1);
            ResponseTemplate::new(200).set_body_string(bodies[at].clone())
        })
        .mount(server)
        .await;
    Mock::given(method("PUT"))
        .and(path(COMMAND_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string("SUCCESS_UM_006"))
        .mount(server)
        .await;
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

/// The armed envelope's skeleton is captured frame 1886's own (qubo's
/// byte-vector fixture): the wrapper's keys, and the device block's key set
/// with handleName included.
fn assert_envelope_skeleton(envelope: &Value) {
    let vectors: Value = serde_json::from_str(include_str!(
        "../../qubo/tests/fixtures/p2p_byte_vectors.json"
    ))
    .expect("the vector fixture is JSON");
    let captured: Value = serde_json::from_str(
        vectors["control_capture"]["json"]
            .as_str()
            .expect("the captured frame's json"),
    )
    .expect("the captured frame is JSON");
    let keys = |value: &Value| -> std::collections::BTreeSet<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(
        keys(envelope),
        keys(&captured),
        "the envelope's top level is the capture's"
    );
    assert_eq!(
        keys(&envelope["command"]["devices"]),
        keys(&captured["command"]["devices"]),
        "the device block carries the capture's keys, handleName included"
    );
}

async fn is_live(supervisor: &Supervisor) -> bool {
    matches!(
        supervisor.status_for(DEVICE_ID, Quality::High).await,
        StreamState::Live { .. }
    )
}

/// A cloud-minted source: the fallback's own shape.
fn is_cloud_source(source: &str) -> bool {
    source.contains("rtsps://") || source.contains("token=cloud")
}

#[tokio::test]
async fn an_arm_failure_falls_back_to_the_cloud_mint() {
    let h = harness(
        FakeLane::failing("the relay stayed silent"),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");

    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the stream goes live through the cloud path");
    let puts = h.go2rtc.puts();
    assert!(
        puts.iter()
            .any(|(name, source)| name == &format!("{DEVICE_ID}-high") && is_cloud_source(source)),
        "the cloud mint's source is what reached go2rtc: {puts:?}"
    );
    assert_eq!(h.lane.arms(), 1, "the fast lane was attempted once");
}

#[tokio::test]
async fn a_hanging_arm_falls_back_within_the_bound() {
    let h = harness(
        FakeLane::hanging(Duration::from_secs(30)),
        vec![DEVICE_ID.to_owned()],
        Duration::from_millis(200),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");

    // The arm hangs for 30s; the bound is 200ms. Only the bound firing
    // inside the ~2s poll window can make the cloud source appear.
    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the bound fired and the cloud path took over");
    let puts = h.go2rtc.puts();
    assert!(
        puts.iter()
            .any(|(name, source)| name == &format!("{DEVICE_ID}-high") && is_cloud_source(source)),
        "the fallback's source reached go2rtc: {puts:?}"
    );
    assert_eq!(
        h.lane.arms(),
        1,
        "the fast lane was still only attempted once"
    );
}

#[tokio::test]
async fn one_cameras_slow_arm_does_not_hold_another_camera_back() {
    // Camera A's arm hangs; camera B asks while it does. The arm lock is
    // per camera, so B's arm is invoked promptly - a global lock would
    // queue B behind A's whole attempt.
    let h = harness(
        FakeLane::hanging(Duration::from_secs(30)),
        vec![DEVICE_ID.to_owned(), SECOND_CAMERA.to_owned()],
        // Long enough that A's hanging attempt would hold a shared lock for
        // the whole test window.
        Duration::from_secs(5),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the first camera's ensure accepts");
    // Let A enter its attempt, then ask for B.
    tokio::time::sleep(Duration::from_millis(100)).await;
    h.supervisor
        .ensure(SECOND_CAMERA, Quality::High, false)
        .await
        .expect("the second camera's ensure accepts");

    let reached = common::eventually(|| async {
        h.lane.armed_devices().iter().any(|id| id == SECOND_CAMERA)
    })
    .await;
    assert!(
        reached,
        "camera B's arm ran while camera A's was still hanging: {:?}",
        h.lane.armed_devices()
    );
}

#[tokio::test]
async fn a_refused_fast_lane_source_falls_back_in_the_same_pass() {
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;
    h.go2rtc.set_put_refused_marker(Some("nazar-fake.sdp"));
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");

    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the cloud path served this pass");
    let puts = h.go2rtc.puts();
    assert!(
        puts.iter()
            .any(|(_, source)| source.contains("nazar-fake.sdp")),
        "the fast-lane source was offered and refused: {puts:?}"
    );
    assert!(
        puts.iter().any(|(_, source)| is_cloud_source(source)),
        "and the cloud source took the same pass: {puts:?}"
    );
    assert!(
        !h.lane.session().alive(),
        "the refused session was ended, not retried blindly"
    );
    assert_eq!(h.lane.arms(), 1, "one arm, one cloud fallback");
}

#[tokio::test]
async fn control_rides_the_engine_when_armed_and_rest_otherwise() {
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;
    mount_control(&h.qubo, vec!["10,10", "20,15", "20,15"]).await;
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");
    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the fast lane armed");
    assert!(
        h.go2rtc
            .puts()
            .iter()
            .any(|(_, source)| source.contains("nazar-fake.sdp")),
        "the session's own source reached go2rtc"
    );

    let response = reqwest::Client::new()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");
    assert_eq!(response.status(), reqwest::StatusCode::OK);

    let controls = h.lane.session().controls();
    assert_eq!(controls.len(), 1, "the move rode the data channel");
    assert_eq!(
        controls[0]["command"]["devices"]["services"]["panTiltControl"]["attributes"]["horizontalVerticalPostion"],
        json!("20,15"),
        "the envelope carries the same absolute position the REST path writes"
    );
    assert_eq!(
        controls[0]["command"]["devices"]["handleName"],
        json!(CONTROLLER),
        "handleName is the camera's controllerUUID, as the captured frame carries it"
    );
    assert_eq!(
        controls[0]["command"]["devices"]["services"]["panTiltControl"]["instanceId"],
        json!(0),
        "instanceId sits with the service, the app's own absolute builder's placement"
    );

    assert_envelope_skeleton(&controls[0]);
    assert!(
        command_bodies(&h.qubo).await.is_empty(),
        "no REST write went out while the session was armed"
    );

    // Without the fast lane, the same call is the REST write as before.
    let rest = harness(
        FakeLane::new(),
        Vec::new(),
        Duration::from_secs(15),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;
    mount_control(&rest.qubo, vec!["10,10", "20,15", "20,15"]).await;
    rest.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");
    let up = common::eventually(|| async { is_live(&rest.supervisor).await }).await;
    assert!(up, "the cloud path serves the stream");
    assert_eq!(rest.lane.arms(), 0, "a camera not in prefer_p2p never arms");

    let response = reqwest::Client::new()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", rest.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bodies = command_bodies(&rest.qubo).await;
    assert_eq!(bodies.len(), 1, "the REST write went out");
    assert_eq!(
        bodies[0]["payload"]["devices"]["services"]["panTiltControl"]["attributes"]["horizontalVerticalPostion"],
        json!("20,15"),
        "the REST write carries the target position"
    );
}

#[tokio::test]
async fn an_unlanded_armed_move_reports_not_applied() {
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;
    // The shadow never reaches the target: the read-back must say so even
    // though the write itself rode the engine.
    mount_control(&h.qubo, vec!["10,10"]).await;
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");
    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the fast lane armed");

    let response = reqwest::Client::new()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["ok"], json!(false), "an unmoved camera is not ok");
    assert_eq!(
        body["reason"],
        json!("not applied"),
        "the read-back's verdict is the contract's"
    );
    assert_eq!(
        body["position"],
        json!({ "x": 10, "y": 10 }),
        "the last position the shadow reported comes back"
    );
    assert_eq!(
        h.lane.session().controls().len(),
        1,
        "the write still rode the engine"
    );
}

#[tokio::test]
async fn a_dead_sessions_camera_routes_ptz_back_to_rest() {
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_secs(30),
        Duration::from_secs(600),
    )
    .await;
    mount_control(&h.qubo, vec!["10,10", "20,15"]).await;
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");
    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the fast lane armed");

    // The session dies while its entry is still in the map (the lookup
    // filters on alive; nothing has pruned it yet).
    h.lane.session().shutdown();

    let response = reqwest::Client::new()
        .post(format!("{}/api/cameras/{DEVICE_ID}/ptz", h.base))
        .json(&json!({ "dx": 10, "dy": 5 }))
        .send()
        .await
        .expect("the call");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let bodies = command_bodies(&h.qubo).await;
    assert_eq!(
        bodies.len(),
        1,
        "the REST write went out for the camera of a dead session"
    );
    assert_eq!(
        bodies[0]["payload"]["devices"]["services"]["panTiltControl"]["attributes"]["horizontalVerticalPostion"],
        json!("20,15"),
        "the REST write carries the target position"
    );
    assert_eq!(
        h.lane.session().controls().len(),
        0,
        "no engine control for the dead session"
    );
}

#[tokio::test]
async fn a_second_stream_reuses_the_camera_session() {
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_millis(400),
        Duration::from_secs(600),
    )
    .await;

    // Both qualities ask at once: the two loops race into the arm-or-reuse
    // decision, and exactly one arm may come out of it.
    let (high, low) = tokio::join!(
        h.supervisor.ensure(DEVICE_ID, Quality::High, false),
        h.supervisor.ensure(DEVICE_ID, Quality::Low, false),
    );
    high.expect("ensure accepts");
    low.expect("ensure accepts");

    let both_live = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Live { .. }
        ) && matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
            StreamState::Live { .. }
        )
    })
    .await;
    assert!(both_live, "both streams go live");
    assert_eq!(
        h.lane.arms(),
        1,
        "one fast-lane session per camera, never two"
    );
    let puts = h.go2rtc.puts();
    let high = format!("{DEVICE_ID}-high");
    let low = format!("{DEVICE_ID}-low");
    assert!(
        puts.iter()
            .any(|(stream, source)| stream == &high && source.contains("nazar-fake.sdp")),
        "the high stream is fed by the one session: {puts:?}"
    );
    assert!(
        puts.iter()
            .any(|(stream, source)| stream == &low && source == &server::go2rtc::low_source(&high)),
        "the low rides the high inside go2rtc: one session reader serves the camera: {puts:?}"
    );
}

#[tokio::test]
async fn a_refused_session_source_falls_back_and_ends_the_session() {
    // source_for refused on a live session (a local reader failure, not a
    // silent pump): the pass takes cloud, and the session is ended - not
    // dropped from the map with its pump still running unreferenced.
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_millis(400),
        Duration::from_secs(600),
    )
    .await;
    h.lane.session().refuse_source();
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");

    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the cloud path served the pass");
    assert!(
        !h.lane.session().alive(),
        "the refused session was ended, not left running unreferenced"
    );
    assert_eq!(h.lane.arms(), 1, "one arm, one cloud fallback");

    // The next pass arms afresh: the session was retired, not leaked, and
    // this failure does not bench the lane.
    h.lane.session().allow_source();
    let rearmed = common::eventually(|| async { h.lane.arms() == 2 }).await;
    assert!(rearmed, "the next pass arms a fresh session");
}

#[tokio::test]
async fn a_media_silent_session_benches_the_lane_and_the_pass_takes_the_cloud_path() {
    // The session died the way a media-silent one does - alive false while
    // its entry is still mapped - and the next pass must not re-arm into
    // it: the lane is benched and the cloud path serves.
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_millis(400),
        Duration::from_secs(600),
    )
    .await;
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");
    let armed = common::eventually(|| async {
        h.go2rtc
            .puts()
            .iter()
            .any(|(_, source)| source.contains("nazar-fake.sdp"))
    })
    .await;
    assert!(armed, "the fast lane served the first pass");

    h.lane.session().shutdown();
    let cloud = common::eventually(|| async {
        h.go2rtc
            .puts()
            .iter()
            .any(|(name, source)| name == &format!("{DEVICE_ID}-high") && is_cloud_source(source))
    })
    .await;
    assert!(
        cloud,
        "the pass after the silent session mints the cloud source"
    );
    // The bench holds across further passes' worth of cadence.
    tokio::time::sleep(Duration::from_millis(900)).await;
    assert_eq!(
        h.lane.arms(),
        1,
        "the silent session is benched for its cooldown, never re-armed inside it"
    );
}

#[tokio::test]
async fn a_benched_lane_is_retried_once_its_cooldown_elapses() {
    // After the silence one pass takes cloud; once the cooldown elapses
    // the lane is tried again rather than staying benched forever.
    let h = harness(
        FakeLane::new(),
        vec![DEVICE_ID.to_owned()],
        Duration::from_secs(15),
        Duration::from_millis(300),
        Duration::from_millis(100),
    )
    .await;
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure accepts");
    let armed = common::eventually(|| async { h.lane.arms() == 1 }).await;
    assert!(armed, "the fast lane served the first pass");

    h.lane.session().shutdown();
    let cloud = common::eventually(|| async {
        h.go2rtc
            .puts()
            .iter()
            .any(|(_, source)| is_cloud_source(source))
    })
    .await;
    assert!(cloud, "the benched pass takes the cloud path");

    let retried = common::eventually(|| async { h.lane.arms() == 2 }).await;
    assert!(retried, "the lane is retried once its cooldown elapses");
}
