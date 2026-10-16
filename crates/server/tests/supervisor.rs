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

use qubo::{Quality, QuboClient, Session, SessionStore};
use server::go2rtc::Go2rtc;
use server::supervisor::{Pacing, StreamState, Supervisor, stream_name};

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
        discovery_retry: Duration::from_millis(50),
        stream_idle: Duration::from_secs(30),
        connect_grace: Duration::from_millis(50),
        connect_window: Duration::from_millis(300),
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
        source.starts_with("ffmpeg:") && source.contains("#input=nazar#video=h264"),
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
        discovery_retry: Duration::from_secs(30),
        stream_idle: Duration::from_secs(30),
        connect_grace: Duration::from_millis(50),
        connect_window: Duration::from_millis(300),
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
                reason.contains("#input=nazar#video=h264"),
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

/// A go2rtc entry is not liveness: a producer whose source never connects
/// is re-minted with a fresh URL, fast. The URL is one-shot, and waiting
/// for the four-minute remint would leave a viewer staring at a dead
/// stream the API calls live.
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
    h.go2rtc
        .disconnect_producer(&stream_name(DEVICE_ID, Quality::High));

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");

    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 3 }).await;
    assert!(reminted, "the loop keeps minting: {:?}", h.go2rtc.puts());

    let urls: Vec<String> = h.go2rtc.puts().iter().map(|(_, src)| src.clone()).collect();
    assert_ne!(urls[0], urls[1], "the second attempt carries a fresh URL");
    assert_ne!(urls[1], urls[2], "and the third never reuses either");
    assert_eq!(
        h.supervisor.status(DEVICE_ID).await,
        StreamState::Waking,
        "an unconnected producer is not live"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// The state stays waking while the producer is away, and flips live when
/// it connects.
#[tokio::test]
async fn the_state_stays_waking_until_the_producer_connects() {
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

    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 2 }).await;
    assert!(reminted, "the loop re-mints while unconnected");
    assert_eq!(
        h.supervisor.status(DEVICE_ID).await,
        StreamState::Waking,
        "not live while the producer is away"
    );

    h.go2rtc.connect_producer(&name);
    let live = common::eventually(|| async { is_live(&h.supervisor).await }).await;
    assert!(live, "a connected producer flips the state live");
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// Every attempt after a failure carries its own URL: a consumed URL never
/// works again, so reuse is a permanent outage.
#[tokio::test]
async fn no_attempt_reuses_a_previously_minted_url() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two", "three", "four", "five"]).await;
    h.go2rtc
        .disconnect_producer(&stream_name(DEVICE_ID, Quality::High));

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");

    let reminted = common::eventually(|| async { h.go2rtc.puts().len() >= 5 }).await;
    assert!(reminted, "five attempts ran: {:?}", h.go2rtc.puts());

    let urls: Vec<String> = h.go2rtc.puts().iter().map(|(_, src)| src.clone()).collect();
    let mut unique = urls.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        urls.len(),
        "no attempt reused a URL: {urls:?}"
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}

/// A producer that never connects degrades with the reason once the
/// window passes - and the loop keeps trying rather than giving up.
#[tokio::test]
async fn a_producer_that_never_connects_degrades_after_the_window() {
    let h = harness(pacing(Duration::from_secs(30))).await;
    mount_discovery(&h.qubo).await;
    h.supervisor.refresh_devices().await.expect("discovery");
    mount_mint_ladder(&h.qubo, &["one", "two"]).await;
    mint_mock(200, &mint_body("repeat")).mount(&h.qubo).await;
    h.go2rtc
        .disconnect_producer(&stream_name(DEVICE_ID, Quality::High));

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");

    let degraded = common::eventually(|| async {
        h.go2rtc.puts().len() >= 3
            && matches!(
                h.supervisor.status(DEVICE_ID).await,
                StreamState::Degraded { reason } if reason == "the producer has not connected"
            )
    })
    .await;
    assert!(
        degraded,
        "the window passes, the reason surfaces, and the loop keeps minting: {:?}",
        h.go2rtc.puts()
    );
    let _ = std::fs::remove_file(&h.sessions_path);
}
