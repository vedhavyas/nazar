//! HTTP surface tests: the gate, the API, the go2rtc proxy and the SPA.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt as _, StreamExt as _};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::tungstenite::client::IntoClientRequest as _;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::{QuboClient, SessionStore};
use server::go2rtc::Go2rtc;
use server::http::{AppState, app};
use server::pin::PinGate;
use server::supervisor::{Pacing, Supervisor};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const PIN: &str = "2468";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const MINT_PATH: &str = "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/stream/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";

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
        },
    );
    let state = AppState::new(
        "test",
        sessions,
        client,
        supervisor,
        PinGate::new(&data_dir),
        go2rtc.base.to_string(),
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
    reqwest::Client::builder()
        .cookie_store(true)
        .build()
        .expect("a client")
}

/// First run: set the PIN, unlock, and hand back the `Set-Cookie` value.
async fn unlock(client: &reqwest::Client, base: &str) -> String {
    let response = client
        .post(format!("{base}/api/pin"))
        .json(&json!({ "pin": PIN }))
        .send()
        .await
        .expect("set pin");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NO_CONTENT,
        "first run sets the PIN"
    );
    let response = client
        .post(format!("{base}/api/unlock"))
        .json(&json!({ "pin": PIN }))
        .send()
        .await
        .expect("unlock");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NO_CONTENT,
        "the right PIN unlocks"
    );
    response
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .expect("a session cookie is set")
        .to_str()
        .expect("a header value")
        .to_owned()
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
async fn the_gate_locks_cameras_and_only_the_right_pin_unlocks() {
    let h = harness().await;
    let client = client();

    let response = client
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("locked cameras");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "cameras are gated"
    );

    let response = client
        .post(format!("{}/api/unlock", h.base))
        .json(&json!({ "pin": PIN }))
        .send()
        .await
        .expect("unlock before any pin exists");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::CONFLICT,
        "unlock tells the UI that no PIN is set yet"
    );

    mount_discovery(&h.qubo).await;
    let cookie = unlock(&client, &h.base).await;
    assert!(
        cookie.starts_with("nazar_session=")
            && cookie.contains("HttpOnly")
            && cookie.contains("SameSite=Lax"),
        "unlock hands out a HttpOnly, SameSite=Lax session cookie: {cookie}"
    );

    let response = client
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("unlocked cameras");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let cameras = response.json::<Value>().await.expect("json");
    assert_eq!(
        cameras,
        json!({
            "cameras": [{ "id": DEVICE_ID, "name": "Hall", "state": "offline", "reason": null }],
            "needsLogin": false,
        }),
        "the camera page carries the list and the login flag"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn a_refused_session_reads_as_needs_login() {
    let h = harness().await;
    mount_discovery(&h.qubo).await;
    let browser = client();
    unlock(&browser, &h.base).await;

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

    let refused = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("list under a dead session");
    assert_eq!(refused.status(), reqwest::StatusCode::OK);
    let page = refused.json::<Value>().await.expect("json");
    assert_eq!(
        page["needsLogin"],
        json!(true),
        "a refused session routes the UI to login"
    );
    assert_eq!(
        page["cameras"][0]["state"],
        json!("degraded"),
        "the last known camera survives the refused refresh"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn a_wrong_pin_is_refused_and_the_pin_is_first_run_only() {
    let h = harness().await;
    let browser = client();
    unlock(&browser, &h.base).await;

    let response = browser
        .post(format!("{}/api/pin", h.base))
        .json(&json!({ "pin": "9999" }))
        .send()
        .await
        .expect("second pin");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::CONFLICT,
        "the PIN cannot be overwritten in M1"
    );

    let fresh = client();
    let response = fresh
        .post(format!("{}/api/unlock", h.base))
        .json(&json!({ "pin": "9999" }))
        .send()
        .await
        .expect("wrong pin");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a wrong PIN is refused"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn the_unlock_throttle_caps_attempts_per_client() {
    let h = harness().await;
    let browser = client();
    unlock(&browser, &h.base).await;

    // The helper's unlock cleared the window, so five wrong PINs stay
    // inside the limit and the sixth try is the one refused.
    for attempt in 1..=5 {
        let response = browser
            .post(format!("{}/api/unlock", h.base))
            .json(&json!({ "pin": "0000" }))
            .send()
            .await
            .expect("wrong pin");
        assert_eq!(
            response.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "wrong pin {attempt} is refused as wrong, not as a flood"
        );
    }
    let flooded = browser
        .post(format!("{}/api/unlock", h.base))
        .json(&json!({ "pin": "0000" }))
        .send()
        .await
        .expect("sixth try");
    assert_eq!(
        flooded.status(),
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        "the sixth try in the window is rate-limited"
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

    let locked = client()
        .post(format!("{}/api/cameras/{DEVICE_ID}/live", h.base))
        .send()
        .await
        .expect("locked watch");
    assert_eq!(
        locked.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "watching is gated too"
    );

    // The first gated call discovers the account's cameras.
    unlock(&browser, &h.base).await;
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
        json!({ "stream": DEVICE_ID }),
        "the stream name is the device id"
    );

    let page = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras")
        .json::<Value>()
        .await
        .expect("json");
    assert_eq!(
        page["cameras"][0]["state"],
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
    unlock(&browser, &h.base).await;

    let _ = browser
        .post(format!("{}/api/cameras/{DEVICE_ID}/live", h.base))
        .send()
        .await
        .expect("watch");

    let page = browser
        .get(format!("{}/api/cameras", h.base))
        .send()
        .await
        .expect("cameras")
        .json::<Value>()
        .await
        .expect("json");
    assert_eq!(page["cameras"][0]["state"], json!("degraded"));
    assert_eq!(
        page["cameras"][0]["reason"],
        json!("auth error: User Token expired, Access denied"),
        "the server's reason reaches the browser verbatim"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn login_otp_is_gated_and_forwards_to_the_gateway() {
    let h = harness().await;
    Mock::given(method("POST"))
        .and(path(
            "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/login",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"uuid":"login-1"}"#))
        .mount(&h.qubo)
        .await;
    let browser = client();

    let locked = browser
        .post(format!("{}/api/login/otp", h.base))
        .json(&json!({ "phone": "9876543210" }))
        .send()
        .await
        .expect("locked otp");
    assert_eq!(
        locked.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "a locked peer cannot burn the resend window under the household number"
    );

    unlock(&browser, &h.base).await;
    let response = browser
        .post(format!("{}/api/login/otp", h.base))
        .json(&json!({ "phone": "9876543210" }))
        .send()
        .await
        .expect("otp");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::NO_CONTENT,
        "an unlocked browser forwards the request"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn verify_requires_the_unlock_and_saves_the_session() {
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
        .and(path(
            "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/verify",
        ))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            json!([{ "accessToken": "token-live", "uuid": "user-uuid-1", "email": "family@example.com" }])
                .to_string(),
        ))
        .mount(&h.qubo)
        .await;

    let client = client();
    let locked = client
        .post(format!("{}/api/login/verify", h.base))
        .json(&json!({ "phone": "9876543210", "code": "123456" }))
        .send()
        .await
        .expect("locked verify");
    assert_eq!(
        locked.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "the OTP alone does not get past the gate"
    );

    unlock(&client, &h.base).await;
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
async fn the_cookie_is_secure_only_behind_tls() {
    let h = harness().await;
    let client = client();

    let local = client
        .post(format!("{}/api/pin", h.base))
        .json(&json!({ "pin": PIN }))
        .send()
        .await
        .expect("set pin");
    assert_eq!(local.status(), reqwest::StatusCode::NO_CONTENT);
    let plain = client
        .post(format!("{}/api/unlock", h.base))
        .json(&json!({ "pin": PIN }))
        .send()
        .await
        .expect("unlock");
    let plain_cookie = plain
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .expect("a cookie")
        .to_str()
        .expect("a header value")
        .to_owned();
    assert!(
        !plain_cookie.contains("Secure"),
        "a plain-http run must keep a usable cookie: {plain_cookie}"
    );

    let behind_tls = client
        .post(format!("{}/api/unlock", h.base))
        .header("x-forwarded-proto", "https")
        .json(&json!({ "pin": PIN }))
        .send()
        .await
        .expect("unlock behind caddy");
    let secure_cookie = behind_tls
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .expect("a cookie")
        .to_str()
        .expect("a header value")
        .to_owned();
    assert!(
        secure_cookie.contains("Secure") && secure_cookie.contains("HttpOnly"),
        "behind TLS the cookie is Secure and HttpOnly: {secure_cookie}"
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
        .and(path(
            "/sms/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/user/phone/verify",
        ))
        .respond_with(
            ResponseTemplate::new(401)
                .set_body_string(include_str!("fixtures/error_auth_002.json")),
        )
        .mount(&h.qubo)
        .await;

    let browser = client();
    unlock(&browser, &h.base).await;
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
        "a gateway auth failure re-locks the browser"
    );
    assert_eq!(
        refused.json::<Value>().await.expect("json")["error"],
        json!("auth error: User Token expired, Access denied"),
        "the gateway's words ride the envelope"
    );
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn the_proxy_reaches_go2rtc_through_the_gate() {
    let h = harness().await;
    let client = client();

    let locked = client
        .get(format!("{}/go2rtc/api/streams", h.base))
        .send()
        .await
        .expect("locked proxy");
    assert_eq!(
        locked.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "the proxy is gated"
    );

    unlock(&client, &h.base).await;
    let response = client
        .post(format!("{}/go2rtc/api/webrtc?src=cam", h.base))
        .send()
        .await
        .expect("proxied sdp");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body = response.text().await.expect("body");
    assert_eq!(
        body, "sdp-answer",
        "the fake go2rtc answered through the proxy"
    );

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
    let _ = std::fs::remove_dir_all(&h.data_dir);
}

#[tokio::test]
async fn websocket_frames_are_piped_through_the_proxy() {
    let h = harness().await;
    let cookie = unlock(&client(), &h.base).await;
    let pair = cookie
        .split(';')
        .next()
        .expect("the cookie pair")
        .to_owned();

    let url = format!("{}/go2rtc/api/ws", h.base.replace("http://", "ws://"));
    let mut request = url.into_client_request().expect("a ws request");
    request
        .headers_mut()
        .insert("cookie", pair.parse().expect("a cookie header"));
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
    let closed = socket
        .next()
        .await
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
