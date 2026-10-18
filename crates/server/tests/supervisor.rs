//! Supervisor tests: mint-once reuse, session loss, and mint failures.
//!
//! The Qubo side answers with the captured shapes: the 401 body is the
//! live capture in `docs/qubo-vod-verification.md`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{BASE_URL, Quality, QuboClient, ServiceProvider, Session, SessionStore};
use server::go2rtc::Go2rtc;
use server::supervisor::{Pacing, StreamState, Supervisor, stream_name, vod_name};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const REFRESH_PATH: &str =
    "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/users/user-uuid-1/auth/refresh";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";

static NEXT_SESSION_FILE: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    supervisor: Supervisor,
    qubo: MockServer,
    go2rtc: common::FakeApi,
    sessions_path: PathBuf,
}

fn pacing(remint: Duration) -> Pacing {
    Pacing {
        remint,
        retry_base: Duration::from_millis(20),
        retry_max: Duration::from_millis(200),
        mint_settle: Duration::from_millis(50),
        discovery_retry: Duration::from_millis(50),
        stream_idle: Duration::from_secs(30),
        connect_grace: Duration::from_millis(50),
        connect_window: Duration::from_millis(300),
        liveness_poll: Duration::from_millis(50),
        touch_ttl: Duration::from_millis(300),
        arm_timeout: Duration::from_secs(15),
        lane_cooldown: Duration::from_secs(600),
    }
}

async fn harness(pacing: Pacing) -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    let sessions_path = temp_path();
    let store = SessionStore::new(&sessions_path);
    store
        .save(&session("token-abc"))
        .expect("the session saves");

    let client = QuboClient::new(qubo.uri().parse().expect("a url"), SP.into());
    let supervisor = Supervisor::new(
        client,
        store,
        Go2rtc::connect(go2rtc.base.clone()).expect("a client"),
        pacing,
    );
    Harness {
        supervisor,
        qubo,
        go2rtc,
        sessions_path,
    }
}

fn session(token: &str) -> Session {
    Session {
        access_token: token.to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
        refresh_token: None,
    }
}

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nazar-supervisor-{}-{}.json",
        std::process::id(),
        NEXT_SESSION_FILE.fetch_add(1, Ordering::SeqCst)
    ))
}

fn mint_body(token: &str) -> String {
    json!({
        "streamURL": format!("rtsps://wowza2.platform.quboworld.com:443/hero-iot-live/{DEVICE_ID}_CAMERA_VOD0?token={token}"),
        "sessionId": "session-1",
    })
    .to_string()
}

fn mint_mock(status: u16, body: &str) -> Mock {
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(ResponseTemplate::new(status).set_body_string(body.to_owned()))
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
        json!({ "data": [{ "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "HCP01" }] })
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

async fn mint_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|request| request.url.path() == MINT_PATH)
        .count()
}

async fn is_live(supervisor: &Supervisor) -> bool {
    matches!(supervisor.status(DEVICE_ID).await, StreamState::Live { .. })
}

/// Whether the wire's last streams operation was a release: a delete with
/// no PUT after it. Every re-mint's swap deletes and then puts, so a swap
/// never reads as one.
fn released_last(h: &Harness) -> bool {
    h.go2rtc
        .ops()
        .last()
        .is_some_and(|op| op.starts_with("delete:"))
}

/// The last streams operation for `name`, as `put:<name>` or `delete:<name>`.
fn last_op(ops: &[String], name: &str) -> Option<String> {
    ops.iter().rev().find(|op| op.ends_with(name)).cloned()
}

/// The stream's state once its loop has taken a position: the first mint
/// runs in the loop, so `Waking` is what a fresh registration reads.
async fn settled(supervisor: &Supervisor) -> StreamState {
    let mut state = supervisor.status(DEVICE_ID).await;
    for _ in 0..50 {
        if !matches!(state, StreamState::Waking) {
            return state;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        state = supervisor.status(DEVICE_ID).await;
    }
    state
}

#[tokio::test]
async fn ensure_live_mints_once_and_reuses_the_stream() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(200, &mint_body("first")).mount(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the first call starts the stream");
    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the stream goes live");
    assert_eq!(mint_count(&h.qubo).await, 1, "the first call mints once");
    assert_eq!(h.go2rtc.puts().len(), 1, "the source reached go2rtc");
    let (name, source) = h.go2rtc.puts().remove(0);
    assert_eq!(
        name,
        stream_name(DEVICE_ID, Quality::High),
        "the stream carries the device and its quality"
    );
    assert!(
        source.starts_with("ffmpeg:") && source.contains("#input=nazar#video=nazar-h264"),
        "the minted URL rides the transcoding template: {source}"
    );

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the second call reuses the stream");
    assert_eq!(
        mint_count(&h.qubo).await,
        1,
        "a second ensure_live does not mint again"
    );
    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "a second ensure_live does not touch go2rtc"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn an_expired_session_degrades_with_the_exact_reason_and_a_fresh_login_recovers() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(200, &mint_body("first"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    mint_mock(401, include_str!("fixtures/error_auth_002.json"))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&h.qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .and(header("authorization", "Bearer token-new"))
        .respond_with(ResponseTemplate::new(200).set_body_string(mint_body("second")))
        .with_priority(3)
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream starts");
    let up = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(up, "the stream goes live");

    let degraded = common::eventually(|| async { !is_live(&h.supervisor).await }).await;
    assert!(degraded, "the expired session takes the stream down");
    match h.supervisor.status(DEVICE_ID).await {
        StreamState::Degraded { reason } => {
            assert_eq!(
                reason, "auth error: User Token expired, Access denied",
                "the API's reason survives verbatim"
            );
        }
        other => panic!("expected Degraded, got {other:?}"),
    }

    // The family logs in again; the next retry must pick the new session up.
    SessionStore::new(&h.sessions_path)
        .save(&session("token-new"))
        .expect("the fresh session saves");

    let recovered = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(recovered, "a fresh login heals the stream");

    let puts = h.go2rtc.puts();
    assert!(puts.len() >= 2, "go2rtc was handed a new source");
    assert!(
        puts.iter()
            .all(|(name, _)| name == &stream_name(DEVICE_ID, Quality::High)),
        "every swap keeps the stream name"
    );
    assert_ne!(
        puts[0].1,
        puts.last().expect("recorded puts").1,
        "the source itself changed"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn a_failing_mint_degrades_and_keeps_retrying() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(500, "").mount(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("ensure_live still returns; the state carries the failure");

    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { .. }
        )
    })
    .await;
    assert!(degraded, "a mint failure degrades the stream");
    match h.supervisor.status(DEVICE_ID).await {
        StreamState::Degraded { reason } => {
            assert_eq!(
                reason, "http error: 500 Internal Server Error",
                "a 5xx reason names the failure exactly"
            );
        }
        other => panic!("expected Degraded, got {other:?}"),
    }

    let retried = common::eventually(|| async { mint_count(&h.qubo).await >= 3 }).await;
    assert!(retried, "the supervisor keeps retrying");
    assert_eq!(
        h.go2rtc.puts().len(),
        0,
        "nothing reaches go2rtc without a mint"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn a_manual_retry_forces_a_remint_and_clears_the_login_flag() {
    // Everything paced long: only the wake signal can deliver a re-mint
    // inside the test's window, so the retry path is what is measured.
    let h = harness(Pacing {
        remint: Duration::from_secs(30),
        retry_base: Duration::from_secs(30),
        retry_max: Duration::from_secs(30),
        mint_settle: Duration::from_millis(50),
        discovery_retry: Duration::from_secs(30),
        stream_idle: Duration::from_secs(30),
        connect_grace: Duration::from_millis(50),
        connect_window: Duration::from_millis(300),
        liveness_poll: Duration::from_millis(50),
        touch_ttl: Duration::from_millis(300),
        arm_timeout: Duration::from_secs(15),
        lane_cooldown: Duration::from_secs(600),
    })
    .await;
    mount_discovery(&h.qubo).await;
    mint_mock(401, include_str!("fixtures/error_auth_002.json"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("starts");

    assert!(
        matches!(settled(&h.supervisor).await, StreamState::Degraded { .. }),
        "the refused mint degrades the stream"
    );
    assert!(
        h.supervisor.needs_login(),
        "the refusal is remembered so the UI can route to login"
    );

    SessionStore::new(&h.sessions_path)
        .save(&session("token-new"))
        .expect("a fresh session");
    mint_mock(200, &mint_body("second"))
        .with_priority(2)
        .mount(&h.qubo)
        .await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, true)
        .await
        .expect("retry");
    let recovered = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(
        recovered,
        "Try again re-mints immediately instead of waiting out the backoff"
    );
    assert!(
        !h.supervisor.needs_login(),
        "a healthy mint clears the login flag"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn a_cancelled_request_does_not_strand_the_stream() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(mint_body("slow"))
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");

    // The browser gives up on the request while the first mint is still in
    // flight. The stream's loop owns that mint, so it must finish: a
    // cancelled request never leaves a camera reading offline, and never
    // orphans the source it started.
    let quick = tokio::time::timeout(
        Duration::from_millis(100),
        h.supervisor.ensure(DEVICE_ID, Quality::High, false),
    )
    .await;
    assert!(
        quick.is_ok(),
        "the request answers as soon as the stream is registered"
    );
    assert_eq!(
        h.supervisor.status(DEVICE_ID).await,
        StreamState::Waking,
        "and the camera reads as starting, never as offline"
    );

    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(
        live,
        "the stream goes live once the mint the request abandoned lands"
    );
    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "and exactly one source reached go2rtc"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn a_put_go2rtc_did_not_take_is_degraded_not_live() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(200, &mint_body("first")).mount(&h.qubo).await;
    h.go2rtc.set_streams_visible(false);
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("starts");

    match settled(&h.supervisor).await {
        StreamState::Degraded { reason } => {
            assert_eq!(
                reason, "go2rtc did not take the stream",
                "an acked PUT with no producer is not Live"
            );
        }
        other => panic!("expected Degraded, got {other:?}"),
    }
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn a_failed_swap_never_leaks_the_stream_token() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(200, &mint_body("sekrit-token"))
        .mount(&h.qubo)
        .await;
    h.go2rtc.set_put_status(400);
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("starts");

    // The first mint runs in the stream's own loop.
    let mut outcome = StreamState::Offline;
    for _ in 0..50 {
        outcome = h.supervisor.status(DEVICE_ID).await;
        if matches!(outcome, StreamState::Degraded { .. }) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    match outcome {
        StreamState::Degraded { reason } => {
            assert!(
                !reason.contains("sekrit-token"),
                "the browser's reason carries no mint token: {reason}"
            );
            assert!(
                reason.contains("token=<redacted>"),
                "the token is visibly redacted: {reason}"
            );
            assert!(
                reason.contains("#input=nazar#video=nazar-h264"),
                "the redaction stops at the token and keeps the tail: {reason}"
            );
        }
        other => panic!("expected Degraded, got {other:?}"),
    }
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn discovery_backs_off_between_tries() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;

    h.supervisor.refresh_devices().await.expect("discovery");
    assert!(
        !h.supervisor.discovery_due(),
        "a just-run discovery is not due again"
    );

    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        h.supervisor.discovery_due(),
        "the backoff expires and recovery can be noticed again"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

fn durable_session(access: &str, refresh: &str) -> Session {
    Session {
        access_token: access.to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
        refresh_token: Some(refresh.to_owned()),
    }
}

/// A refusal on a session someone has already healed spends nothing: the
/// boot pair and the feed race here by construction, and a second spend
/// of the same pair is a refusal the login flag must not ride.
#[tokio::test]
async fn a_refusal_on_an_already_healed_session_spends_nothing() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    SessionStore::new(&h.sessions_path)
        .save(&durable_session("token-old", "refresh-1"))
        .expect("the durable session saves");
    // The old token's refusal lands slowly, so the heal can land while
    // the call is in flight.
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer token-old"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_delay(Duration::from_millis(200))
                    .set_body_string(include_str!("fixtures/error_auth_002.json")),
            )
            .with_priority(2)
            .mount(&h.qubo)
            .await;
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer token-new"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(if route == UNITS_PATH {
                    json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string()
                } else {
                    json!({ "data": [{ "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "HCP01" }] })
                        .to_string()
                }),
            )
            .with_priority(1)
            .mount(&h.qubo)
            .await;
    }
    // Any spend at all would be refused: the pin is that none happens.
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .expect(0)
        .mount(&h.qubo)
        .await;

    let supervisor = h.supervisor.clone();
    let discover = tokio::spawn(async move { supervisor.refresh_devices().await });
    // The other caller heals the store while the refusal is in flight.
    tokio::time::sleep(Duration::from_millis(80)).await;
    SessionStore::new(&h.sessions_path)
        .save(&durable_session("token-new", "refresh-2"))
        .expect("the heal lands");
    let devices = discover
        .await
        .expect("the task joins")
        .expect("the call heals");
    assert!(!devices.is_empty(), "the stored heal's devices arrive");
    assert!(
        !h.supervisor.needs_login(),
        "no login flag on a session that was healed mid-flight"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Two callers refused on the same token spend once: the boot pair does
/// this by construction, and the loser must ride the winner's heal, not
/// a second POST the gateway may answer with a rotation.
#[tokio::test]
async fn two_refusals_on_one_token_spend_once() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    SessionStore::new(&h.sessions_path)
        .save(&durable_session("token-old", "refresh-1"))
        .expect("the durable session saves");
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer token-old"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_delay(Duration::from_millis(100))
                    .set_body_string(include_str!("fixtures/error_auth_002.json")),
            )
            .with_priority(2)
            .mount(&h.qubo)
            .await;
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer token-new"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(if route == UNITS_PATH {
                    json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string()
                } else {
                    json!({ "data": [{ "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "HCP01" }] })
                        .to_string()
                }),
            )
            .with_priority(1)
            .mount(&h.qubo)
            .await;
    }
    // Exactly one spend, or the second POST would be the loser's rot.
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({ "accessToken": "token-new", "refreshToken": "refresh-2" }).to_string(),
        ))
        .expect(1)
        .mount(&h.qubo)
        .await;

    let one = h.supervisor.clone();
    let two = h.supervisor.clone();
    let (a, b) = tokio::join!(async move { one.refresh_devices().await }, async move {
        two.refresh_devices().await
    },);
    assert!(a.is_ok() && b.is_ok(), "both callers heal: {a:?} {b:?}");
    assert!(
        !h.supervisor.needs_login(),
        "the loser rode the winner's heal, not a refusal"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Warm on access: every camera starts minting in the background, and the
/// account's non-cameras are left alone.
#[tokio::test]
async fn warm_all_starts_every_camera_in_the_background() {
    const SECOND: &str = "c0000000-0000-4000-8000-000000000002";
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_json(
        &h.qubo,
        UNITS_PATH,
        json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string(),
    )
    .await;
    mount_json(
        &h.qubo,
        DEVICES_PATH,
        json!({ "data": [
            { "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "ptzCamera3MP", "aModelNo": "HCP01" },
            { "deviceUUID": SECOND, "deviceName": "Gate", "deviceType": "ptzCamera3MP", "aModelNo": "HCP01" },
            { "deviceUUID": "air-1", "deviceName": "Purifier", "deviceType": "smartAirPurifierX4", "aModelNo": "HPH04A" },
        ] })
        .to_string(),
    )
    .await;
    mint_mock(200, &mint_body("one")).mount(&h.qubo).await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH.replace(DEVICE_ID, SECOND)))
        .respond_with(ResponseTemplate::new(200).set_body_string(mint_body("two")))
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor.warm_all().await;
    let warmed = common::eventually(|| async {
        let puts = h.go2rtc.puts();
        puts.iter()
            .any(|(name, _)| name == &stream_name(DEVICE_ID, Quality::High))
            && puts
                .iter()
                .any(|(name, _)| name == &stream_name(SECOND, Quality::High))
    })
    .await;
    assert!(
        warmed,
        "every camera started minting: {:?}",
        h.go2rtc.puts()
    );
    assert!(
        !h.go2rtc
            .puts()
            .iter()
            .any(|(name, _)| name.contains("air-1")),
        "the account's non-cameras are left alone"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Warm on access lands on the first discovery that succeeds: a boot
/// whose own discovery failed still warms when a later poll's lands.
#[tokio::test]
async fn a_late_discovery_still_warms_the_house() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mint_mock(200, &mint_body("one")).mount(&h.qubo).await;
    h.supervisor.warm_on_discovery();

    // The boot's discovery fails: no mocks are mounted yet.
    h.supervisor
        .refresh_devices()
        .await
        .expect_err("an unmocked discovery fails");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        h.go2rtc.puts().is_empty(),
        "nothing warms without a device list"
    );

    // A later poll's discovery lands: the warmth follows it.
    mount_discovery(&h.qubo).await;
    h.supervisor
        .refresh_devices()
        .await
        .expect("the late discovery lands");
    let warmed = common::eventually(|| async { !h.go2rtc.puts().is_empty() }).await;
    assert!(
        warmed,
        "the first successful discovery starts the house: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The warmth is permanent: a warmed stream re-mints on its cadence for
/// as long as the session lives, and nobody asking for it parks nothing.
#[tokio::test]
async fn a_warmed_stream_never_parks() {
    let h = harness(Pacing {
        remint: Duration::from_millis(200),
        stream_idle: Duration::from_millis(100),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    mint_mock(200, &mint_body("one")).mount(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor.warm_all().await;
    let minted = common::eventually(|| async { !h.go2rtc.puts().is_empty() }).await;
    assert!(minted, "the warm started the mint");
    let started = h.go2rtc.puts().len();

    // Well past both the asking and the old warm windows: still up, still
    // re-minting on the cadence.
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(
        !released_last(&h),
        "a warmed stream is never parked for going unasked: {:?}",
        h.go2rtc.ops()
    );
    assert!(
        h.go2rtc.puts().len() > started,
        "and it keeps re-minting on its cadence: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Warming can never fail a login: a refused mint is the stream's state,
/// not the caller's error.
#[tokio::test]
async fn a_refused_warm_degrades_and_returns() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(500, "").mount(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor.warm_all().await;
    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { .. }
        )
    })
    .await;
    assert!(
        degraded,
        "the refusal surfaces on the stream, and warm_all returned: {:?}",
        h.supervisor.status(DEVICE_ID).await
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A 401 on discovery spends the stored refresh token once and lists
/// again with the fresh session: an idle hub's hour-expired access token
/// must not strand the house at the login screen while a 180-day refresh
/// sits in the store.
#[tokio::test]
async fn a_refused_discovery_spends_the_refresh_token_and_heals() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    SessionStore::new(&h.sessions_path)
        .save(&durable_session("token-old", "refresh-1"))
        .expect("the durable session saves");
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer token-old"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(include_str!("fixtures/error_auth_002.json")),
            )
            .with_priority(2)
            .mount(&h.qubo)
            .await;
    }
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer token-new"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(if route == UNITS_PATH {
                    json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string()
                } else {
                    json!({ "data": [{ "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "HCP01" }] })
                        .to_string()
                }),
            )
            .with_priority(1)
            .mount(&h.qubo)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({ "accessToken": "token-new", "refreshToken": "refresh-2" }).to_string(),
        ))
        .expect(1)
        .mount(&h.qubo)
        .await;

    let devices = h
        .supervisor
        .refresh_devices()
        .await
        .expect("discovery heals");
    assert!(
        !devices.is_empty(),
        "the devices came from the fresh session"
    );
    assert!(
        !h.supervisor.needs_login(),
        "a healed discovery never raises the login flag"
    );
    let stored = SessionStore::new(&h.sessions_path)
        .load()
        .expect("the session loads")
        .expect("a session is stored");
    assert_eq!(stored.access_token, "token-new");
    assert_eq!(stored.refresh_token.as_deref(), Some("refresh-2"));
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A refused refresh is the session's end, wherever the refusal lands:
/// discovery degrades to the login flag and the stored session is left
/// untouched.
#[tokio::test]
async fn a_refused_refresh_on_discovery_raises_the_login_flag() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    SessionStore::new(&h.sessions_path)
        .save(&durable_session("token-old", "refresh-1"))
        .expect("the durable session saves");
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(include_str!("fixtures/error_auth_002.json")),
            )
            .mount(&h.qubo)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .expect(1)
        .mount(&h.qubo)
        .await;

    h.supervisor
        .refresh_devices()
        .await
        .expect_err("the refused refresh is the end");
    assert!(
        h.supervisor.needs_login(),
        "a discovery that cannot heal routes to login"
    );
    let stored = SessionStore::new(&h.sessions_path)
        .load()
        .expect("the session loads")
        .expect("a session is stored");
    assert_eq!(
        stored.access_token, "token-old",
        "a refused refresh does not clobber the stored session"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A 401 mid-mint spends the stored refresh token once and runs the mint
/// again with the fresh one: the durable session heals with no phone.
#[tokio::test]
async fn a_mid_mint_401_spends_the_refresh_token_and_heals() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    mount_discovery(&h.qubo).await;
    SessionStore::new(&h.sessions_path)
        .save(&durable_session("token-old", "refresh-1"))
        .expect("the durable session saves");
    mint_mock(200, &mint_body("first"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    mint_mock(401, include_str!("fixtures/error_auth_002.json"))
        .up_to_n_times(1)
        .with_priority(2)
        .mount(&h.qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .and(header("authorization", "Bearer token-new"))
        .respond_with(ResponseTemplate::new(200).set_body_string(mint_body("second")))
        .with_priority(3)
        .mount(&h.qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!({ "accessToken": "token-new", "refreshToken": "refresh-2" }).to_string(),
        ))
        .expect(1)
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream starts");
    let healed = common::eventually(|| async {
        let stored = SessionStore::new(&h.sessions_path)
            .load()
            .expect("the session loads")
            .expect("a session is stored");
        is_live(&h.supervisor).await
            && !h.supervisor.needs_login()
            && stored.access_token == "token-new"
            && stored.refresh_token.as_deref() == Some("refresh-2")
    })
    .await;
    assert!(healed, "the fresh session is stored and the stream is up");

    let requests = h.qubo.received_requests().await.expect("recorded requests");
    let refreshes: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == REFRESH_PATH)
        .collect();
    assert_eq!(
        refreshes.len(),
        1,
        "the refresh token is spent exactly once"
    );
    let body: Value = serde_json::from_slice(&refreshes[0].body).expect("a json body");
    assert_eq!(body["accessToken"], json!("token-old"));
    assert_eq!(body["refreshToken"], json!("refresh-1"));
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A refresh the gateway refuses is the session's end: the stream degrades
/// and the login flag rises, and the stored session is left untouched.
#[tokio::test]
async fn a_refused_refresh_surfaces_needs_login() {
    let h = harness(pacing(Duration::from_millis(120))).await;
    mount_discovery(&h.qubo).await;
    SessionStore::new(&h.sessions_path)
        .save(&durable_session("token-old", "refresh-1"))
        .expect("the durable session saves");
    mint_mock(401, include_str!("fixtures/error_auth_002.json"))
        .mount(&h.qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(REFRESH_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let refused = common::eventually(|| async { h.supervisor.needs_login() }).await;
    assert!(refused, "a refused refresh ends in the login flag");
    assert!(
        matches!(settled(&h.supervisor).await, StreamState::Degraded { .. }),
        "and the stream is degraded, not pretending"
    );
    let stored = SessionStore::new(&h.sessions_path)
        .load()
        .expect("the session loads")
        .expect("a session is stored");
    assert_eq!(
        stored.access_token, "token-old",
        "a refused refresh does not clobber the stored session"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Mount a mint ladder: each attempt answers a different URL, as a fresh
/// mint does.
async fn mount_mint_ladder(server: &MockServer, tokens: &[&str]) {
    for (index, token) in tokens.iter().enumerate() {
        mint_mock(200, &mint_body(token))
            .up_to_n_times(1)
            .with_priority(u8::try_from(index + 1).expect("a priority"))
            .mount(server)
            .await;
    }
}

/// A re-mint replaces a URL whose session is spent: the predecessor's
/// chain must go with it, or its ffmpeg keeps encoding a session the cloud
/// still serves (measured on the hub: a fresh pair beside a fifty-minute
/// one, seven chains where four cameras live).
#[tokio::test]
async fn a_remint_reaps_the_predecessor_before_the_swap() {
    let h = harness(Pacing {
        connect_window: Duration::from_secs(5),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two", "three"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(reminted, "the loop re-mints: {:?}", h.go2rtc.puts());
    let ops = h.go2rtc.ops();
    assert!(
        ops.windows(2)
            .any(|pair| pair[0] == format!("delete:{name}") && pair[1] == format!("put:{name}")),
        "the swap reaps the predecessor's chain before its PUT: {ops:?}"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A watched producer whose source never connects is re-minted with a
/// fresh URL, fast: the URL is one-shot, and waiting for the four-minute
/// remint would leave a viewer staring at a dead stream.
#[tokio::test]
async fn a_stalled_producer_is_reminted_with_a_fresh_url() {
    // A window far past the assertions: this test pins the re-minting, not
    // the degradation.
    let h = harness(Pacing {
        connect_window: Duration::from_secs(5),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two", "three"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    // A viewer on the picture: that is what makes the stall the loop's
    // business.
    h.go2rtc.set_viewers(&name, 1);

    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 3 }).await;
    assert!(reminted, "the loop keeps minting: {:?}", h.go2rtc.puts());

    let urls: Vec<String> = h.go2rtc.puts().iter().map(|(_, src)| src.clone()).collect();
    assert_ne!(urls[0], urls[1], "the second attempt carries a fresh URL");
    assert_ne!(urls[1], urls[2], "and the third never reuses either");
    assert_eq!(
        h.supervisor.status(DEVICE_ID).await,
        StreamState::Waking,
        "a starved viewer reads waking, not live"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The warm-live interaction: a stream whose producer died is kept, never
/// released, however long it sits unasked - and when a viewer arrives at
/// the dead picture, the stall machinery re-mints it with no release
/// racing the heal.
#[tokio::test]
async fn a_dead_producer_re_mints_without_a_release_racing_it() {
    let h = harness(Pacing {
        connect_window: Duration::from_secs(5),
        stream_idle: Duration::from_millis(60),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let landed = common::eventually(|| async { !h.go2rtc.puts().is_empty() }).await;
    assert!(landed, "the first mint landed");
    let started = h.go2rtc.puts().len();

    // Nobody asks again and the producer never connected: far past the old
    // idle window, the entry stays and nothing is released.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        !released_last(&h),
        "an unasked dead stream is kept, not released: {:?}",
        h.go2rtc.ops()
    );
    assert_ne!(
        h.supervisor.status(DEVICE_ID).await,
        StreamState::Offline,
        "and its entry stays tracked"
    );

    // A viewer arrives at the dead picture: the stall machinery re-mints
    // on its own, with no release racing the heal.
    h.go2rtc.set_viewers(&name, 1);
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() > started }).await;
    assert!(
        reminted,
        "the arrival's stall run re-mints: {:?}",
        h.go2rtc.puts()
    );
    assert!(
        !released_last(&h),
        "no release raced the heal: {:?}",
        h.go2rtc.ops()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The state stays waking while a watched producer is away, and flips live
/// when a fresh mint's source connects.
#[tokio::test]
async fn the_state_stays_waking_until_the_producer_connects() {
    let h = harness(Pacing {
        connect_window: Duration::from_secs(5),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    let waking = common::eventually(|| async {
        h.supervisor.status(DEVICE_ID).await == StreamState::Waking
    })
    .await;
    assert!(waking, "a starved viewer reads waking");

    // The watch mints fresh while the producer is away: the URL it holds
    // is spent, so waiting out the next cadence would starve the viewer.
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(
        reminted,
        "a fresh URL went out while the producer was away: {:?}",
        h.go2rtc.puts()
    );

    h.go2rtc.connect_producer(&name);
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "the picture's return flips the state live");
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A watched producer that stops answering demotes the stream from live to
/// waking: the stale-live class this fix exists for.
#[tokio::test]
async fn a_stalled_producer_leaves_live_and_reads_waking() {
    // A window far enough away that the wake is what the assertions see.
    let h = harness(Pacing {
        connect_window: Duration::from_secs(5),
        ..pacing(Duration::from_millis(200))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "the stream is live first");

    h.go2rtc.set_viewers(&name, 1);
    h.go2rtc.disconnect_producer(&name);
    let waking = common::eventually(|| async {
        h.supervisor.status(DEVICE_ID).await == StreamState::Waking
    })
    .await;
    assert!(
        waking,
        "a producer that stops connecting leaves Live for Waking: {:?}",
        h.supervisor.status(DEVICE_ID).await
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A watched producer that takes its time is waited for through the grace:
/// the URL is not re-minted around a source that is merely slow.
#[tokio::test]
async fn a_slow_producer_connects_within_the_grace() {
    let h = harness(Pacing {
        connect_grace: Duration::from_millis(400),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.connect_after(&name, Duration::from_millis(100));

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    // The producer needs its moment; no re-mint may cut it short.
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "the slow source was never re-minted around: {:?}",
        h.go2rtc.puts()
    );
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "and it does land live");
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A viewer attaching after the URL's one dial - a reopen, a second device
/// - finds nothing on it, and the liveness watch mints fresh: the C1 repro
/// end to end, and the heal a person never has to ask for.
#[tokio::test]
async fn a_late_viewer_heals_through_a_fresh_mint() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");

    // The first viewer attaches and leaves: the URL is spent.
    h.go2rtc.set_viewers(&name, 1);
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "the first viewer's picture is up");
    h.go2rtc.set_viewers(&name, 0);

    // The late viewer: the spent URL cannot connect, so the watch mints
    // fresh, and the picture comes up on the second URL.
    h.go2rtc.set_viewers(&name, 1);
    let mints = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(mints, "a fresh mint went out: {:?}", h.go2rtc.puts());
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "and the late viewer's picture is up");
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A watcher starved past the window reads degraded with the reason - and
/// the state stays degraded across the loop's later re-mints, so the UI's
/// Try again survives them.
#[tokio::test]
async fn a_producer_that_never_connects_degrades_after_the_window() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "the producer has not connected"
        )
    })
    .await;
    assert!(degraded, "the window passes and the reason surfaces");

    let persisted = common::eventually(|| async {
        h.go2rtc.puts().len() >= 3
            && matches!(
                h.supervisor.status(DEVICE_ID).await,
                StreamState::Degraded { reason } if reason == "the producer has not connected"
            )
    })
    .await;
    assert!(
        persisted,
        "and a later re-mint does not launder it: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Past the window the reason sticks across further attempts: an attempt
/// that lands unconnected must not return the stream to waking, which
/// would take the UI's Try again away.
#[tokio::test]
async fn a_degraded_stream_stays_degraded_across_attempts() {
    let h = harness(Pacing {
        connect_window: Duration::from_millis(300),
        ..pacing(Duration::from_millis(200))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two", "three"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "the producer has not connected"
        )
    })
    .await;
    assert!(degraded, "the window passes and the reason surfaces");

    // Sample across a further full attempt: the state must hold, reason
    // and all - a mint landing mid-run must not launder it.
    let before = h.go2rtc.puts().len();
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert_eq!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded {
                reason: "the producer has not connected".to_owned()
            },
            "a later attempt must not launder the reason"
        );
    }
    assert!(
        h.go2rtc.puts().len() > before,
        "an attempt ran inside the sample window"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A mint that fails mid-starvation is another way the picture is dead,
/// not a new window: the starvation clock keeps running, so the reason
/// lands on its own deadline and the Try again affordance does not
/// flicker away on a blip.
#[tokio::test]
async fn a_mint_failure_mid_stall_does_not_restart_the_window() {
    let h = harness(Pacing {
        connect_window: Duration::from_millis(350),
        retry_base: Duration::from_millis(20),
        retry_max: Duration::from_millis(100),
        mint_settle: Duration::from_millis(50),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    // The first mint lands; every re-mint from then on is refused.
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(500, "").mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "http error: 500 Internal Server Error"
        )
    })
    .await;
    assert!(
        degraded,
        "the window lands on its own deadline and carries the mint's reason: {:?}",
        h.supervisor.status(DEVICE_ID).await
    );
    // The premise provably ran: the refused mints actually happened.
    assert!(
        mint_count(&h.qubo).await >= 2,
        "a refused mint ran mid-stall: {} mints",
        mint_count(&h.qubo).await
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A device the cloud reports offline can never serve the URL we hold:
/// the stale source is dropped, and the cloud's reason is what the tile
/// keeps saying instead of the generic producer line.
#[tokio::test]
async fn an_offline_mint_failure_drops_the_stale_source_and_keeps_the_reason() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    // The first mint lands; every re-mint is refused because the device
    // has gone offline in the cloud.
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(
        400,
        r#"{"code":"ERR_STM_010","message":"device is offline"}"#,
    )
    .mount(&h.qubo)
    .await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    let refused = common::eventually(|| async { mint_count(&h.qubo).await >= 2 }).await;
    assert!(refused, "the re-mint runs");

    let dropped = common::eventually(|| async {
        h.go2rtc
            .ops()
            .into_iter()
            .rev()
            .find(|op| op.ends_with(&name))
            .is_some_and(|op| op.starts_with("delete:"))
    })
    .await;
    assert!(dropped, "the stale source is dropped: {:?}", h.go2rtc.ops());

    let reason = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "device offline in the cloud"
        )
    })
    .await;
    assert!(
        reason,
        "the cloud reason is carried, not the generic line: {:?}",
        h.supervisor.status(DEVICE_ID).await
    );

    // The stall's own window must not launder it back to the generic line.
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "device offline in the cloud"
        ),
        "the reason survives the window re-assert: {:?}",
        h.supervisor.status(DEVICE_ID).await
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The tile rendition has no cloud URL of its own: when the offline
/// refusal drops the high's source, the low must stop dialling it too.
#[tokio::test]
async fn an_offline_drop_removes_the_low_chain() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(
        400,
        r#"{"code":"ERR_STM_010","message":"device is offline"}"#,
    )
    .mount(&h.qubo)
    .await;
    let high = stream_name(DEVICE_ID, Quality::High);
    let low = stream_name(DEVICE_ID, Quality::Low);
    h.go2rtc.disconnect_producer(&high);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the high stream registers");
    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the low chain registers");
    h.go2rtc.set_viewers(&high, 1);

    let dropped = common::eventually(|| async {
        let ops = h.go2rtc.ops();
        last_op(&ops, &high).is_some_and(|op| op.starts_with("delete:"))
            && last_op(&ops, &low).is_some_and(|op| op.starts_with("delete:"))
    })
    .await;
    assert!(
        dropped,
        "both chains are dropped off the offline high: {:?}",
        h.go2rtc.ops()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The low has no cloud URL of its own: after an offline drop, the next
/// successful mint must rebuild it off the recovered high.
#[tokio::test]
async fn a_mint_after_an_offline_drop_rebuilds_the_low_chain() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(
        400,
        r#"{"code":"ERR_STM_010","message":"device is offline"}"#,
    )
    .mount(&h.qubo)
    .await;
    let high = stream_name(DEVICE_ID, Quality::High);
    let low = stream_name(DEVICE_ID, Quality::Low);
    h.go2rtc.disconnect_producer(&high);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the high stream registers");
    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the low chain registers");
    h.go2rtc.set_viewers(&high, 1);

    // The rebuild is only meaningful after the drop: wait for both gone.
    let dropped = common::eventually(|| async {
        let ops = h.go2rtc.ops();
        last_op(&ops, &high).is_some_and(|op| op.starts_with("delete:"))
            && last_op(&ops, &low).is_some_and(|op| op.starts_with("delete:"))
    })
    .await;
    assert!(
        dropped,
        "the offline drop removes both first: {:?}",
        h.go2rtc.ops()
    );

    // The camera is back: the next mint rebuilds the high, and the
    // reconcile rebuilds the low off it.
    mint_mock(200, &mint_body("again"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    let rebuilt = common::eventually(|| async {
        let ops = h.go2rtc.ops();
        last_op(&ops, &high).is_some_and(|op| op.starts_with("put:"))
            && last_op(&ops, &low).is_some_and(|op| op.starts_with("put:"))
    })
    .await;
    assert!(
        rebuilt,
        "the next mint rebuilds the high and reconciles the low: {:?}",
        h.go2rtc.ops()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A transient refusal is not proof the URL is dead: the source stays
/// while the loop keeps its backoff.
#[tokio::test]
async fn a_transient_mint_failure_keeps_the_stale_source() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(500, "").mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "http error: 500 Internal Server Error"
        )
    })
    .await;
    assert!(degraded, "the refusal degrades the stream");
    let last = h
        .go2rtc
        .ops()
        .into_iter()
        .rev()
        .find(|op| op.ends_with(&name));
    assert_eq!(
        last,
        Some(format!("put:{name}")),
        "a transient refusal keeps the source: {:?}",
        h.go2rtc.ops()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A viewer's ask is what the server sees of a viewer directly: an attach
/// whose producer never starts registers no consumer at go2rtc (measured
/// live 2026-10-16), so the ask itself is the clock. A dead stream a
/// viewer is knocking on is re-minted with no consumer ever attached.
#[tokio::test]
async fn a_viewers_ask_heals_a_dead_stream_without_a_consumer() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.supervisor.note_viewer_touch(&name);

    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(
        reminted,
        "the ask sent a fresh URL out: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A tile asks for the low rendition, and the tile's ask counts for the
/// cloud stream that carries it.
#[tokio::test]
async fn a_tiles_ask_counts_for_its_cloud_stream() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.supervisor
        .note_viewer_touch(&stream_name(DEVICE_ID, Quality::Low));

    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(
        reminted,
        "the tile's ask sent a fresh URL out: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A playback stream's ask, and an ask that names nothing, leave the live
/// loop alone.
#[tokio::test]
async fn a_playback_ask_does_not_touch_the_live_stream() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.supervisor.note_viewer_touch(&vod_name(DEVICE_ID));
    h.supervisor.note_viewer_touch("nonsense");

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "nothing re-minted: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The asker can go away without the run dying: a tile unmounts its
/// player the moment the stream reads waking, so the ask expires - and
/// the run that ask started must still finish, at the producer or at the
/// window, or the reason would never land on the home grid.
#[tokio::test]
async fn a_stall_outlives_the_ask_that_started_it() {
    let h = harness(Pacing {
        touch_ttl: Duration::from_millis(100),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.supervisor.note_viewer_touch(&name);
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(reminted, "the ask heals first: {:?}", h.go2rtc.puts());

    // No further asks, ever: the run still reaches its window (300ms,
    // against an ask that expired at 100ms) and lands the reason.
    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "the producer has not connected"
        )
    })
    .await;
    assert!(
        degraded,
        "the run outlives the ask that started it: {:?}",
        h.supervisor.status(DEVICE_ID).await
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// An ask on a stream whose producer is up changes nothing: the connected
/// short-circuit comes first, or every ask would re-mint-churn and
/// falsely degrade a perfectly live stream.
#[tokio::test]
async fn a_connected_producer_ignores_a_fresh_ask() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "the attended stream is live");

    h.supervisor.note_viewer_touch(&name);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "a live stream is not re-minted around: {:?}",
        h.go2rtc.puts()
    );
    assert!(is_live(&h.supervisor).await, "and it stays live");
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A go2rtc that cannot answer, or that has lost the stream, is not a
/// recovery: the run stands, the reason stands, and no read failure
/// launders the state.
#[tokio::test]
async fn an_unreadable_or_missing_producer_does_not_heal() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);
    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { reason } if reason == "the producer has not connected"
        )
    })
    .await;
    assert!(degraded, "the window passes and the reason surfaces");

    // The API goes unreadable: no read, no recovery - and when it reads
    // again, the run is where it was, not restarted.
    h.go2rtc.set_streams_error(true);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { .. }
        ),
        "an unreadable go2rtc is not a recovery: {:?}",
        h.supervisor.status(DEVICE_ID).await
    );
    h.go2rtc.set_streams_error(false);
    assert!(
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { .. }
        ),
        "the sampling window opens on a degraded stream"
    );
    for _ in 0..8 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        assert!(
            matches!(
                h.supervisor.status(DEVICE_ID).await,
                StreamState::Degraded { .. }
            ),
            "the run survived the read failure, reason and all: {:?}",
            h.supervisor.status(DEVICE_ID).await
        );
    }

    // The stream entry is gone, as a go2rtc restart leaves it: still no
    // recovery - the producer certainly is not up. Sampled finely, because
    // the mint failures re-degrade the state and could mask a Live blip.
    h.go2rtc.set_streams_visible(false);
    for _ in 0..80 {
        tokio::time::sleep(Duration::from_millis(10)).await;
        assert!(
            !matches!(
                h.supervisor.status(DEVICE_ID).await,
                StreamState::Live { .. }
            ),
            "a lost entry is not a recovery: {:?}",
            h.supervisor.status(DEVICE_ID).await
        );
    }
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// narar's own consumer keeps the producer up between viewers: a client
/// that connects finds a picture already flowing, not a dial.
#[tokio::test]
async fn a_held_stream_stays_warm_without_viewers() {
    let mut h = harness(pacing(Duration::from_secs(30))).await;
    h.supervisor =
        h.supervisor
            .clone()
            .with_hold(std::sync::Arc::new(common::FakeHold::attaching(
                h.go2rtc.clone(),
            )));
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let warm = common::eventually(|| async {
        let (consumers, connected) = h.go2rtc.stream_view(&name);
        consumers == 1 && connected
    })
    .await;
    assert!(
        warm,
        "the hold's consumer warmed the producer with no viewer: {:?}",
        h.go2rtc.stream_view(&name)
    );
    assert!(is_live(&h.supervisor).await, "and the state holds live");
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The tile chain is held warm like the high: born on the first ask, its
/// producer stays up after every consumer leaves, so a tab never waits out
/// its chain's spawn (measured through the hub: 30s cold).
#[tokio::test]
async fn the_tile_chain_is_held_warm() {
    let mut h = harness(pacing(Duration::from_secs(30))).await;
    h.supervisor =
        h.supervisor
            .clone()
            .with_hold(std::sync::Arc::new(common::FakeHold::attaching(
                h.go2rtc.clone(),
            )));
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let low = stream_name(DEVICE_ID, Quality::Low);

    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile registers");
    let held = common::eventually(|| async { h.go2rtc.stream_view(&low).0 >= 1 }).await;
    assert!(
        held,
        "the tile's chain carries narar's own consumer: {:?}",
        h.go2rtc.stream_view(&low)
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Every re-PUT reaps its predecessor, the tile's re-asserts included: the
/// retry re-arms the chain through the same swap, so the loopback side
/// cannot fan out generations either.
#[tokio::test]
async fn a_tile_re_assert_reaps_the_predecessor_too() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let low = stream_name(DEVICE_ID, Quality::Low);

    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile registers");
    let landed =
        common::eventually(|| async { h.go2rtc.puts().iter().any(|(name, _)| name == &low) }).await;
    assert!(landed, "the tile's chain landed: {:?}", h.go2rtc.puts());

    // The person's Try again re-asserts the chain: a retry PUTs again.
    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, true)
        .await
        .expect("the re-assert lands");
    let re_put = common::eventually(|| async {
        h.go2rtc
            .puts()
            .iter()
            .filter(|(name, _)| name == &low)
            .count()
            >= 2
    })
    .await;
    assert!(
        re_put,
        "the retry re-asserts the chain: {:?}",
        h.go2rtc.puts()
    );
    let ops = h.go2rtc.ops();
    assert!(
        ops.windows(2)
            .any(|pair| pair[0] == format!("delete:{low}") && pair[1] == format!("put:{low}")),
        "the re-assert reaps the predecessor before its PUT: {ops:?}"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// narar's own consumer never counts as a viewer: a stream nobody asks for
/// keeps its picture and its own hold, however long it sits unasked.
#[tokio::test]
async fn the_hold_lives_with_the_stream() {
    let mut h = harness(Pacing {
        stream_idle: Duration::from_millis(150),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    h.supervisor =
        h.supervisor
            .clone()
            .with_hold(std::sync::Arc::new(common::FakeHold::attaching(
                h.go2rtc.clone(),
            )));
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let held = common::eventually(|| async { h.go2rtc.stream_view(&name).0 >= 1 }).await;
    assert!(held, "the hold's consumer attached");

    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !released_last(&h),
        "the idle window retires nothing: {:?}",
        h.go2rtc.ops()
    );
    assert_eq!(
        h.go2rtc.stream_view(&name).0,
        1,
        "and the hold stays attached with its stream"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A closed run leaves no stale deadline behind: a deadline armed during
/// the run (here: a failed mint's backoff) must not fire after the
/// recovery.
#[tokio::test]
async fn a_closed_run_does_not_re_mint_on_the_stale_deadline() {
    let h = harness(Pacing {
        retry_base: Duration::from_millis(300),
        retry_max: Duration::from_secs(30),
        mint_settle: Duration::from_millis(50),
        remint: Duration::from_millis(250),
        connect_window: Duration::from_secs(5),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(reminted, "the starved run re-mints: {:?}", h.go2rtc.puts());

    // The cloud starts refusing: the loop backs off with a deadline armed,
    // and the storm grows the failure tail (about 300, 900, 2100ms).
    mint_mock(500, "").with_priority(1).mount(&h.qubo).await;
    tokio::time::sleep(Duration::from_millis(2600)).await;
    assert!(
        mint_count(&h.qubo).await >= 4,
        "the pre-recovery storm ran: {} mints",
        mint_count(&h.qubo).await
    );

    // The camera's push lands on the URL already attached: the run closes
    // while the failure's tail is still standing.
    h.go2rtc.connect_producer(&name);
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "the recovery closes the run");

    // With no run and no viewer, the closed run's pace is the whole story:
    // a fresh refusal must retry at retry_base, not the grown tail. The
    // mints land on the remint cadence (250ms); the reset keeps the
    // failure backoff at ~300/600ms, the tail would put the second gap
    // alone past the window.
    h.go2rtc.set_viewers(&name, 0);
    h.go2rtc.disconnect_producer(&name);
    let before = mint_count(&h.qubo).await;
    tokio::time::sleep(Duration::from_millis(2000)).await;
    let refusals = mint_count(&h.qubo).await - before;
    assert!(
        refusals >= 2,
        "the post-recovery refusal retries at retry_base: {refusals} refused mints in 2s"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A held stream is watched for liveness even with nobody watching: its
/// producer dying (the cloud session's death) opens the run, and the heal
/// re-mints at once - the producer was proven alive this mint, so no dial
/// grace applies, and the session is not left dead until the cadence.
#[tokio::test]
async fn a_held_stream_with_a_dead_producer_heals_at_once() {
    let mut h = harness(pacing(Duration::from_secs(30))).await;
    h.supervisor =
        h.supervisor
            .clone()
            .with_hold(std::sync::Arc::new(common::FakeHold::attaching(
                h.go2rtc.clone(),
            )));
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let warm = common::eventually(|| async { h.go2rtc.stream_view(&name).1 }).await;
    assert!(warm, "the hold warmed the producer");
    let mints = h.go2rtc.puts().len();

    // The camera goes away under the hold: nobody is watching, and the
    // hold alone notices - the run re-mints off its own clock.
    h.go2rtc.disconnect_producer(&name);
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() > mints }).await;
    assert!(
        reminted,
        "the held stream's death re-mints without a viewer: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A fresh mint is given its settle before any re-mint replaces it: the
/// camera's push to a new session lags, and replacing the URL under an
/// in-flight push starves it (the dead spiral measured on the hub).
#[tokio::test]
async fn a_young_mint_is_not_trampled_by_the_run() {
    let h = harness(Pacing {
        mint_settle: Duration::from_millis(600),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);

    // Inside the settle: the waiting viewer does not get the URL swapped
    // out from under the dial.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "one mint inside the settle: {:?}",
        h.go2rtc.puts()
    );

    // The settle spent: the run re-mints for the waiting viewer.
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(
        reminted,
        "the settle spent, the run re-mints: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The hold's own consumer is the early witness of a spent session: it
/// dies with the frames while go2rtc still reports the producer connected
/// (the hub's death sat invisible ~58s until the producer's own read
/// timeout). A death well into the session's life re-mints at once, with
/// no viewer and no run to wait on.
#[tokio::test]
async fn a_hold_death_re_mints_at_once() {
    let mut h = harness(pacing(Duration::from_secs(30))).await;
    let hold = std::sync::Arc::new(common::FakeHold::attaching(h.go2rtc.clone()));
    h.supervisor = h.supervisor.clone().with_hold(hold.clone());
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let warm = common::eventually(|| async { h.go2rtc.stream_view(&name).1 }).await;
    assert!(warm, "the hold warmed the producer");
    // The producer stays CONNECTED throughout: the illusion is the point.
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mints = h.go2rtc.puts().len();

    hold.report_death();
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() > mints }).await;
    assert!(
        reminted,
        "the hold's death re-mints without a viewer: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A hold death inside a mint's settle is a swap's own churn, not a
/// witness: it must not start a mint of its own.
#[tokio::test]
async fn a_young_mints_hold_death_does_not_churn() {
    // A settle long enough for the death to land inside it.
    let mut h = harness(Pacing {
        mint_settle: Duration::from_secs(2),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    let hold = std::sync::Arc::new(common::FakeHold::attaching(h.go2rtc.clone()));
    h.supervisor = h.supervisor.clone().with_hold(hold.clone());
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let warm = common::eventually(|| async { h.go2rtc.stream_view(&name).1 }).await;
    assert!(warm, "the hold warmed the producer");

    // The death lands inside the mint's settle: a swap's churn, not a
    // witness.
    hold.report_death();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "a swap-churn death mints nothing: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A stuck push backs the mints off: consecutive fresh mints that never
/// connect double the settle each round. The camera's push to a new
/// session can lag past the base settle (52s measured), and a metronomic
/// re-mint at that cadence replaces the URL before the push can land -
/// the hub starved for 17 minutes on exactly that ~62s metronome.
#[tokio::test]
async fn a_stuck_push_backs_the_mints_off() {
    let h = harness(Pacing {
        mint_settle: Duration::from_millis(250),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two", "three"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);
    let second = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(second, "the stuck run re-mints after the first settle");

    // The third waits the DOUBLED settle: past the base one, short of the
    // doubled one, nothing goes out.
    tokio::time::sleep(Duration::from_millis(350)).await;
    assert_eq!(
        h.go2rtc.puts().len(),
        2,
        "the doubled settle holds the third mint: {:?}",
        h.go2rtc.puts()
    );
    let third = common::eventually(|| async { h.go2rtc.puts().len() >= 3 }).await;
    assert!(
        third,
        "and the third lands when the doubled settle is spent"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A run's re-mints pace on a compounding backoff: every mid-run mint is
/// another attempt at a picture that has not arrived, not a reason to
/// start the delay over.
#[tokio::test]
async fn a_stall_re_mints_on_a_compounding_backoff() {
    let h = harness(Pacing {
        retry_base: Duration::from_millis(50),
        retry_max: Duration::from_millis(400),
        mint_settle: Duration::from_millis(50),
        connect_window: Duration::from_secs(5),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);
    let reminting = common::eventually(|| async { h.go2rtc.puts().len() >= 3 }).await;
    assert!(reminting, "the run re-mints: {:?}", h.go2rtc.puts());

    // One second at the cap: attempts land on the backoff (50ms doubling
    // to 400ms), not on every liveness slice.
    let before = h.go2rtc.puts().len();
    tokio::time::sleep(Duration::from_millis(1000)).await;
    let minted = h.go2rtc.puts().len() - before;
    assert!(
        (1..=6).contains(&minted),
        "paced, not per-slice: {minted} re-mints in a second"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A hold that never attaches must not shadow a real viewer: the viewer
/// still counts as watching, so the idle window cannot tear the stream
/// down under them.
#[tokio::test]
async fn a_never_attaching_hold_does_not_shadow_a_viewer() {
    let mut h = harness(Pacing {
        stream_idle: Duration::from_millis(150),
        ..pacing(Duration::from_secs(30))
    })
    .await;
    h.supervisor =
        h.supervisor
            .clone()
            .with_hold(std::sync::Arc::new(common::FakeHold::never_attaching(
                h.go2rtc.clone(),
            )));
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !released_last(&h),
        "the viewer holds the stream past the idle window: {:?}",
        h.go2rtc.ops()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// And the other half: a never-attaching hold must not mask a starved
/// viewer from the liveness watch - the run still opens and mints.
#[tokio::test]
async fn a_never_attaching_hold_does_not_mask_a_starved_viewer() {
    let mut h = harness(pacing(Duration::from_secs(30))).await;
    h.supervisor =
        h.supervisor
            .clone()
            .with_hold(std::sync::Arc::new(common::FakeHold::never_attaching(
                h.go2rtc.clone(),
            )));
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    let name = stream_name(DEVICE_ID, Quality::High);
    h.go2rtc.disconnect_producer(&name);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.go2rtc.set_viewers(&name, 1);
    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(
        reminted,
        "the viewer's starvation still opens the run: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The house against the real gateway from a stored session: the session
/// file loads, discovery accepts it, and the account's devices come back.
/// Ignored by default; run deliberately with `QUBO_E2E_SESSION` pointing
/// at a real session file (the lead's e2e one).
#[tokio::test]
#[ignore = "live: reads the real account; run with QUBO_E2E_SESSION set"]
async fn live_discovery_reads_the_stored_session() {
    let path = std::env::var("QUBO_E2E_SESSION").expect("set QUBO_E2E_SESSION to a session file");
    let go2rtc = common::FakeApi::start().await;
    let client = QuboClient::new(
        BASE_URL.parse().expect("the gateway URL parses"),
        ServiceProvider::default(),
    );
    let supervisor = Supervisor::new(
        client,
        SessionStore::new(path),
        Go2rtc::connect(go2rtc.base.clone()).expect("a client"),
        pacing(Duration::from_secs(30)),
    );

    let devices = supervisor
        .refresh_devices()
        .await
        .expect("the minted session is accepted");
    assert!(
        !supervisor.needs_login(),
        "a fresh durable session does not ask for a login"
    );
    let cameras = devices.iter().filter(|device| device.is_camera()).count();
    assert!(cameras > 0, "the house has cameras");
    println!(
        "live discovery: {} devices, {cameras} cameras",
        devices.len()
    );
    for device in &devices {
        println!("  {} ({})", device.name, device.device_id);
    }
}
