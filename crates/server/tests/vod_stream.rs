//! VOD playback: a seek mints a cloud session at the clip's instant and
//! hands it to go2rtc as `vod-<device>`; the player's exit releases it,
//! and one nobody watches is released on its idle window (a leaked
//! session burns a camera slot, so release is the point).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::net::TcpListener;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{QuboClient, Session, SessionStore};
use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::http::{AppState, Wiring, app};
use server::motion::MotionOrder;
use server::notify::{COALESCE, NotificationStore, Snapshots};
use server::supervisor::{Pacing, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const CAMERA: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const VOD_PATH: &str = "/stream-manager/api/v2/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";

/// A clip's instant: midnight IST, 16 October 2026.
const CLIP_MS: i64 = 1_792_089_000_000;

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nazar-{name}-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    dir
}

struct Harness {
    base: String,
    qubo: MockServer,
    go2rtc: common::FakeApi,
}

async fn harness(stream_idle: Duration) -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    mount_discovery(&qubo).await;
    let data_dir = temp_dir("vod-harness");
    let sessions = SessionStore::new(data_dir.join("session.json"));
    sessions
        .save(&Session {
            access_token: "token-abc".to_owned(),
            uuid: "user-uuid-1".to_owned(),
            email: None,
        })
        .expect("the session saves");
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
            stream_idle,
        },
    );
    let snapshots = Snapshots::new(Go2rtc::connect(go2rtc.base.clone()), data_dir.join("snaps"));
    let state = AppState::new(
        "test",
        Wiring {
            sessions,
            qubo: client,
            supervisor: supervisor.clone(),
            go2rtc_api: go2rtc.base.to_string(),
            events: EventBus::new(),
            motion: Arc::new(Mutex::new(MotionOrder::new())),
            notifications: Arc::new(NotificationStore::open(
                data_dir.join("notifications.json"),
                COALESCE,
            )),
            episodes: common::episodes_in(&data_dir),
            snapshots,
        },
    );
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
    let base = format!("http://{}", listener.local_addr().expect("the address"));
    tokio::spawn(async move {
        axum::serve(listener, app(state)).await.expect("serves");
    });
    Harness { base, qubo, go2rtc }
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

/// The VOD mint, as the captured session answered it.
async fn mount_vod(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path(VOD_PATH))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(include_str!("../../qubo/tests/fixtures/mint_vod.json")),
        )
        .mount(server)
        .await;
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("a client")
}

async fn play(h: &Harness, timestamp: i64) -> reqwest::Response {
    client()
        .post(format!("{}/api/cameras/{}/playback", h.base, CAMERA))
        .json(&json!({ "timestamp": timestamp }))
        .send()
        .await
        .expect("the playback starts")
}

#[tokio::test]
async fn a_playback_mints_at_the_instant_and_names_its_own_stream() {
    let h = harness(Duration::from_secs(120)).await;
    mount_vod(&h.qubo).await;

    let started = play(&h, CLIP_MS).await;
    assert_eq!(started.status(), reqwest::StatusCode::OK);
    let body: Value = started.json().await.expect("json");
    assert_eq!(
        body["stream"],
        json!(format!("vod-{CAMERA}")),
        "the player asks go2rtc for the playback stream's own name"
    );
    assert!(
        body["expires_in"].is_u64(),
        "and the release window is told: {body}"
    );

    let mints: Vec<String> = h
        .qubo
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|request| request.url.path() == VOD_PATH)
        .map(|request| request.url.query().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(mints.len(), 1, "one mint for one seek");
    assert!(
        mints[0].contains(&format!("timestamp={CLIP_MS}")),
        "minted at the clip's own instant: {mints:?}"
    );

    let puts = h.go2rtc.puts();
    assert_eq!(puts.len(), 1, "the source reached go2rtc: {puts:?}");
    assert_eq!(
        puts[0].0,
        format!("vod-{CAMERA}"),
        "under the playback name"
    );
    assert!(
        puts[0].1.starts_with("ffmpeg:rtsps://"),
        "fed from the minted session: {}",
        puts[0].1
    );
}

#[tokio::test]
async fn a_second_seek_releases_the_old_session_first() {
    let h = harness(Duration::from_secs(120)).await;
    mount_vod(&h.qubo).await;

    assert_eq!(play(&h, CLIP_MS).await.status(), reqwest::StatusCode::OK);
    assert_eq!(
        play(&h, CLIP_MS + 60_000).await.status(),
        reqwest::StatusCode::OK
    );

    assert_eq!(
        h.go2rtc.deletes(),
        vec![format!("vod-{CAMERA}")],
        "the old playback stream was dropped before the new one was set"
    );
    let mints: Vec<String> = h
        .qubo
        .received_requests()
        .await
        .expect("recorded requests")
        .iter()
        .filter(|request| request.url.path() == VOD_PATH)
        .map(|request| request.url.query().unwrap_or_default().to_owned())
        .collect();
    assert!(
        mints
            .iter()
            .any(|q| q.contains(&format!("timestamp={}", CLIP_MS + 60_000))),
        "the seek minted at its new instant: {mints:?}"
    );
}

#[tokio::test]
async fn a_seek_releases_the_old_session_before_it_sets_the_new_one() {
    let h = harness(Duration::from_secs(120)).await;
    mount_vod(&h.qubo).await;

    assert_eq!(play(&h, CLIP_MS).await.status(), reqwest::StatusCode::OK);
    assert_eq!(
        play(&h, CLIP_MS + 60_000).await.status(),
        reqwest::StatusCode::OK
    );

    let name = format!("vod-{CAMERA}");
    assert_eq!(
        h.go2rtc.ops(),
        vec![
            format!("put:{name}"),
            format!("delete:{name}"),
            format!("put:{name}")
        ],
        "the seek releases the old session before the new source is set: a \
         put before the release would be deleted again under the player's name"
    );
}

/// A release landing inside a seek's own release step: whichever way the
/// two interleave, one session is released per session minted, nothing is
/// deleted that was never set, and no stream is left behind for a player
/// who left.
#[tokio::test]
async fn a_release_landing_during_a_seek_leaves_nothing_running() {
    let h = harness(Duration::from_secs(120)).await;
    mount_vod(&h.qubo).await;
    assert_eq!(play(&h, CLIP_MS).await.status(), reqwest::StatusCode::OK);

    // The seek's own release of the old session takes its time, opening
    // the window for the player's release to land inside it.
    h.go2rtc.set_delete_delay(Duration::from_millis(300));
    let seeking = play(&h, CLIP_MS + 60_000);
    tokio::time::sleep(Duration::from_millis(100)).await;
    let leaving = client()
        .delete(format!("{}/api/cameras/{CAMERA}/playback", h.base))
        .send();

    let (seeked, released) = tokio::join!(seeking, leaving);
    assert_eq!(seeked.status(), reqwest::StatusCode::OK);
    assert_eq!(
        released.expect("the release").status(),
        reqwest::StatusCode::NO_CONTENT
    );

    // However it landed, the state now converges: one more release is a
    // no-op if the racing one already took the fresh session, and takes it
    // if it did not.
    let name = format!("vod-{CAMERA}");
    let settled = client()
        .delete(format!("{}/api/cameras/{CAMERA}/playback", h.base))
        .send()
        .await
        .expect("the settling release");
    assert_eq!(settled.status(), reqwest::StatusCode::NO_CONTENT);

    assert_eq!(
        h.go2rtc.deletes(),
        vec![name.clone(), name.clone()],
        "two sessions minted, two released - and nothing else: {:?}",
        h.go2rtc.ops()
    );
    assert_eq!(
        h.go2rtc
            .ops()
            .iter()
            .filter(|op| op.as_str() == format!("put:{name}"))
            .count(),
        2,
        "both mints reached go2rtc: {:?}",
        h.go2rtc.ops()
    );
    // And nothing re-appears: the final release found nothing to release.
    let again = client()
        .delete(format!("{}/api/cameras/{CAMERA}/playback", h.base))
        .send()
        .await
        .expect("the last release");
    assert_eq!(again.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        h.go2rtc.deletes().len(),
        2,
        "no third delete appeared: {:?}",
        h.go2rtc.ops()
    );
}

#[tokio::test]
async fn the_player_leaving_releases_the_session() {
    let h = harness(Duration::from_secs(120)).await;
    mount_vod(&h.qubo).await;
    assert_eq!(play(&h, CLIP_MS).await.status(), reqwest::StatusCode::OK);

    let released = client()
        .delete(format!("{}/api/cameras/{}/playback", h.base, CAMERA))
        .send()
        .await
        .expect("the release");
    assert_eq!(released.status(), reqwest::StatusCode::NO_CONTENT);
    assert_eq!(
        h.go2rtc.deletes(),
        vec![format!("vod-{CAMERA}")],
        "go2rtc forgot the playback stream"
    );

    // Leaving twice is a person clicking twice: nothing to release, and
    // nothing to complain about.
    let again = client()
        .delete(format!("{}/api/cameras/{}/playback", h.base, CAMERA))
        .send()
        .await
        .expect("the second release");
    assert_eq!(again.status(), reqwest::StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn an_unwatched_playback_releases_itself() {
    let h = harness(Duration::from_millis(60)).await;
    mount_vod(&h.qubo).await;
    assert_eq!(play(&h, CLIP_MS).await.status(), reqwest::StatusCode::OK);

    // Nobody asked again and nobody is watching: the session must not
    // outlive its window, or it burns the camera's slot.
    let released =
        common::eventually(|| async { h.go2rtc.deletes() == vec![format!("vod-{CAMERA}")] }).await;
    assert!(
        released,
        "the idle playback released itself: {:?}",
        h.go2rtc.deletes()
    );
}

#[tokio::test]
async fn a_watched_playback_is_not_released() {
    let h = harness(Duration::from_millis(60)).await;
    mount_vod(&h.qubo).await;
    h.go2rtc.set_viewers(&format!("vod-{CAMERA}"), 1);
    assert_eq!(play(&h, CLIP_MS).await.status(), reqwest::StatusCode::OK);

    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        h.go2rtc.deletes().is_empty(),
        "a watched playback outlives the idle window: {:?}",
        h.go2rtc.deletes()
    );

    // The viewer leaves; the idle rule applies again.
    h.go2rtc.set_viewers(&format!("vod-{CAMERA}"), 0);
    let released =
        common::eventually(|| async { h.go2rtc.deletes() == vec![format!("vod-{CAMERA}")] }).await;
    assert!(released, "once nobody is watching, it is released");
}

#[tokio::test]
async fn a_seek_without_an_instant_is_refused() {
    let h = harness(Duration::from_secs(120)).await;
    let missing = client()
        .post(format!("{}/api/cameras/{}/playback", h.base, CAMERA))
        .json(&json!({}))
        .send()
        .await
        .expect("the answer");
    assert_eq!(missing.status(), reqwest::StatusCode::BAD_REQUEST);
}
