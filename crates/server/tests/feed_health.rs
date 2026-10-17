//! Feed health: a quiet feed reconnects, a fresh login reconnects it, and
//! a reconnect the gateway turns away raises the login flag the streams
//! already use.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{QuboClient, Session, SessionStore};
use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::mqtt::{FeedConfig, MqttService, UnitEvent};
use server::supervisor::{Pacing, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";

static NEXT_SESSION_FILE: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    supervisor: Supervisor,
    qubo: MockServer,
    events: EventBus,
    sessions: SessionStore,
}

fn temp_path() -> PathBuf {
    std::env::temp_dir().join(format!(
        "nazar-feed-{}-{}.json",
        std::process::id(),
        NEXT_SESSION_FILE.fetch_add(1, Ordering::SeqCst)
    ))
}

fn session(token: &str) -> Session {
    Session {
        access_token: token.to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
        refresh_token: None,
    }
}

/// The feed pointed at the test's broker, with the windows squeezed so a
/// test lives in milliseconds.
fn feed_config(
    port: u16,
    quiet_window: Duration,
    session_check: Duration,
    motion_window: Duration,
) -> FeedConfig {
    FeedConfig {
        host: "127.0.0.1".to_owned(),
        port,
        tls: false,
        quiet_window,
        motion_window,
        session_check,
        idle_retry: Duration::from_millis(20),
        retry_min: Duration::from_millis(20),
        retry_max: Duration::from_millis(50),
    }
}

async fn harness(
    broker: &common::mqtt::FakeBroker,
    quiet_window: Duration,
    session_check: Duration,
    motion_window: Duration,
) -> Harness {
    let config = feed_config(broker.port(), quiet_window, session_check, motion_window);
    harness_with(broker, config).await
}

async fn harness_with(_broker: &common::mqtt::FakeBroker, config: FeedConfig) -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    mount_json(
        &qubo,
        UNITS_PATH,
        json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string(),
    )
    .await;
    mount_json(
        &qubo,
        DEVICES_PATH,
        json!({ "data": [{ "deviceUUID": "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01", "deviceName": "Hall", "deviceType": "HCP01" }] })
            .to_string(),
    )
    .await;

    let sessions = SessionStore::new(temp_path());
    sessions.save(&session("token-abc")).expect("the session");
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
            arm_timeout: Duration::from_secs(15),
            lane_cooldown: Duration::from_secs(600),
        },
    );
    let events = EventBus::new();
    let _feed = MqttService::start(sessions.clone(), supervisor.clone(), events.clone(), config);

    Harness {
        supervisor,
        qubo,
        events,
        sessions,
    }
}

async fn mount_json(server: &MockServer, route: &str, body: String) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
        .mount(server)
        .await;
}

#[tokio::test]
async fn a_silent_feed_reconnects_no_faster_than_its_window() {
    let broker = common::mqtt::FakeBroker::silent().await;
    let _h = harness(
        &broker,
        Duration::from_millis(120),
        Duration::from_millis(30),
        Duration::from_secs(30),
    )
    .await;

    let reconnected = common::eventually(|| async { broker.sessions() >= 2 }).await;
    assert!(
        reconnected,
        "a feed that has heard nothing for the window reconnects"
    );

    // Five windows' worth of silence: a watchdog nudges the feed, it does
    // not spin it. A feed cycle subscribes once, so the broker's session
    // count is the reconnect count.
    let settled = broker.sessions();
    tokio::time::sleep(Duration::from_millis(600)).await;
    let reconnects = broker.sessions() - settled;
    assert!(
        (3..=7).contains(&reconnects),
        "a quiet feed reconnects about once per window, not in a loop: {reconnects} in five windows"
    );
}

#[tokio::test]
async fn a_chatty_feed_is_left_alone() {
    let broker = common::mqtt::FakeBroker::chatty(Duration::from_millis(40)).await;
    let h = harness(
        &broker,
        Duration::from_millis(200),
        Duration::from_millis(30),
        Duration::from_secs(30),
    )
    .await;
    let mut watcher = h.events.subscribe();

    let heard = tokio::time::timeout(Duration::from_secs(2), watcher.recv())
        .await
        .expect("a heartbeat arrives")
        .expect("the subscription is live");
    assert_eq!(
        heard,
        UnitEvent::Liveness {
            device_id: "d0000000-0000-4000-8000-000000000002".to_owned(),
            online: true,
        },
        "the feed parses what the broker publishes"
    );

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        broker.sessions(),
        1,
        "a feed that keeps hearing the house is never reconnected"
    );
}

/// A refusal on the feed's re-resolve spends the stored refresh token
/// once, and the connect that follows rides the fresh session - the
/// reconnect must never carry the token that was just refused.
#[tokio::test]
async fn a_refused_re_resolve_heals_onto_the_fresh_token() {
    const REFRESH_PATH: &str =
        "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/users/user-uuid-1/auth/refresh";
    let broker = common::mqtt::FakeBroker::chatty(Duration::from_millis(40)).await;
    let h = harness(
        &broker,
        Duration::from_secs(30),
        Duration::from_millis(30),
        Duration::from_secs(30),
    )
    .await;
    // The durable pair, as a real login would have left it.
    h.sessions
        .save(&Session {
            access_token: "token-old".to_owned(),
            uuid: "user-uuid-1".to_owned(),
            email: None,
            refresh_token: Some("refresh-1".to_owned()),
        })
        .expect("the durable session saves");
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer token-old",
            ))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(include_str!("fixtures/error_auth_002.json")),
            )
            .with_priority(2)
            .mount(&h.qubo)
            .await;
        Mock::given(method("GET"))
            .and(path(route))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer token-new",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_string(if route == UNITS_PATH {
                json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string()
            } else {
                json!({ "data": [{ "deviceUUID": "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01", "deviceName": "Hall", "deviceType": "HCP01" }] })
                    .to_string()
            }))
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

    let connected = common::eventually(|| async { broker.sessions() >= 1 }).await;
    assert!(
        connected,
        "the feed connects once the refusal heals: {} sessions",
        broker.sessions()
    );
    assert!(
        !h.supervisor.needs_login(),
        "a healed re-resolve never raises the login flag"
    );
    let stored = h
        .sessions
        .load()
        .expect("the session loads")
        .expect("a session is stored");
    assert_eq!(
        stored.access_token, "token-new",
        "the broker's next connect rides the fresh token"
    );
    // The wire is the proof: no CONNECT ever carried the refused token -
    // the store would read healed even if the connect went out stale.
    let connects = broker.connects();
    assert!(
        !connects.is_empty(),
        "at least one CONNECT was recorded: {connects:?}"
    );
    assert!(
        connects.iter().all(|token| token != "token-old"),
        "no connect ever carried the refused token: {connects:?}"
    );
    assert!(
        connects.iter().any(|token| token == "token-new"),
        "and a connect carried the fresh one: {connects:?}"
    );
}

#[tokio::test]
async fn a_fresh_login_reconnects_the_feed() {
    let broker = common::mqtt::FakeBroker::chatty(Duration::from_millis(40)).await;
    let h = harness(
        &broker,
        Duration::from_secs(30),
        Duration::from_millis(40),
        Duration::from_secs(30),
    )
    .await;
    assert!(
        common::eventually(|| async { broker.sessions() == 1 }).await,
        "the feed connects once"
    );

    // The family logs in again: the feed's token is stale even though the
    // broker still holds the old connection.
    h.sessions
        .save(&session("token-new"))
        .expect("the fresh session saves");

    let reconnected = common::eventually(|| async { broker.sessions() >= 2 }).await;
    assert!(
        reconnected,
        "a session change reconnects the feed onto the new token"
    );
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert_eq!(
        broker.sessions(),
        2,
        "and once, not repeatedly: the new token matches from then on"
    );
}

#[tokio::test]
async fn a_refused_subscription_reconnects_the_feed() {
    // The quiet window is long, so only the broker's refusal can move it,
    // and the retry ceiling is far away so a bypass of the backoff shows as
    // a burst of sessions.
    let broker = common::mqtt::FakeBroker::refusing().await;
    let config = FeedConfig {
        retry_max: Duration::from_secs(5),
        ..feed_config(
            broker.port(),
            Duration::from_secs(30),
            Duration::from_millis(30),
            Duration::from_secs(30),
        )
    };
    let _h = harness_with(&broker, config).await;

    let reconnected = common::eventually(|| async { broker.sessions() >= 2 }).await;
    assert!(
        reconnected,
        "a subscription the broker refuses is not a quiet house: the feed reconnects"
    );

    // ...and it reconnects on the backoff, not in a hot loop: each attempt
    // costs a REST call and a handshake, and a broker that keeps refusing
    // must not be hammered. With retry_max at five seconds, a burst of
    // attempts here would mean the refusal bypassed the backoff.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        broker.sessions() <= 8,
        "a refusal is retried on the backoff, not in a hot loop (saw {})",
        broker.sessions()
    );
}

#[tokio::test]
async fn a_feed_with_no_motion_reconnects_on_its_own_clock() {
    // Heartbeats keep arriving, so nothing looks quiet - but no motion ever
    // does, which is what a silently dead motion subscription looks like.
    let broker = common::mqtt::FakeBroker::chatty(Duration::from_millis(40)).await;
    let _h = harness(
        &broker,
        Duration::from_secs(30),
        Duration::from_millis(30),
        Duration::from_millis(150),
    )
    .await;

    // Waited for tightly: well inside the window the feed was given, so a
    // window that does not fire at all cannot pass this.
    let mut reconnected = false;
    for _ in 0..30 {
        if broker.sessions() >= 2 {
            reconnected = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        reconnected,
        "a feed that has heard no motion for its own window reconnects"
    );
}

#[tokio::test]
async fn motion_keeps_the_feed_connected() {
    let broker = common::mqtt::FakeBroker::moving(Duration::from_millis(40)).await;
    let h = harness(
        &broker,
        Duration::from_secs(30),
        Duration::from_millis(30),
        // Wide against the motion cadence below, so a delayed tick under
        // load cannot be read as a quiet feed.
        Duration::from_secs(2),
    )
    .await;
    let mut watcher = h.events.subscribe();

    tokio::time::timeout(Duration::from_secs(2), watcher.recv())
        .await
        .expect("a motion event arrives")
        .expect("the subscription is live");
    // Past the window itself, not just into it: the motion stream has to
    // hold the feed open for longer than the window it would otherwise
    // trip.
    tokio::time::sleep(Duration::from_millis(2_500)).await;

    assert_eq!(
        broker.sessions(),
        1,
        "motion is liveness: the feed stays on its connection"
    );
}

#[tokio::test]
async fn a_blip_after_a_healthy_stretch_keeps_the_fast_retry() {
    // Every connection hears one heartbeat and is then dropped: each cycle
    // was healthy, so the feed must keep coming back at the minimum delay,
    // not climb towards the maximum.
    let broker = common::mqtt::FakeBroker::shaky(Duration::from_millis(20)).await;
    let config = FeedConfig {
        retry_max: Duration::from_secs(5),
        ..feed_config(
            broker.port(),
            Duration::from_secs(30),
            Duration::from_millis(30),
            Duration::from_secs(30),
        )
    };
    let _h = harness_with(&broker, config).await;

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        broker.sessions() >= 8,
        "a healthy-then-dropped feed retries at the minimum delay (saw {})",
        broker.sessions()
    );
}

#[tokio::test]
async fn an_auth_refused_reconnect_raises_the_login_flag() {
    let broker = common::mqtt::FakeBroker::silent().await;
    let h = harness(
        &broker,
        Duration::from_millis(120),
        Duration::from_millis(30),
        Duration::from_secs(30),
    )
    .await;
    assert!(
        common::eventually(|| async { broker.sessions() >= 1 }).await,
        "the feed connects while the session lives"
    );
    assert!(
        !h.supervisor.needs_login(),
        "a working session raises nothing"
    );

    // The session expires: every later discovery is turned away.
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(include_str!("fixtures/error_auth_002.json")),
            )
            .with_priority(1)
            .mount(&h.qubo)
            .await;
    }

    let raised = common::eventually(|| async { h.supervisor.needs_login() }).await;
    assert!(
        raised,
        "a reconnect the gateway refuses sends the UI to login"
    );
}
