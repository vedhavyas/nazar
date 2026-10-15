//! Supervisor tests: mint-once reuse, session loss, and mint failures.
//!
//! The Qubo side answers with the captured shapes: the 401 body is the
//! live capture in `docs/qubo-vod-verification.md`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{QuboClient, Session, SessionStore};
use server::go2rtc::Go2rtc;
use server::supervisor::{Pacing, StreamState, Supervisor, SupervisorError};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
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
    let supervisor = Supervisor::new(client, store, Go2rtc::connect(go2rtc.base.clone()), pacing);
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

#[tokio::test]
async fn ensure_live_mints_once_and_reuses_the_stream() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(200, &mint_body("first")).mount(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");

    h.supervisor
        .ensure_live(DEVICE_ID)
        .await
        .expect("the first call starts the stream");
    assert!(is_live(&h.supervisor).await, "the stream goes live");
    assert_eq!(mint_count(&h.qubo).await, 1, "the first call mints once");
    assert_eq!(h.go2rtc.puts().len(), 1, "the source reached go2rtc");
    let (name, source) = h.go2rtc.puts().remove(0);
    assert_eq!(name, DEVICE_ID, "the stream carries the device id");
    assert!(
        source.starts_with("ffmpeg:") && source.contains("#input=nazar#video=h264"),
        "the minted URL rides the transcoding template: {source}"
    );

    h.supervisor
        .ensure_live(DEVICE_ID)
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
        .ensure_live(DEVICE_ID)
        .await
        .expect("the stream starts");
    assert!(is_live(&h.supervisor).await, "the stream goes live");

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
        puts.iter().all(|(name, _)| name == DEVICE_ID),
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
        .ensure_live(DEVICE_ID)
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
    // Long pacing: only the wake signal can deliver a re-mint this fast.
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    mint_mock(401, include_str!("fixtures/error_auth_002.json"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");
    h.supervisor.ensure_live(DEVICE_ID).await.expect("starts");

    assert!(
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Degraded { .. }
        ),
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

    h.supervisor.ensure_live(DEVICE_ID).await.expect("retry");
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
async fn a_cancelled_start_does_not_strand_the_device() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(mint_body("slow"))
                .set_delay(Duration::from_secs(5)),
        )
        .mount(&h.qubo)
        .await;
    h.supervisor.refresh_devices().await.expect("discovery");

    let cancelled = tokio::time::timeout(
        Duration::from_millis(150),
        h.supervisor.ensure_live(DEVICE_ID),
    )
    .await;
    assert!(cancelled.is_err(), "the request gave up mid-mint");

    assert_eq!(
        h.supervisor.status(DEVICE_ID).await,
        StreamState::Offline,
        "a cancelled start leaves no Waking registration behind"
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

    h.supervisor.ensure_live(DEVICE_ID).await.expect("starts");

    match h.supervisor.status(DEVICE_ID).await {
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

    h.supervisor.ensure_live(DEVICE_ID).await.expect("starts");

    match h.supervisor.status(DEVICE_ID).await {
        StreamState::Degraded { reason } => {
            assert!(
                !reason.contains("sekrit-token"),
                "the browser's reason carries no mint token: {reason}"
            );
            assert!(
                reason.contains("token=<redacted>"),
                "the token is visibly redacted: {reason}"
            );
        }
        other => panic!("expected Degraded, got {other:?}"),
    }
    let _ = std::fs::remove_file(&h.sessions_path);
}

#[tokio::test]
async fn vod_is_reserved_for_m3() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    let err = h
        .supervisor
        .ensure_vod(DEVICE_ID, 1_700_000_000_000)
        .expect_err("M1 has no VOD path");
    assert!(
        matches!(err, SupervisorError::VodUnavailable),
        "the stub refuses, naming M3: {err:?}"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}
