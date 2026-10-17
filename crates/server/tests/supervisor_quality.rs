//! Quality classes: one cloud stream per camera, the tile riding it as a
//! chain inside go2rtc, and a tile nobody watches dropped after its idle
//! window.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{Quality, QuboClient, Session, SessionStore};
use server::go2rtc::Go2rtc;
use server::supervisor::{Pacing, StreamState, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";

static NEXT_SESSION_FILE: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    supervisor: Supervisor,
    qubo: MockServer,
    go2rtc: common::FakeApi,
}

/// Pacing for the quality tests: re-mints and retries are far away, so
/// anything that happens is the idle rule's doing.
fn pacing(stream_idle: Duration) -> Pacing {
    Pacing {
        remint: Duration::from_secs(30),
        retry_base: Duration::from_secs(30),
        retry_max: Duration::from_secs(30),
        mint_settle: Duration::from_millis(50),
        discovery_retry: Duration::from_secs(30),
        stream_idle,
        connect_grace: Duration::from_millis(50),
        connect_window: Duration::from_millis(300),
        liveness_poll: Duration::from_millis(50),
        touch_ttl: Duration::from_millis(300),
        arm_timeout: Duration::from_secs(15),
        lane_cooldown: Duration::from_secs(600),
    }
}

async fn harness(stream_idle: Duration) -> Harness {
    harness_with(pacing(stream_idle)).await
}

/// A harness on its own pacing, for tests that need the re-mint cadence
/// or the retry backoff inside their window.
async fn harness_with(pacing: Pacing) -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    mount_discovery(&qubo).await;
    let sessions_path = temp_path();
    let store = SessionStore::new(&sessions_path);
    store
        .save(&Session {
            access_token: "token-abc".to_owned(),
            uuid: "user-uuid-1".to_owned(),
            email: None,
            refresh_token: None,
        })
        .expect("the session saves");
    let client = QuboClient::new(qubo.uri().parse().expect("a url"), SP.into());
    let supervisor = Supervisor::new(
        client,
        store,
        Go2rtc::connect(go2rtc.base.clone()).expect("a client"),
        pacing,
    );
    supervisor.refresh_devices().await.expect("discovery");
    Harness {
        supervisor,
        qubo,
        go2rtc,
    }
}

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nazar-quality-{}-{}.json",
        std::process::id(),
        NEXT_SESSION_FILE.fetch_add(1, Ordering::SeqCst)
    ))
}

fn mint_body(quality: &str) -> String {
    json!({
        "streamURL": format!("rtsps://wowza2.platform.quboworld.com:443/hero-iot-live/{DEVICE_ID}_{quality}?token=tok"),
        "sessionId": format!("session-{quality}"),
    })
    .to_string()
}

fn mint_mock(quality: &str, status: u16, body: String) -> Mock {
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .and(query_param("type", quality))
        .respond_with(ResponseTemplate::new(status).set_body_string(body))
}

async fn mount_mint(server: &MockServer, quality: &str, status: u16, body: String) {
    mint_mock(quality, status, body).mount(server).await;
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

#[tokio::test]
async fn a_tile_rides_the_hero_stream_and_the_cloud_is_minted_once() {
    let h = harness(Duration::from_secs(30)).await;
    // Only the high mint is mounted: a low mint would be a 404, and a 404
    // is exactly the second cloud consumer this fix exists to prevent.
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the hero wakes");
    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile wakes");

    let up = common::eventually(|| async {
        let puts = h.go2rtc.puts();
        puts.iter()
            .any(|(name, _)| name == &format!("{DEVICE_ID}-high"))
            && puts
                .iter()
                .any(|(name, _)| name == &format!("{DEVICE_ID}-low"))
    })
    .await;
    assert!(up, "both streams reached go2rtc: {:?}", h.go2rtc.puts());

    let puts = h.go2rtc.puts();
    let high_source = puts
        .iter()
        .find(|(name, _)| name == &format!("{DEVICE_ID}-high"))
        .map(|(_, source)| source.clone())
        .expect("the hero's source");
    assert!(
        high_source.starts_with("ffmpeg:rtsps://"),
        "the hero is the cloud mint: {high_source}"
    );
    let low_source = puts
        .iter()
        .find(|(name, _)| name == &format!("{DEVICE_ID}-low"))
        .map(|(_, source)| source.clone())
        .expect("the tile's source");
    assert_eq!(
        low_source,
        format!(
            "ffmpeg:{DEVICE_ID}-high#input=nazar#video=nazar-h264#width=640#audio=opus#audio=aac#starttimeout=60"
        ),
        "the tile chains off the hero's own stream, inside go2rtc"
    );

    let mints = mint_queries(&h.qubo).await;
    assert!(
        mints.iter().all(|query| !query.contains("type=low")),
        "the tile never mints a cloud stream of its own: {mints:?}"
    );
    assert!(
        mints.iter().any(|query| query.contains("type=high")),
        "the hero mints high: {mints:?}"
    );
    let mirrored = common::eventually(|| async {
        h.supervisor.status_for(DEVICE_ID, Quality::Low).await
            == h.supervisor.status_for(DEVICE_ID, Quality::High).await
    })
    .await;
    assert!(
        mirrored,
        "the tile's state is the state of the stream it rides"
    );
}

#[tokio::test]
async fn a_tile_carries_the_cloud_streams_trouble() {
    let h = harness(Duration::from_secs(30)).await;
    mount_mint(&h.qubo, "high", 500, "boom".to_owned()).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile registers");

    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Degraded { .. }
        )
    })
    .await;
    assert!(degraded, "a refused cloud mint degrades the stream");
    // The mirror lands on the loop's next reconcile slice, so the tile's
    // state is waited for rather than read in the same breath.
    let tile_degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
            StreamState::Degraded { .. }
        )
    })
    .await;
    assert!(
        tile_degraded,
        "and the tile, which has nothing to ride, degrades with it: {:?}",
        h.supervisor.status_for(DEVICE_ID, Quality::Low).await
    );
}

#[tokio::test]
async fn a_failing_cameras_tile_is_kept() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 500, "boom".to_owned()).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile registers");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // No mint ever succeeds; nobody asks for the tile again. Nothing is
    // released for it: the entry and its rendition stay.
    assert!(
        h.go2rtc.deletes().is_empty(),
        "a camera that never streams keeps its tile: {:?}",
        h.go2rtc.deletes()
    );
    assert_ne!(
        h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
        StreamState::Offline,
        "and the tile is still tracked"
    );
}

#[tokio::test]
async fn asking_again_reuses_the_stream() {
    let h = harness(Duration::from_secs(30)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the hero wakes");
    let up = common::eventually(|| async { h.go2rtc.puts().len() == 1 }).await;
    assert!(up, "the first ask mints");
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("a second ask reuses");
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("and so does a third");
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert_eq!(
        h.go2rtc.puts().len(),
        1,
        "three asks, one mint: the stream is reused, not hammered"
    );
}

#[tokio::test]
async fn an_idle_tile_is_kept() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile wakes");
    tokio::time::sleep(Duration::from_millis(250)).await;

    assert!(
        h.go2rtc.deletes().is_empty(),
        "an unasked tile is never dropped: {:?}",
        h.go2rtc.deletes()
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
            StreamState::Live { .. }
        ),
        "the tile stays live"
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Live { .. }
        ),
        "and the cloud stream it rides stays with it"
    );
}

#[tokio::test]
async fn the_chain_is_re_asserted_after_a_mint() {
    let h = harness_with(Pacing {
        remint: Duration::from_secs(30),
        retry_base: Duration::from_millis(20),
        retry_max: Duration::from_millis(50),
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
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;
    // A go2rtc that refuses every source, as one that was down would: the
    // tile's own ask cannot put its chain, and neither can the first mint.
    h.go2rtc.set_put_status(500);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile registers");
    let chain = |h: &Harness| {
        h.go2rtc
            .puts()
            .iter()
            .filter(|(name, _)| name == &format!("{DEVICE_ID}-low"))
            .count()
    };
    let refused = common::eventually(|| async { chain(&h) >= 1 }).await;
    assert!(refused, "the tile's own chain attempt was made");

    // go2rtc answers again; the next mint is what puts the chain back, so
    // a mint that re-asserts the tile is the only reason it exists.
    h.go2rtc.set_put_status(200);
    let re_asserted = common::eventually(|| async { chain(&h) >= 2 }).await;
    assert!(
        re_asserted,
        "the mint re-asserted the tile's chain: {:?}",
        h.go2rtc.puts()
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
            StreamState::Live { .. }
        ),
        "and the tile rides the stream again"
    );
}

#[tokio::test]
async fn a_watched_stream_waits_out_the_remint_cadence() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;
    h.go2rtc.set_viewers(&format!("{DEVICE_ID}-high"), 1);

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream wakes");
    // Several idle windows pass, all of them watched: the early wake is
    // only a re-check of the watch, not a mint.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert_eq!(
        mint_calls(&h.qubo).await,
        1,
        "a watched stream is re-checked on the idle window but minted on the re-mint cadence"
    );
    assert!(
        h.go2rtc.deletes().is_empty(),
        "and it is never retired while watched: {:?}",
        h.go2rtc.deletes()
    );
}

#[tokio::test]
async fn an_idle_stream_keeps_its_picture() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream wakes");
    // Several idle windows pass with nobody asking.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        h.go2rtc.deletes().is_empty(),
        "an unasked stream is never released: {:?}",
        h.go2rtc.deletes()
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Live { .. }
        ),
        "and it is still live"
    );
}

/// A healthy tile chain is not re-asserted on every liveness slice: every
/// PUT rewrites go2rtc's whole config on disk, and a chain nobody broke
/// needs none of that.
#[tokio::test]
async fn a_healthy_tile_chain_is_not_re_written_every_slice() {
    let h = harness(Duration::from_secs(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile wakes");
    let chain = |h: &Harness| {
        let want = format!("put:{DEVICE_ID}-low");
        h.go2rtc.ops().iter().filter(|op| **op == want).count()
    };
    let chained = common::eventually(|| async { chain(&h) >= 1 }).await;
    assert!(chained, "the tile's chain took: {:?}", h.go2rtc.ops());

    let before = chain(&h);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        chain(&h),
        before,
        "a healthy chain is not re-written every slice: {:?}",
        h.go2rtc.ops()
    );
}

/// A re-ask of a stream that has sat unasked for windows and windows
/// reuses it where it stands: nothing was released in between.
#[tokio::test]
async fn a_re_asked_stream_is_reused() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream wakes");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        h.go2rtc.deletes().is_empty(),
        "the stream is never retired: {:?}",
        h.go2rtc.deletes()
    );

    let again = h
        .supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the re-ask lands");
    assert!(
        matches!(again, StreamState::Live { .. } | StreamState::Waking),
        "the re-ask finds the stream in place, not gone: {again:?}"
    );
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(
        h.go2rtc.deletes().is_empty(),
        "and it keeps streaming after the re-ask: {:?}",
        h.go2rtc.deletes()
    );
}

/// The teardown holds the state lock across its deletes on purpose (a
/// request landing then must spawn fresh, not be told Live about a stream
/// being deleted under it) - and every await under it is bounded, so a
/// go2rtc that accepts and then never answers wedges a status read for one
/// timeout, never for the process's lifetime.
/// A camera that fails, recovers, and fails again starts its backoff over:
/// the second storm's first retry waits `retry_base`, not the grown tail of
/// the first - otherwise a flapping camera's recovery decays toward
/// `retry_max`.
#[tokio::test]
async fn a_recovered_camera_starts_its_backoff_over() {
    let h = harness_with(Pacing {
        remint: Duration::from_millis(300),
        retry_base: Duration::from_millis(100),
        retry_max: Duration::from_millis(400),
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
    // Two refusals, then an answer: the loop has backed off to 200ms when
    // the recovery lands.
    mint_mock("high", 500, "boom".to_owned())
        .up_to_n_times(2)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    mint_mock("high", 200, mint_body("high"))
        .with_priority(2)
        .mount(&h.qubo)
        .await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let live = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Live { .. }
        )
    })
    .await;
    assert!(live, "the camera answers");

    // The camera fails again at the next re-mint.
    h.qubo.reset().await;
    mount_mint(&h.qubo, "high", 500, "boom".to_owned()).await;
    let degraded = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Degraded { .. }
        )
    })
    .await;
    assert!(degraded, "the re-mint is refused");
    let before = mint_calls(&h.qubo).await;

    // From that refusal, the next attempt is one retry_base away (100ms),
    // not the 400ms the un-reset tail would wait.
    let soon = tokio::time::timeout(Duration::from_millis(250), async {
        while mint_calls(&h.qubo).await == before {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    assert!(
        soon.is_ok(),
        "the next attempt waits retry_base, not the grown tail (mints since the reset: {})",
        mint_calls(&h.qubo).await
    );
}

#[tokio::test]
async fn a_tile_is_never_dropped_while_the_hero_keeps_streaming() {
    let h = harness(Duration::from_millis(80)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the hero wakes");
    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile wakes");

    // The hero keeps being asked for; the tile is not asked for again.
    for _ in 0..16 {
        h.supervisor
            .ensure(DEVICE_ID, Quality::High, false)
            .await
            .expect("the hero is still watched");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        h.go2rtc.deletes().is_empty(),
        "an unasked tile is never dropped: {:?}",
        h.go2rtc.deletes()
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Live { .. }
        ),
        "the hero keeps streaming"
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
            StreamState::Live { .. }
        ),
        "and the tile stays with it"
    );
}

#[tokio::test]
async fn a_tile_with_a_viewer_is_never_dropped() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;
    h.go2rtc.set_viewers(&format!("{DEVICE_ID}-low"), 1);

    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile wakes");
    // Nobody asks again: a home that is watching holds the stream open,
    // and the browser never tells this server about it. The viewer holds
    // the cloud stream too, since the tile rides it.
    tokio::time::sleep(Duration::from_millis(300)).await;

    assert!(
        h.go2rtc.deletes().is_empty(),
        "a viewed stream outlives the idle window: {:?}",
        h.go2rtc.deletes()
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
            StreamState::Live { .. }
        ),
        "and it is still live"
    );

    // The viewer leaves; the tile is kept all the same.
    h.go2rtc.set_viewers(&format!("{DEVICE_ID}-low"), 0);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.go2rtc.deletes().is_empty(),
        "the tile is kept with or without a viewer: {:?}",
        h.go2rtc.deletes()
    );
}

#[tokio::test]
async fn a_watched_tile_is_kept() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;

    for _ in 0..6 {
        h.supervisor
            .ensure(DEVICE_ID, Quality::Low, false)
            .await
            .expect("the tile wakes");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    assert!(
        h.go2rtc.deletes().is_empty(),
        "a tile that keeps being asked for is never dropped: {:?}",
        h.go2rtc.deletes()
    );
    assert!(
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::Low).await,
            StreamState::Live { .. }
        ),
        "and it is still live"
    );
    assert!(
        matches!(
            h.supervisor.status(DEVICE_ID).await,
            StreamState::Live { .. }
        ),
        "a camera held only as a tile still reads as live"
    );
}

#[tokio::test]
async fn a_status_check_that_fails_keeps_the_stream() {
    let h = harness(Duration::from_millis(60)).await;
    mount_mint(&h.qubo, "high", 200, mint_body("high")).await;
    h.go2rtc.set_streams_error(true);

    h.supervisor
        .ensure(DEVICE_ID, Quality::Low, false)
        .await
        .expect("the tile wakes");
    tokio::time::sleep(Duration::from_millis(300)).await;

    // An API that cannot say whether anyone is watching must not be read
    // as nobody: a wrong teardown kills a live picture.
    assert!(
        h.go2rtc.deletes().is_empty(),
        "an unreadable status is not an idle stream: {:?}",
        h.go2rtc.deletes()
    );

    // The status answers again; the stream is kept all the same.
    h.go2rtc.set_streams_error(false);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.go2rtc.deletes().is_empty(),
        "the stream is kept whatever the status says: {:?}",
        h.go2rtc.deletes()
    );
}

#[tokio::test]
async fn a_retry_during_a_mint_does_not_mint_again() {
    let h = harness(Duration::from_secs(30)).await;
    // The camera refuses once, then answers slowly, so a second retry can
    // land while the answering mint is still in flight.
    mint_mock("high", 500, "boom".to_owned())
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .and(query_param("type", "high"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(mint_body("high"))
                .set_delay(Duration::from_millis(250)),
        )
        .with_priority(2)
        .mount(&h.qubo)
        .await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let refused = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Degraded { .. }
        )
    })
    .await;
    assert!(refused, "a refused mint degrades the stream");

    // The person's retry starts the slow answering mint...
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, true)
        .await
        .expect("the retry");
    tokio::time::sleep(Duration::from_millis(50)).await;
    // ...and a second retry lands while it is still in flight.
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, true)
        .await
        .expect("the second retry");

    let live = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Live { .. }
        )
    })
    .await;
    assert!(live, "the answering mint landed");
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(
        mint_calls(&h.qubo).await,
        2,
        "a retry that landed during the mint is not a third mint"
    );
}

#[tokio::test]
async fn only_a_persons_retry_skips_the_backoff() {
    let h = harness(Duration::from_secs(30)).await;
    // The camera refuses once, then answers; the first mint takes the
    // refusal, every later one the success.
    mint_mock("high", 500, "boom".to_owned())
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.qubo)
        .await;
    mint_mock("high", 200, mint_body("high"))
        .with_priority(2)
        .mount(&h.qubo)
        .await;

    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("the stream registers");
    let refused = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Degraded { .. }
        )
    })
    .await;
    let seen = h.supervisor.status_for(DEVICE_ID, Quality::High).await;
    assert!(
        refused,
        "a refused mint degrades the stream (state {seen:?}, mints {})",
        mint_calls(&h.qubo).await
    );
    assert_eq!(
        mint_calls(&h.qubo).await,
        1,
        "and it backs off: a failed first mint is not retried in a hot loop"
    );

    // A page that keeps asking (the poll) must not hammer a down camera:
    // the loop's own backoff is 30s here, so any second mint is the wake.
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, false)
        .await
        .expect("asking again");
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        mint_calls(&h.qubo).await,
        1,
        "a poll's ask does not re-mint"
    );

    // The person presses Try again; the camera answers now.
    h.supervisor
        .ensure(DEVICE_ID, Quality::High, true)
        .await
        .expect("the retry");

    let live = common::eventually(|| async {
        matches!(
            h.supervisor.status_for(DEVICE_ID, Quality::High).await,
            StreamState::Live { .. }
        )
    })
    .await;
    assert!(
        live,
        "the person's retry re-mints promptly (mints: {})",
        mint_calls(&h.qubo).await
    );
    assert_eq!(
        mint_calls(&h.qubo).await,
        2,
        "one mint per ask, not a storm"
    );
}

async fn mint_queries(server: &MockServer) -> Vec<String> {
    server
        .received_requests()
        .await
        .expect("the requests")
        .iter()
        .filter(|request| request.url.path() == MINT_PATH)
        .map(|request| request.url.query().unwrap_or_default().to_owned())
        .collect()
}

async fn mint_calls(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("the requests")
        .iter()
        .filter(|request| request.url.path() == MINT_PATH)
        .count()
}

#[tokio::test]
async fn an_unknown_camera_is_refused() {
    let h = harness(Duration::from_secs(30)).await;

    let outcome = h
        .supervisor
        .ensure("0f1e2d3c-4b5a-6978-8a9b-0c1d2e3f4a5b", Quality::High, false)
        .await;
    assert!(
        matches!(
            outcome,
            Err(server::supervisor::SupervisorError::UnknownDevice(_))
        ),
        "an id the account does not have cannot be ensured"
    );
}
