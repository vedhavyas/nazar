//! The HTTP surface: the JSON API, the built SPA, and the go2rtc proxy the
//! browser's WebRTC signalling rides.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts as _, Path, Query, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt as _, StreamExt as _};
use rust_embed::Embed;
use serde::Deserialize;
use serde_json::{Value, json};

use qubo::{
    Device, Quality, QuboClient, QuboError, Session, SessionStore, read_shadow, request_otp,
    verify_otp, write_service,
};

use crate::events::EventBus;
use crate::motion::{Hysteresis, MotionOrder};
use crate::mqtt::UnitEvent;
use crate::supervisor::{StreamState, Supervisor, SupervisorError, stream_name};

/// The built SPA. Debug builds read it from disk; release builds embed it.
#[derive(Embed)]
#[folder = "../../web/dist"]
struct Spa;

/// Bodies proxied to go2rtc are small (signalling and config); anything
/// bigger is a browser mistake, not a stream.
const PROXY_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// The go2rtc paths the browser's player may reach: its signalling socket,
/// and the HLS fallback's parts (design §5). Everything else go2rtc serves
/// is an admin surface.
const PROXY_WS_PATH: &str = "api/ws";
const PROXY_HLS_MASTER: &str = "api/stream.m3u8";
const PROXY_HLS_PARTS: [&str; 4] = [
    "api/hls/playlist.m3u8",
    "api/hls/segment.ts",
    "api/hls/segment.m4s",
    "api/hls/init.mp4",
];

/// The WebSocket client wants a `ws://` URL; the API base is `http://`.
fn ws_target(target: &str) -> String {
    match target.strip_prefix("http") {
        Some(rest) => format!("ws{rest}"),
        None => target.to_owned(),
    }
}

fn is_websocket(headers: &HeaderMap) -> bool {
    headers
        .get(header::UPGRADE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
}

/// How long a control write is given to show up in the shadow.
#[derive(Debug, Clone, Copy)]
pub struct ControlTiming {
    pub poll_every: Duration,
    pub window: Duration,
}

impl Default for ControlTiming {
    fn default() -> Self {
        Self {
            poll_every: Duration::from_millis(250),
            window: Duration::from_secs(2),
        }
    }
}

/// Everything the routes share.
#[derive(Clone)]
pub struct AppState {
    pub version: &'static str,
    pub sessions: SessionStore,
    pub qubo: QuboClient,
    pub supervisor: Supervisor,
    /// go2rtc's API base the proxy forwards to.
    pub go2rtc_api: String,
    pub control: ControlTiming,
    /// The broker events every live view follows.
    pub events: EventBus,
    /// The hero decision, fed by the event bus.
    pub motion: Arc<Mutex<MotionOrder>>,
    /// One lock per camera: a step is a read-modify-write, and two of them
    /// racing read the same base and lose one another's move.
    ptz_locks: Arc<Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    http: reqwest::Client,
}

/// What the app is wired to: the deployment's parts, in one literal.
pub struct Wiring {
    pub sessions: SessionStore,
    pub qubo: QuboClient,
    pub supervisor: Supervisor,
    pub go2rtc_api: String,
    pub events: EventBus,
    pub motion: Arc<Mutex<MotionOrder>>,
}

impl AppState {
    pub fn new(version: &'static str, wiring: Wiring) -> Self {
        Self {
            version,
            sessions: wiring.sessions,
            qubo: wiring.qubo,
            supervisor: wiring.supervisor,
            go2rtc_api: wiring.go2rtc_api,
            control: ControlTiming::default(),
            events: wiring.events,
            motion: wiring.motion,
            ptz_locks: Arc::new(Mutex::new(HashMap::new())),
            // No redirects on the proxy: a 3xx from go2rtc must reach the
            // browser as-is rather than be followed into another host.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|err| {
                    tracing::warn!(%err, "proxy client fell back to defaults");
                    reqwest::Client::new()
                }),
        }
    }
}

impl AppState {
    /// The camera's step lock, created on first use.
    fn ptz_lock(&self, id: &str) -> Arc<tokio::sync::Mutex<()>> {
        self.ptz_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(id.to_owned())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }
}

/// The API's error envelope: `{"error": "..."}`.
pub struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.status, Json(json!({ "error": self.message }))).into_response()
    }
}

impl From<QuboError> for ApiError {
    fn from(err: QuboError) -> Self {
        let status = if matches!(err, QuboError::Auth { .. }) {
            StatusCode::UNAUTHORIZED
        } else {
            StatusCode::BAD_GATEWAY
        };
        Self::new(status, err.to_string())
    }
}

impl From<SupervisorError> for ApiError {
    fn from(err: SupervisorError) -> Self {
        match err {
            SupervisorError::UnknownDevice(id) => {
                Self::new(StatusCode::NOT_FOUND, format!("unknown device {id}"))
            }
            SupervisorError::VodUnavailable => {
                Self::new(StatusCode::NOT_IMPLEMENTED, err.to_string())
            }
            SupervisorError::Qubo(err) => err.into(),
        }
    }
}

/// The app's routes.
pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/api/health", get(health))
        .route("/api/login/otp", post(login_otp))
        .route("/api/login/verify", post(login_verify))
        .route("/api/cameras", get(cameras))
        .route("/api/cameras/{id}/live", post(camera_live))
        .route("/api/cameras/{id}/ptz", post(camera_ptz))
        .route(
            "/api/cameras/{id}/privacy",
            get(camera_privacy_state).post(camera_privacy),
        )
        .route("/api/motion/hero", get(motion_hero))
        .route("/api/events", get(events_socket))
        .route("/go2rtc/{*rest}", any(proxy))
        .fallback(spa)
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "ok": true, "version": state.version }))
}

#[derive(Deserialize)]
struct OtpRequest {
    phone: String,
}

async fn login_otp(
    State(state): State<AppState>,
    Json(body): Json<OtpRequest>,
) -> Result<StatusCode, ApiError> {
    request_otp(&state.qubo, &body.phone).await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct VerifyRequest {
    phone: String,
    code: String,
}

async fn login_verify(
    State(state): State<AppState>,
    Json(body): Json<VerifyRequest>,
) -> Result<StatusCode, ApiError> {
    let session = verify_otp(&state.qubo, &body.phone, &body.code).await?;
    let sessions = state.sessions.clone();
    blocking(move || sessions.save(&session))
        .await?
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if let Err(err) = state.supervisor.refresh_devices().await {
        tracing::warn!(%err, "device discovery after login failed");
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn cameras(State(state): State<AppState>) -> Result<Json<Value>, ApiError> {
    ensure_devices(&state).await?;

    let mut cameras = Vec::new();
    for device in state.supervisor.devices().await {
        // The account carries more than cameras: purifiers and the app's
        // own row would render as useless offline tiles.
        if !device.is_camera() {
            continue;
        }
        let (name, reason) = match state.supervisor.status(&device.device_id).await {
            StreamState::Offline => ("offline", None),
            StreamState::Waking => ("waking", None),
            StreamState::Live { .. } => ("live", None),
            StreamState::Degraded { reason } => ("degraded", Some(reason)),
        };
        cameras.push(json!({
            "id": device.device_id,
            "name": device.name,
            "state": name,
            "reason": reason,
        }));
    }
    Ok(Json(json!({
        "cameras": cameras,
        "needsLogin": state.supervisor.needs_login(),
    })))
}

#[derive(Deserialize)]
struct LiveQuery {
    quality: Option<String>,
    /// The person is asking again - a Try again - and only that skips the
    /// supervisor's backoff.
    retry: Option<bool>,
}

async fn camera_live(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<LiveQuery>,
) -> Result<Json<Value>, ApiError> {
    let quality = match query.quality.as_deref() {
        None | Some("high") => Quality::High,
        Some("low") => Quality::Low,
        Some(other) => {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                format!("unknown quality {other}"),
            ));
        }
    };
    ensure_devices(&state).await?;
    state
        .supervisor
        .ensure(&id, quality, query.retry.unwrap_or(false))
        .await?;
    Ok(Json(json!({ "stream": stream_name(&id, quality) })))
}

/// The camera the house is watching: the supervisor's hysteresis decision,
/// never the browser's own ordering.
async fn motion_hero(State(state): State<AppState>) -> Json<Value> {
    let hero = state
        .motion
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .hero(Instant::now(), Hysteresis::default());
    Json(json!({ "hero": hero }))
}

/// Every broker event, as it happens, for the home to follow.
async fn events_socket(State(state): State<AppState>, upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(move |socket| pump_events(socket, state.events.subscribe()))
}

async fn pump_events(socket: WebSocket, mut events: tokio::sync::broadcast::Receiver<UnitEvent>) {
    let mut socket = socket;
    loop {
        let frame = match events.recv().await {
            Ok(UnitEvent::Motion { device_id }) => {
                json!({ "type": "motion", "device_id": device_id })
            }
            Ok(UnitEvent::Liveness { device_id, online }) => {
                json!({ "type": "liveness", "device_id": device_id, "online": online })
            }
            // A viewer that fell behind misses events rather than the
            // connection.
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return,
        };
        if socket
            .send(Message::Text(frame.to_string().into()))
            .await
            .is_err()
        {
            return;
        }
    }
}

#[derive(Deserialize)]
struct PtzStep {
    dx: i64,
    dy: i64,
}

/// The biggest step one request may carry, in camera units. The camera's
/// own range is not known to anyone (no reference carries one), so the
/// read-back stays the contract; this only stops a hand-rolled request
/// from throwing the lens across its arc in one write.
const MAX_STEP: i64 = 60;

/// The camera is asked to move by a delta from where it reports it is, and
/// the answer is the position the shadow holds afterwards - the camera's
/// ack says nothing either way.
async fn camera_ptz(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(step): Json<PtzStep>,
) -> Result<Json<Value>, ApiError> {
    if step.dx.abs() > MAX_STEP || step.dy.abs() > MAX_STEP {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("a step is at most {MAX_STEP} units"),
        ));
    }
    let (device, session) = control_target(&state, &id).await?;
    // The read and the write are one move: a second step waits its turn,
    // so its delta is added to the position the first one left behind.
    let turn = state.ptz_lock(&id);
    let _held = turn.lock().await;
    let (x, y) = read_shadow(&state.qubo, &session, &device)
        .await?
        .position()
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_GATEWAY,
                "the camera reports no pan/tilt position",
            )
        })?;
    let target = (x + step.dx, y + step.dy);
    let target_text = format!("{},{}", target.0, target.1);
    write_service(
        &state.qubo,
        &session,
        &device,
        "panTiltControl",
        &[("horizontalVerticalPostion", target_text.as_str())],
    )
    .await?;
    drop(_held);

    let landed = await_position(&state, &device, &session, target).await?;
    Ok(Json(match landed {
        Some(position) if position == target => json!({
            "ok": true,
            "position": { "x": position.0, "y": position.1 },
        }),
        last => json!({
            "ok": false,
            "reason": NOT_APPLIED,
            "position": last.map(|(x, y)| json!({ "x": x, "y": y })),
        }),
    }))
}

#[derive(Deserialize)]
struct PrivacyOn {
    on: bool,
}

/// The lens, as the camera itself reports it: what the screen shows on
/// arrival, before any write has happened.
async fn camera_privacy_state(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let (device, session) = control_target(&state, &id).await?;
    let privacy = read_shadow(&state.qubo, &session, &device)
        .await?
        .camera_enabled()
        .map(|enabled| !enabled);
    Ok(Json(json!({ "privacy": privacy })))
}

/// Privacy on means the lens is closed: `cameraFunction.state = disable`.
/// The answer carries the state the shadow reports, never the ack.
async fn camera_privacy(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(body): Json<PrivacyOn>,
) -> Result<Json<Value>, ApiError> {
    let (device, session) = control_target(&state, &id).await?;
    write_service(
        &state.qubo,
        &session,
        &device,
        "cameraFunction",
        &[("state", if body.on { "disable" } else { "enable" })],
    )
    .await?;

    let deadline = Instant::now() + state.control.window;
    let reported = loop {
        let reported = read_shadow(&state.qubo, &session, &device)
            .await?
            .camera_enabled()
            .map(|enabled| !enabled);
        if reported == Some(body.on) || Instant::now() >= deadline {
            break reported;
        }
        tokio::time::sleep(state.control.poll_every).await;
    };
    Ok(Json(match reported {
        Some(privacy) if privacy == body.on => json!({ "ok": true, "privacy": privacy }),
        last => json!({ "ok": false, "reason": NOT_APPLIED, "privacy": last }),
    }))
}

/// The reason every unconfirmed write comes back with.
const NOT_APPLIED: &str = "not applied";

/// Read the shadow until it reports `target`, within the control window;
/// the last position seen, matched or not.
async fn await_position(
    state: &AppState,
    device: &Device,
    session: &Session,
    target: (i64, i64),
) -> Result<Option<(i64, i64)>, ApiError> {
    let deadline = Instant::now() + state.control.window;
    loop {
        let position = read_shadow(&state.qubo, session, device).await?.position();
        if position == Some(target) || Instant::now() >= deadline {
            return Ok(position);
        }
        tokio::time::sleep(state.control.poll_every).await;
    }
}

/// Resolve a camera id to the device and the session a control write
/// needs.
async fn control_target(state: &AppState, id: &str) -> Result<(Device, Session), ApiError> {
    ensure_devices(state).await?;
    let device = state
        .supervisor
        .devices()
        .await
        .into_iter()
        .find(|device| device.device_id == id)
        .ok_or_else(|| SupervisorError::UnknownDevice(id.to_owned()))?;
    let sessions = state.sessions.clone();
    let session = blocking(move || sessions.load())
        .await?
        .map_err(ApiError::from)?
        .ok_or_else(|| ApiError::new(StatusCode::UNAUTHORIZED, "no session; log in again"))?;
    Ok((device, session))
}

/// Discovery is lazy: the first gated call after boot or login fills the
/// supervisor's camera list, and a session Qubo has refused is re-tried so
/// recovery is noticed. An auth failure here is not the request's error -
/// the `needsLogin` flag carries it and the UI routes to login.
async fn ensure_devices(state: &AppState) -> Result<(), ApiError> {
    let missing = state.supervisor.devices().await.is_empty();
    let retry_refused = state.supervisor.needs_login() && state.supervisor.discovery_due();
    if (missing || retry_refused)
        && let Err(err) = state.supervisor.refresh_devices().await
        && !state.supervisor.needs_login()
    {
        return Err(err.into());
    }
    Ok(())
}

/// Forward `/go2rtc/*` to go2rtc's loopback API, HTTP and WebSocket alike.
async fn proxy(State(state): State<AppState>, request: Request) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();

    let rest = parts.uri.path().trim_start_matches("/go2rtc/").to_owned();
    let query = parts.uri.query().unwrap_or_default();
    let base = state.go2rtc_api.trim_end_matches('/').to_owned();
    // HLS's parts carry an opaque session id that only a master playlist
    // can create: they read, they never name a source, so an id that is
    // shaped like an id is all they need.
    let target = if PROXY_HLS_PARTS.contains(&rest.as_str()) {
        let id = single_param(query, "id")
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "only ?id=<session> is proxied"))?;
        let safe = !id.is_empty()
            && id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
        if !safe {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "not found"));
        }
        format!("{base}/{rest}?id={id}")
    } else if rest == PROXY_WS_PATH || rest == PROXY_HLS_MASTER {
        // go2rtc patches (or creates) streams from a `src`, so the query is
        // rebuilt rather than forwarded: exactly one src, and only a stream
        // this server already minted. Everything else the caller sent is
        // dropped, and the value is never percent-decoded - `ffmpeg%3A...`
        // must not become a source go2rtc would accept.
        let src = single_src(query)
            .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "only ?src=<camera> is proxied"))?;
        // The id must also be shaped like an id: a URL-shaped one must never
        // reach go2rtc, whatever the cloud marked it as.
        let safe = !src.is_empty()
            && src
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
        if !safe {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "unknown camera"));
        }
        // A stream is a camera plus its quality class, and only those two.
        let Some((camera, quality)) = src.rsplit_once('-') else {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "unknown camera"));
        };
        if !matches!(quality, "high" | "low") {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "unknown camera"));
        }
        let known = state
            .supervisor
            .devices()
            .await
            .iter()
            .any(|device| device.device_id == camera);
        if !known {
            return Err(ApiError::new(StatusCode::NOT_FOUND, "unknown camera"));
        }
        if rest == PROXY_WS_PATH {
            format!("{base}/api/ws?src={src}")
        } else {
            format!("{base}/api/stream.m3u8?src={src}")
        }
    } else {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not found"));
    };

    if is_websocket(&parts.headers) {
        let upgrade = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
            Ok(upgrade) => upgrade,
            Err(rejection) => return Ok(rejection.into_response()),
        };
        return Ok(upgrade.on_upgrade(move |browser| pipe(browser, ws_target(&target))));
    }

    let mut request = state.http.request(parts.method, &target);
    for (name, value) in &parts.headers {
        if name == header::HOST || name == header::COOKIE || name == header::CONTENT_LENGTH {
            continue;
        }
        request = request.header(name, value);
    }
    let body = axum::body::to_bytes(body, PROXY_BODY_LIMIT)
        .await
        .map_err(|err| ApiError::new(StatusCode::BAD_REQUEST, err.to_string()))?;
    let response = request
        .body(body)
        .send()
        .await
        .map_err(|err| ApiError::new(StatusCode::BAD_GATEWAY, err.to_string()))?;
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let bytes = response
        .bytes()
        .await
        .map_err(|err| ApiError::new(StatusCode::BAD_GATEWAY, err.to_string()))?;

    let mut builder = Response::builder().status(status);
    if let Some(content_type) = content_type {
        builder = builder.header(header::CONTENT_TYPE, content_type);
    }
    builder
        .body(Body::from(bytes))
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}

/// Pipe frames between the browser's socket and go2rtc's, verbatim.
async fn pipe(browser: WebSocket, target: String) {
    let upstream = match tokio_tungstenite::connect_async(&target).await {
        Ok((upstream, _)) => upstream,
        Err(err) => {
            tracing::warn!(%err, %target, "go2rtc websocket connect failed");
            return;
        }
    };
    let (mut browser_tx, mut browser_rx) = browser.split();
    let (mut upstream_tx, mut upstream_rx) = upstream.split();

    loop {
        tokio::select! {
            message = browser_rx.next() => match message {
                Some(Ok(message)) => {
                    if let Err(err) = upstream_tx.send(to_upstream(message)).await {
                        tracing::warn!(%err, %target, "browser frame could not reach go2rtc");
                        let _ = browser_tx.send(Message::Close(None)).await;
                        break;
                    }
                }
                Some(Err(err)) => {
                    tracing::warn!(%err, %target, "browser socket failed");
                    let _ = upstream_tx
                        .send(tokio_tungstenite::tungstenite::Message::Close(None))
                        .await;
                    break;
                }
                None => {
                    let _ = upstream_tx
                        .send(tokio_tungstenite::tungstenite::Message::Close(None))
                        .await;
                    break;
                }
            },
            message = upstream_rx.next() => match message {
                Some(Ok(message)) => {
                    if let Err(err) = browser_tx.send(to_browser(message)).await {
                        tracing::warn!(%err, %target, "go2rtc frame could not reach the browser");
                        let _ = upstream_tx
                            .send(tokio_tungstenite::tungstenite::Message::Close(None))
                            .await;
                        break;
                    }
                }
                Some(Err(err)) => {
                    tracing::warn!(%err, %target, "go2rtc socket failed");
                    let _ = browser_tx.send(Message::Close(None)).await;
                    break;
                }
                None => {
                    let _ = browser_tx.send(Message::Close(None)).await;
                    break;
                }
            },
        }
    }
}

fn to_upstream(message: Message) -> tokio_tungstenite::tungstenite::Message {
    use tokio_tungstenite::tungstenite::Message as Upstream;
    match message {
        Message::Text(text) => Upstream::Text(text.to_string().into()),
        Message::Binary(bytes) => Upstream::Binary(bytes.to_vec().into()),
        Message::Ping(bytes) => Upstream::Ping(bytes.to_vec().into()),
        Message::Pong(bytes) => Upstream::Pong(bytes.to_vec().into()),
        Message::Close(_) => Upstream::Close(None),
    }
}

fn to_browser(message: tokio_tungstenite::tungstenite::Message) -> Message {
    use tokio_tungstenite::tungstenite::Message as Upstream;
    match message {
        Upstream::Text(text) => Message::text(text.to_string()),
        Upstream::Binary(bytes) => Message::binary(bytes.to_vec()),
        Upstream::Ping(bytes) => Message::Ping(bytes.to_vec().into()),
        Upstream::Pong(bytes) => Message::Pong(bytes.to_vec().into()),
        // `Frame` is a raw-protocol variant the browser side never sees.
        Upstream::Close(_) | Upstream::Frame(_) => Message::Close(None),
    }
}

/// The SPA: real assets, then `index.html` for client routes.
async fn spa(headers: HeaderMap, uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    let path = if path.is_empty() { "index.html" } else { path };
    if let Some(asset) = asset(path, &headers) {
        return asset;
    }
    if path.contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    match asset("index.html", &headers) {
        Some(asset) => asset,
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "the web bundle is missing",
        )
            .into_response(),
    }
}

/// An embedded asset, served `no-cache` with a content ETag so the hub's
/// caddy can revalidate rather than hold a stale bundle.
fn asset(path: &str, request_headers: &HeaderMap) -> Option<Response> {
    let file = Spa::get(path)?;
    let etag = etag(&file.data);
    let fresh = request_headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
        == Some(etag.as_str());

    Response::builder()
        .status(if fresh {
            StatusCode::NOT_MODIFIED
        } else {
            StatusCode::OK
        })
        .header(header::CONTENT_TYPE, mime_for(path))
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::ETAG, etag)
        .header(header::X_CONTENT_TYPE_OPTIONS, "nosniff")
        .header(header::CONTENT_SECURITY_POLICY, "frame-ancestors 'none'")
        .body(if fresh {
            Body::empty()
        } else {
            Body::from(file.data.to_vec())
        })
        .ok()
}

fn etag(bytes: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    bytes.hash(&mut hasher);
    format!("\"{:x}\"", hasher.finish())
}

fn mime_for(path: &str) -> &'static str {
    match path.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript",
        Some("css") => "text/css",
        Some("json" | "map") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}

/// The one `src` value in a raw query, or nothing.
fn single_src(query: &str) -> Option<String> {
    single_param(query, "src")
}

/// The one `name` value in a raw query, or nothing when it repeats.
fn single_param(query: &str, wanted: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name != wanted {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(value.to_owned());
    }
    found
}

/// Run blocking work (fsyncs) off the runtime's worker threads so a
/// session write cannot stall every other request.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
}
