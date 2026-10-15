//! The HTTP surface: the JSON API, the PIN gate, the built SPA, and the
//! go2rtc proxy the browser's WebRTC signalling rides.

use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash as _, Hasher as _};
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::sync::Semaphore;

use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, FromRequestParts as _, Path, Request, State};
use axum::http::{HeaderMap, StatusCode, Uri, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use axum::{Json, Router};
use futures_util::{SinkExt as _, StreamExt as _};
use rust_embed::Embed;
use serde::Deserialize;
use serde_json::{Value, json};

use qubo::{QuboClient, QuboError, SessionStore, request_otp, verify_otp};

use crate::config::TrustedProxies;
use crate::pin::{PinError, PinGate};
use crate::supervisor::{StreamState, Supervisor, SupervisorError};

/// The built SPA. Debug builds read it from disk; release builds embed it.
#[derive(Embed)]
#[folder = "../../web/dist"]
struct Spa;

/// Bodies proxied to go2rtc are small (signalling and config); anything
/// bigger is a browser mistake, not a stream.
const PROXY_BODY_LIMIT: usize = 10 * 1024 * 1024;

/// The only go2rtc path the browser's player may reach: its signalling
/// socket. Everything else go2rtc serves is an admin surface.
const PROXY_WS_PATH: &str = "api/ws";

/// A fixed-window attempt limit, keyed by client: a short PIN must not be
/// brute-forceable, and each verify costs a 19 MiB hash. A house-wide
/// ceiling sits beside the per-client one so rotating keys cannot buy
/// unlimited guesses.
struct Throttle {
    limit: u32,
    global_limit: u32,
    window: Duration,
    attempts: Mutex<HashMap<String, Attempts>>,
    global: Mutex<Attempts>,
    permits: Semaphore,
}

struct Attempts {
    count: u32,
    until: Instant,
}

impl Throttle {
    fn new(limit: u32, global_limit: u32, window: Duration, concurrent: usize) -> Self {
        Self {
            limit,
            global_limit,
            window,
            attempts: Mutex::new(HashMap::new()),
            global: Mutex::new(Attempts {
                count: 0,
                until: Instant::now(),
            }),
            permits: Semaphore::new(concurrent),
        }
    }

    /// Count one attempt, or say how long the caller must wait.
    fn allow(&self, key: &str) -> Result<(), Duration> {
        let now = Instant::now();
        {
            let mut global = self.lock_global();
            if now >= global.until {
                *global = Attempts {
                    count: 0,
                    until: now + self.window,
                };
            }
            if global.count >= self.global_limit {
                return Err(global.until - now);
            }
            global.count += 1;
        }

        let mut attempts = self.lock_attempts();
        if attempts.len() > 64 {
            attempts.retain(|_, entry| entry.until > now);
        }
        let entry = attempts.entry(key.to_owned()).or_insert(Attempts {
            count: 0,
            until: now + self.window,
        });
        if now >= entry.until {
            *entry = Attempts {
                count: 0,
                until: now + self.window,
            };
        }
        if entry.count >= self.limit {
            return Err(entry.until - now);
        }
        entry.count += 1;
        Ok(())
    }

    /// Forget a client's failures once it has proven itself.
    fn clear(&self, key: &str) {
        self.lock_attempts().remove(key);
    }

    fn lock_attempts(&self) -> std::sync::MutexGuard<'_, HashMap<String, Attempts>> {
        self.attempts
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn lock_global(&self) -> std::sync::MutexGuard<'_, Attempts> {
        self.global
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The client the rate limiter counts against: the first `X-Forwarded-For`
/// hop, but only when the transport peer is a configured proxy - otherwise
/// anyone reaching the server directly could pick their own bucket by
/// setting the header. A stranger peer keys as itself.
fn client_key(headers: &HeaderMap, peer: IpAddr, trusted: &TrustedProxies) -> String {
    if trusted.trusts(peer)
        && let Some(forwarded) = headers
            .get("x-forwarded-for")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.split(',').next())
            .map(str::trim)
            .filter(|value| !value.is_empty())
    {
        return forwarded.to_owned();
    }
    peer.to_string()
}

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
    /// Whose `X-Forwarded-For` the throttle believes.
    pub trusted_proxies: TrustedProxies,
    /// Off only for local http runs; deployment is TLS-only.
    pub secure_cookies: bool,
    http: reqwest::Client,
    throttle: Arc<Throttle>,
}

/// What the app is wired to: the deployment's parts, in one literal.
pub struct Wiring {
    pub sessions: SessionStore,
    pub qubo: QuboClient,
    pub supervisor: Supervisor,
    pub pin: PinGate,
    pub go2rtc_api: String,
    pub trusted_proxies: TrustedProxies,
    pub secure_cookies: bool,
}

impl AppState {
    pub fn new(version: &'static str, wiring: Wiring) -> Self {
        Self {
            version,
            sessions: wiring.sessions,
            qubo: wiring.qubo,
            supervisor: wiring.supervisor,
            pin: wiring.pin,
            go2rtc_api: wiring.go2rtc_api,
            trusted_proxies: wiring.trusted_proxies,
            secure_cookies: wiring.secure_cookies,
            // No redirects on the proxy: a 3xx from go2rtc must reach the
            // browser as-is rather than be followed into another host.
            http: reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap_or_else(|err| {
                    tracing::warn!(%err, "proxy client fell back to defaults");
                    reqwest::Client::new()
                }),
            throttle: Arc::new(Throttle::new(5, 30, Duration::from_secs(60), 2)),
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
    let gate = state.pin.clone();
    blocking(move || gate.set(&body.pin))
        .await?
        .map_err(|err| match err {
            PinError::AlreadySet => ApiError::new(StatusCode::CONFLICT, err.to_string()),
            PinError::TooShort => ApiError::new(StatusCode::BAD_REQUEST, err.to_string()),
            other => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?;
    Ok(StatusCode::NO_CONTENT)
}

async fn unlock(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<PinRequest>,
) -> Result<Response, ApiError> {
    let client = client_key(&headers, peer.ip(), &state.trusted_proxies);
    if let Err(wait) = state.throttle.allow(&client) {
        tracing::warn!(client = %client, "unlock refused: too many tries");
        return Err(ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            format!("too many tries; wait {}s", wait.as_secs().max(1)),
        ));
    }
    let _verify = state.throttle.permits.try_acquire().map_err(|_| {
        ApiError::new(
            StatusCode::TOO_MANY_REQUESTS,
            "the server is busy unlocking; try again",
        )
    })?;

    let gate = state.pin.clone();
    let token = blocking(move || gate.unlock(&body.pin))
        .await?
        .map_err(|err| match err {
            // No PIN exists yet: the UI turns this into its first-run form.
            PinError::NotSet => ApiError::new(StatusCode::CONFLICT, err.to_string()),
            PinError::Wrong => {
                tracing::warn!(client = %client, "unlock refused: wrong PIN");
                ApiError::new(StatusCode::UNAUTHORIZED, err.to_string())
            }
            other => ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, other.to_string()),
        })?;
    state.throttle.clear(&client);
    Ok(with_session_cookie(
        StatusCode::NO_CONTENT,
        &token,
        state.secure_cookies,
    ))
}

#[derive(Deserialize)]
struct OtpRequest {
    phone: String,
}

/// Gated like the rest: a locked peer must not be able to burn the
/// 60-minute resend window or send SMS under the household's number.
async fn login_otp(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<OtpRequest>,
) -> Result<StatusCode, ApiError> {
    require_unlocked(&state, &headers)?;
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
    let sessions = state.sessions.clone();
    blocking(move || sessions.save(&session))
        .await?
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))?;
    if let Err(err) = state.supervisor.refresh_devices().await {
        tracing::warn!(%err, "device discovery after login failed");
    }
    Ok(with_session_cookie(
        StatusCode::NO_CONTENT,
        &state.pin.mint_session(),
        state.secure_cookies,
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
    Ok(Json(json!({
        "cameras": cameras,
        "needsLogin": state.supervisor.needs_login(),
    })))
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
    require_unlocked(&state, &parts.headers)?;

    let rest = parts.uri.path().trim_start_matches("/go2rtc/").to_owned();
    if rest != PROXY_WS_PATH {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "not found"));
    }
    // go2rtc patches (or creates) streams from api/ws's `src`, so the query
    // is rebuilt rather than forwarded: exactly one src, and only a camera
    // this server already knows. Everything else the caller sent is
    // dropped, and the value is never percent-decoded - `ffmpeg%3A...` must
    // not become a source go2rtc would accept.
    let src = single_src(parts.uri.query().unwrap_or_default())
        .ok_or_else(|| ApiError::new(StatusCode::NOT_FOUND, "only ?src=<camera> is proxied"))?;
    let known = state
        .supervisor
        .devices()
        .await
        .iter()
        .any(|device| device.device_id == src);
    if !known {
        return Err(ApiError::new(StatusCode::NOT_FOUND, "unknown camera"));
    }
    let base = state.go2rtc_api.trim_end_matches('/');
    let target = format!("{base}/api/ws?src={src}");

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

fn with_session_cookie(status: StatusCode, token: &str, secure: bool) -> Response {
    // Secure only when caddy terminated TLS: a plain-http local run would
    // otherwise have its cookie dropped by the browser.
    let secure = if secure { "; Secure" } else { "" };
    let cookie = format!("nazar_session={token}; Path=/; HttpOnly; SameSite=Lax{secure}");
    ([(header::SET_COOKIE, cookie)], status).into_response()
}

/// The one `src` value in a raw query, or nothing.
fn single_src(query: &str) -> Option<String> {
    let mut found: Option<String> = None;
    for pair in query.split('&') {
        let Some((name, value)) = pair.split_once('=') else {
            continue;
        };
        if name != "src" {
            continue;
        }
        if found.is_some() {
            return None;
        }
        found = Some(value.to_owned());
    }
    found
}

/// Run blocking work (Argon2 hashing, fsyncs) off the runtime's worker
/// threads so an unlock attempt cannot stall every other request.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|err| ApiError::new(StatusCode::INTERNAL_SERVER_ERROR, err.to_string()))
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
