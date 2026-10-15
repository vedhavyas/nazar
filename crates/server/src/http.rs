//! The HTTP surface: the JSON API, the PIN gate, the built SPA, and the
//! go2rtc proxy the browser's WebRTC signalling rides.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts as _, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt as _, StreamExt as _};
use rust_embed::Embed;
use serde::Deserialize;
use serde_json::{Value, json};

use qubo::{QuboClient, QuboError, SessionStore, request_otp, verify_otp};

use crate::pin::{PinError, PinGate};
use crate::supervisor::{StreamState, Supervisor, SupervisorError};

/// The built SPA. Debug builds read it from disk; release builds embed it.
#[derive(Embed)]
#[folder = "../../web/dist"]
struct Spa;

/// Bodies proxied to go2rtc are small (signalling and config); anything
/// bigger is a browser mistake, not a stream.
const PROXY_BODY_LIMIT: usize = 10 * 1024 * 1024;

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

/// Everything the routes share.
#[derive(Clone)]
pub struct AppState {
    pub version: &'static str,
    pub sessions: SessionStore,
    pub qubo: QuboClient,
    pub supervisor: Supervisor,
    pub pin: PinGate,
    /// go2rtc's API base the proxy forwards to.
    pub go2rtc_api: String,
    http: reqwest::Client,
}

impl AppState {
    pub fn new(
        version: &'static str,
        sessions: SessionStore,
        qubo: QuboClient,
        supervisor: Supervisor,
        pin: PinGate,
        go2rtc_api: impl Into<String>,
    ) -> Self {
        Self {
            version,
            sessions,
            qubo,
            supervisor,
            pin,
            go2rtc_api: go2rtc_api.into(),
            http: reqwest::Client::new(),
        }
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

    fn locked() -> Self {
        Self::new(StatusCode::UNAUTHORIZED, "locked")
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
        .route("/api/pin", post(set_pin))
        .route("/api/unlock", post(unlock))
        .route("/api/login/otp", post(login_otp))
        .route("/api/login/verify", post(login_verify))
        .route("/api/cameras", get(cameras))
        .route("/api/cameras/{id}/live", post(camera_live))
        .route("/go2rtc/{*rest}", any(proxy))
        .fallback(spa)
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<Value> {
    Json(json!({ "ok": true, "version": state.version }))
}

#[derive(Deserialize)]
struct PinRequest {
    pin: String,
}

async fn set_pin(
    State(state): State<AppState>,
    Json(body): Json<PinRequest>,
) -> Result<StatusCode, ApiError> {
    state.pin.set(&body.pin).map_err(|err| match err {
        PinError::AlreadySet => ApiError::new(StatusCode::CONFLICT, err.to_string()),
        other => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unlock(
    State(state): State<AppState>,
    Json(body): Json<PinRequest>,
) -> Result<Response, ApiError> {
    let token = state.pin.unlock(&body.pin).map_err(|err| match err {
        PinError::Wrong | PinError::NotSet => {
            ApiError::new(StatusCode::UNAUTHORIZED, err.to_string())
        }
        other => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
    })?;
    Ok(with_session_cookie(StatusCode::NO_CONTENT, &token))
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

/// A browser must be unlocked before it may log the server in: the PIN is
/// the device gate, and the OTP alone does not grant one.
async fn login_verify(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<VerifyRequest>,
) -> Result<Response, ApiError> {
    require_unlocked(&state, &headers)?;
    let session = verify_otp(&state.qubo, &body.phone, &body.code).await?;
    state
        .sessions
        .save(&session)
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if let Err(err) = state.supervisor.refresh_devices().await {
        tracing::warn!(%err, "device discovery after login failed");
    }
    Ok(with_session_cookie(
        StatusCode::NO_CONTENT,
        &state.pin.mint_session(),
    ))
}

async fn cameras(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    require_unlocked(&state, &headers)?;
    ensure_devices(&state).await?;

    let mut cameras = Vec::new();
    for device in state.supervisor.devices().await {
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
    Ok(Json(json!(cameras)))
}

async fn camera_live(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    require_unlocked(&state, &headers)?;
    ensure_devices(&state).await?;
    state.supervisor.ensure_live(&id).await?;
    Ok(Json(json!({ "stream": id })))
}

/// Discovery is lazy: the first gated call after boot or login fills the
/// supervisor's camera list.
async fn ensure_devices(state: &AppState) -> Result<(), ApiError> {
    if state.supervisor.devices().await.is_empty() {
        state.supervisor.refresh_devices().await?;
    }
    Ok(())
}

/// Forward `/go2rtc/*` to go2rtc's loopback API, HTTP and WebSocket alike.
async fn proxy(State(state): State<AppState>, request: Request) -> Result<Response, ApiError> {
    let (mut parts, body) = request.into_parts();
    require_unlocked(&state, &parts.headers)?;

    let rest = parts.uri.path().trim_start_matches("/go2rtc/").to_owned();
    let query = parts
        .uri
        .query()
        .map_or(String::new(), |query| format!("?{query}"));
    let base = state.go2rtc_api.trim_end_matches('/');
    let target = format!("{base}/{rest}{query}");

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
                    if upstream_tx.send(to_upstream(message)).await.is_err() {
                        break;
                    }
                }
                _ => break,
            },
            message = upstream_rx.next() => match message {
                Some(Ok(message)) => {
                    if browser_tx.send(to_browser(message)).await.is_err() {
                        break;
                    }
                }
                _ => break,
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

fn with_session_cookie(status: StatusCode, token: &str) -> Response {
    let cookie = format!("nazar_session={token}; Path=/; HttpOnly; SameSite=Lax");
    ([(header::SET_COOKIE, cookie)], status).into_response()
}

fn require_unlocked(state: &AppState, headers: &HeaderMap) -> Result<(), ApiError> {
    match session_token(headers) {
        Some(token) if state.pin.is_unlocked(&token) => Ok(()),
        _ => Err(ApiError::locked()),
    }
}

fn session_token(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies
        .split(';')
        .find_map(|part| part.trim().strip_prefix("nazar_session="))
        .map(str::to_owned)
}
