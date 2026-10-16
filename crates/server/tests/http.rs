//! HTTP surface tests: the API, the go2rtc proxy and the SPA.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{QuboClient, SessionStore};
use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::http::{AppState, Wiring, app};
use server::motion::MotionOrder;
use server::supervisor::{Pacing, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const OTP_PATH: &str = "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/login";
const VERIFY_PATH: &str = "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/verify";

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

struct Harness {
    base: String,
    qubo: MockServer,
    data_dir: PathBuf,
}

async fn harness() -> Harness {
    let qubo = MockServer::start().await;
    let go2rtc = common::FakeApi::start().await;
    let data_dir = std::env::temp_dir().join(format!(
        "nazar-http-{}-{}",
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
        .expect("a stored session, as after a login");
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
    let state = AppState::new(
        "test",
        Wiring {
            sessions,
            qubo: client,
            supervisor,
            go2rtc_api: go2rtc.base.to_string(),
            events: EventBus::new(),
            motion: Arc::new(Mutex::new(MotionOrder::new())),
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
        data_dir,
    }
}

fn client() -> reqwest::Client {
    reqwest::Client::builder().build().expect("a client")
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
async fn health_is_open_and_carries_the_version() {
    let h = harness().await;
    let response = client()
        .get(format!("{}/api/health", h.base))
        .send()
        .await
        .expect("health");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(
        response.json::<Value>().await.expect("json"),
        json!({ "ok": true, "version": "test" }),
        "health answers without a session"
    );
}

#[tokio::test]
async fn a_refused_session_reads_as_needs_login() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    let browser = client();

    let listed = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("first list");
    assert_eq!(
        listed.json::<Value>().await.expect("json")["needsLogin"],
        json!(false),
        "a healthy session does not ask for a login"
    );

    // The stored session dies; discovery and mint both refuse it.
    SessionStore::new(h.data_dir.join("session.json"))
        .save(&qubo::Session {
            access_token: "token-dead".to_owned(),
            uuid: "user-uuid-1".to_owned(),
            email: None,
        })
        .expect("swap the session");
    for route in [UNITS_PATH, DEVICES_PATH] {
        Mock::given(method("GET"))
            .and(path(route))
            .and(header("authorization", "Bearer token-dead"))
            .respond_with(
                ResponseTemplate::new(401)
                    .set_body_string(include_str!("fixtures/error_auth_002.json")),
            )
            .with_priority(1)
            .mount(&h.qubo)
            .await;
    }
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .mount(&h.qubo)
        .await;

    let watch = browser
        .post(format!("{}/api/cameras/{DEVICE_ID}/live", h.base))
        .send()
        .await
        .expect("watch under a dead session");
    assert_eq!(watch.status(), reqwest::StatusCode::OK);

    // The stream's first mint is the loop's business now, so the refusal
    // lands a moment later; the page polls until it does, as the UI does.
    let mut page = Value::Null;
    let mut refused = false;
    for _ in 0..50 {
        refused = browser
            .get(format!("{}/api/cameras", h.base))
            .send()
            .await
            .expect("list under a dead session")
            .json::<Value>()
            .await
            .expect("json")["needsLogin"]
            == json!(true);
        if refused {
            page = browser
                .get(format!("{}/api/cameras", h.base))
                .send()
                .await
                .expect("list under a dead session")
                .json::<Value>()
                .await
                .expect("json");
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "a refused session routes the UI to login");
    assert_eq!(
        page["cameras"][0]["state"],
        json!("degraded"),
        "the last known camera survives the refused refresh"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn only_cameras_are_listed() {
    let h = harness().await;
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
            { "deviceUUID": DEVICE_ID, "deviceName": "Hall", "deviceType": "HCP01" },
            { "deviceUUID": "d0000000-0000-4000-8000-000000000003", "deviceName": "Purifier", "deviceType": "HPH01" },
            { "deviceUUID": "d0000000-0000-4000-8000-000000000002", "deviceName": "Something", "deviceType": "" }
        ] })
        .to_string(),
    )
    .await;

    let page: Value = client()
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras")
        .json()
        .await
        .expect("json");

    let ids: Vec<&str> = page["cameras"]
        .as_array()
        .expect("cameras")
        .iter()
        .map(|camera| camera["id"].as_str().expect("an id"))
        .collect();
    assert_eq!(
        ids,
        vec![DEVICE_ID, "d0000000-0000-4000-8000-000000000002"],
        "the camera and the unmarked device stay; the purifier is not a camera"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn the_live_listing_shows_the_cameras_it_carries() {
    let h = harness().await;
    mount_json(
        &h.qubo,
        UNITS_PATH,
        json!([{ "unitUUID": UNIT, "name": "Home" }]).to_string(),
    )
    .await;
    // The 2026-10-16 capture verbatim: on the live wire the deviceType is
    // the product family and the model rides aModelNo. A filter that reads
    // the type alone renders this house empty - the bug this pins.
    mount_json(
        &h.qubo,
        DEVICES_PATH,
        include_str!("../../qubo/tests/fixtures/devices_live.json").to_owned(),
    )
    .await;

    let page: Value = client()
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras")
        .json()
        .await
        .expect("json");

    let names: Vec<&str> = page["cameras"]
        .as_array()
        .expect("cameras")
        .iter()
        .map(|camera| camera["name"].as_str().expect("a name"))
        .collect();
    assert_eq!(
        names,
        vec!["Cam360 3MP", "First floor", "Second floor", "Front Door"],
        "the live house shows its four cameras, not an empty page"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn watching_a_camera_starts_the_stream() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({
                    "streamURL": format!("rtsps://wowza2.platform.quboworld.com:443/hero-iot-live/{DEVICE_ID}_CAMERA_VOD0?token=m1"),
                    "sessionId": "session-1",
                })
                .to_string(),
            ),
        )
        .mount(&h.qubo)
        .await;
    let browser = client();

    // The first call discovers the account's cameras.
    let unknown = browser
        .post(format!("{}/api/cameras/no-such-camera/live", h.base))
        .send()
        .await
        .expect("unknown camera");
    assert_eq!(
        unknown.status(),
        reqwest::StatusCode::NOT_FOUND,
        "an unknown camera is not found, not a panic"
    );

    let started = browser
        .post(format!("{}/api/cameras/{DEVICE_ID}/live", h.base))
        .send()
        .await
        .expect("watch");
    assert_eq!(started.status(), reqwest::StatusCode::OK);
    assert_eq!(
        started.json::<Value>().await.expect("json"),
        json!({ "stream": format!("{DEVICE_ID}-high") }),
        "the stream name is the device and its quality class"
    );

    // The first mint runs in the stream's own loop, so the page is polled
    // until it lands, as a browser would.
    let mut state = Value::Null;
    for _ in 0..50 {
        state = browser
            .get(format!("{}/api/cameras", h.base))
            .send()
            .await
            .expect("cameras")
            .json::<Value>()
            .await
            .expect("json")["cameras"][0]["state"]
            .clone();
        if state == json!("live") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(
        state,
        json!("live"),
        "the camera reports live once go2rtc holds the producer"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn the_camera_page_carries_the_degraded_reason() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("POST"))
        .and(path(MINT_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .mount(&h.qubo)
        .await;
    let browser = client();

    let _ = browser
        .post(format!("{}/api/cameras/{DEVICE_ID}/live", h.base))
        .send()
        .await
        .expect("watch");

    // The refusal comes from the stream's own loop, a moment later.
    let mut page = Value::Null;
    for _ in 0..50 {
        page = browser
            .get(format!("{}/api/cameras", h.base))
            .send()
            .await
            .expect("cameras")
            .json::<Value>()
            .await
            .expect("json");
        if page["cameras"][0]["state"] == json!("degraded") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(page["cameras"][0]["state"], json!("degraded"));
    assert_eq!(
        page["cameras"][0]["reason"],
        json!("auth error: User Token expired, Access denied"),
        "the server's reason reaches the browser verbatim"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn login_otp_forwards_to_the_gateway() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(OTP_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"uuid":"login-1"}"#))
        .mount(&h.qubo)
        .await;
    let browser = client();

    let response = browser
        .post(format!("{}/api/login/otp", h.base))
        .json(&json!({ "phone": "9876543210" }))
        .send()
        .await
        .expect("otp");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NO_CONTENT,
        "the request forwards"
    );
    let requests = h.qubo.received_requests().await.expect("recorded requests");
    let otp = requests
        .iter()
        .find(|request| request.url.path() == OTP_PATH)
        .expect("the OTP request reached the gateway");
    let body: Value = serde_json::from_slice(&otp.body).expect("json");
    assert_eq!(
        body["phone"], "9876543210",
        "the number the browser typed is the number the gateway texts"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn verify_saves_the_session() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("POST"))
        .and(path(OTP_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"uuid":"login-1"}"#))
        .mount(&h.qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(VERIFY_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!([{ "accessToken": "token-live", "uuid": "user-uuid-1", "email": "family@example.com" }])
                .to_string(),
        ))
        .mount(&h.qubo)
        .await;

    let client = client();
    let otp = client
        .post(format!("{}/api/login/otp", h.base))
        .json(&json!({ "phone": "9876543210" }))
        .send()
        .await
        .expect("otp first");
    assert_eq!(otp.status(), reqwest::StatusCode::NO_CONTENT);
    let response = client
        .post(format!("{}/api/login/verify", h.base))
        .json(&json!({ "phone": "9876543210", "code": "123456" }))
        .send()
        .await
        .expect("verify");
    assert_eq!(response.status(), reqwest::StatusCode::NO_CONTENT);
    let stored = std::fs::read_to_string(h.data_dir.join("session.json")).expect("session saved");
    assert!(
        stored.contains("token-live"),
        "the session lands in the data dir: {stored}"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn a_gateway_auth_failure_reads_as_unauthorized() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    Mock::given(method("POST"))
        .and(path(
            "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/login",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"uuid":"login-1"}"#))
        .mount(&h.qubo)
        .await;
    Mock::given(method("POST"))
        .and(path(VERIFY_PATH))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .mount(&h.qubo)
        .await;

    let browser = client();
    let otp = browser
        .post(format!("{}/api/login/otp", h.base))
        .json(&json!({ "phone": "9876543210" }))
        .send()
        .await
        .expect("otp");
    assert_eq!(otp.status(), reqwest::StatusCode::NO_CONTENT);

    let refused = browser
        .post(format!("{}/api/login/verify", h.base))
        .json(&json!({ "phone": "9876543210", "code": "000000" }))
        .send()
        .await
        .expect("verify");
    assert_eq!(
        refused.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a gateway auth failure reads as unauthorized"
    );
    assert_eq!(
        refused.json::<Value>().await.expect("json")["error"],
        json!("auth error: User Token expired, Access denied"),
        "the gateway's words ride the envelope"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn the_proxy_only_reaches_the_ws_path() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    let client = client();

    // Fill the device list, so a refusal below is about the source and not
    // about an empty list.
    let listed = client
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);

    // go2rtc's admin surface stays out of the browser's reach.
    let admin = client
        .put(format!("{}/go2rtc/api/streams?name=x&src=y", h.base))
        .send()
        .await
        .expect("admin put");
    assert_eq!(
        admin.status(),
        reqwest::StatusCode::NOT_FOUND,
        "the streams admin path is not proxied"
    );

    // Even the signalling path takes only a known camera's stream as its
    // source: go2rtc would otherwise patch the stream from whatever src
    // arrives. The quality suffix must be one of the two classes.
    for source in [
        "ffmpeg:rtsp://evil/x",
        "rtsp://evil/x",
        "nope",
        "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01-ultra",
        "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01",
    ] {
        let redirected = client
            .get(format!("{}/go2rtc/api/ws?src={source}", h.base))
            .send()
            .await
            .expect("bad src");
        assert_eq!(
            redirected.status(),
            reqwest::StatusCode::NOT_FOUND,
            "src={source} is refused"
        );
    }

    let doubled = client
        .get(format!(
            "{}/go2rtc/api/ws?src={DEVICE_ID}&src=rtsp://evil/x",
            h.base
        ))
        .send()
        .await
        .expect("doubled src");
    assert_eq!(
        doubled.status(),
        reqwest::StatusCode::NOT_FOUND,
        "two src values are refused rather than resolved"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn the_proxy_serves_the_hls_fallback() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    let browser = client();
    let listed = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);

    // The master playlist, for a stream this server minted.
    let master = browser
        .get(format!(
            "{}/go2rtc/api/stream.m3u8?src={DEVICE_ID}-high",
            h.base
        ))
        .send()
        .await
        .expect("the master playlist");
    assert_eq!(master.status(), reqwest::StatusCode::OK);
    let body = master.text().await.expect("a playlist");
    assert!(
        body.contains("api/hls/playlist.m3u8"),
        "the fallback's playlist came through: {body}"
    );

    // And the parts it names, by their opaque session id.
    let part = browser
        .get(format!(
            "{}/go2rtc/api/hls/segment.m4s?id=hls-1&n=0",
            h.base
        ))
        .send()
        .await
        .expect("a segment");
    assert_eq!(
        part.status(),
        reqwest::StatusCode::OK,
        "a segment with a session id is proxied"
    );

    // Everything a fallback must not reach: an id the caller made up is
    // shaped fine but go2rtc refuses it, and a src that is not a stream
    // this server minted never leaves here.
    let invented = browser
        .get(format!(
            "{}/go2rtc/api/stream.m3u8?src=ffmpeg:rtsp://evil/x",
            h.base
        ))
        .send()
        .await
        .expect("an invented src");
    assert_eq!(invented.status(), reqwest::StatusCode::NOT_FOUND);
    let shape = browser
        .get(format!("{}/go2rtc/api/hls/segment.m4s?id=a/b", h.base))
        .send()
        .await
        .expect("a shaped id");
    assert_eq!(shape.status(), reqwest::StatusCode::NOT_FOUND);
    let bare = browser
        .get(format!("{}/go2rtc/api/hls/segment.m4s", h.base))
        .send()
        .await
        .expect("no id");
    assert_eq!(bare.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn websocket_frames_are_piped_through_the_proxy() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    let browser = client();
    // The proxy only forwards a camera this server already knows.
    let listed = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras");
    assert_eq!(listed.status(), reqwest::StatusCode::OK);

    let url = format!(
        "{}/go2rtc/api/ws?src={DEVICE_ID}-high",
        h.base.replace("http://", "ws://")
    );
    let request = url.into_client_request().expect("a ws request");
    let (mut socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .expect("the proxy accepts the socket");

    socket
        .send(WsMessage::text("offer"))
        .await
        .expect("send a frame");
    let echoed = socket
        .next()
        .await
        .expect("a frame comes back")
        .expect("without an error");
    assert_eq!(
        echoed,
        WsMessage::text("offer"),
        "the frame round-trips through the proxy"
    );

    // go2rtc hanging up reaches the browser as a close, not a silent stall.
    let closed = tokio::time::timeout(Duration::from_secs(2), socket.next())
        .await
        .expect("the close arrives promptly")
        .expect("a close frame comes back")
        .expect("without an error");
    assert!(
        matches!(closed, WsMessage::Close(_)),
        "the browser side is closed when go2rtc drops: {closed:?}"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn the_spa_serves_index_and_revalidates_with_etags() {
    let h = harness().await;
    let client = client();
    let index = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../web/dist/index.html"),
    )
    .expect("web/dist/index.html exists (run `just build-web`)");

    let root = client
        .get(format!("{}/", h.base))
        .send()
        .await
        .expect("index");
    assert_eq!(root.status(), reqwest::StatusCode::OK);
    let etag = root
        .headers()
        .get(reqwest::header::ETAG)
        .expect("an etag")
        .to_str()
        .expect("a header value")
        .to_owned();
    assert_eq!(
        root.headers()
            .get(reqwest::header::CACHE_CONTROL)
            .expect("cache-control")
            .to_str()
            .expect("a header value"),
        "no-cache",
        "the bundle revalidates rather than sticking in a cache"
    );
    assert_eq!(
        root.headers()
            .get(reqwest::header::X_CONTENT_TYPE_OPTIONS)
            .expect("nosniff")
            .to_str()
            .expect("a header value"),
        "nosniff",
        "the SPA is not sniffed into another type"
    );
    assert_eq!(
        root.headers()
            .get(reqwest::header::CONTENT_SECURITY_POLICY)
            .expect("csp")
            .to_str()
            .expect("a header value"),
        "frame-ancestors 'none'",
        "nobody frames the house"
    );
    let body = root.text().await.expect("body");
    assert_eq!(body, index, "the built index is served");

    let route = client
        .get(format!("{}/some/client/route", h.base))
        .send()
        .await
        .expect("client route");
    assert_eq!(route.status(), reqwest::StatusCode::OK);
    assert_eq!(
        route.text().await.expect("body"),
        index,
        "client routes fall back to index.html"
    );

    let revalidated = client
        .get(format!("{}/", h.base))
        .header(reqwest::header::IF_NONE_MATCH, etag)
        .send()
        .await
        .expect("revalidate");
    assert_eq!(
        revalidated.status(),
        reqwest::StatusCode::NOT_MODIFIED,
        "a matching ETag answers 304"
    );

    let missing = client
        .get(format!("{}/missing.js", h.base))
        .send()
        .await
        .expect("missing asset");
    assert_eq!(
        missing.status(),
        reqwest::StatusCode::NOT_FOUND,
        "a missing asset is a 404, not the index"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}
