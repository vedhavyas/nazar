//! Test scaffolding shared by the server's integration tests.

#![allow(dead_code)]

pub mod mqtt;

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::Message;
use axum::extract::{Query, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, put};
use serde_json::Value;
use tokio::net::TcpListener;

use qubo::Device;
use server::supervisor::{Lane, LaneSession};

/// The fast lane as the tests drive it: the arm outcome is scripted, arms
/// are counted, and the session records the control envelopes it is handed.
pub struct FakeLane {
    session: Arc<FakeSession>,
    arms: Arc<AtomicUsize>,
    outcome: Arc<Mutex<FakeOutcome>>,
}

#[derive(Clone)]
enum FakeOutcome {
    Ok,
    Fail(&'static str),
    Hang(Duration),
}

impl FakeLane {
    fn with(outcome: FakeOutcome) -> Self {
        Self {
            session: Arc::new(FakeSession {
                source: "ffmpeg:/tmp/nazar-fake.sdp#input=nazar".to_owned(),
                controls: Mutex::new(Vec::new()),
                alive: AtomicBool::new(true),
            }),
            arms: Arc::new(AtomicUsize::new(0)),
            outcome: Arc::new(Mutex::new(outcome)),
        }
    }

    /// An engine that arms instantly.
    pub fn new() -> Self {
        Self::with(FakeOutcome::Ok)
    }

    /// An engine whose arm always fails with `reason`.
    pub fn failing(reason: &'static str) -> Self {
        Self::with(FakeOutcome::Fail(reason))
    }

    /// An engine whose arm takes `delay` (a caller's bound has to cut it).
    pub fn hanging(delay: Duration) -> Self {
        Self::with(FakeOutcome::Hang(delay))
    }

    pub fn arms(&self) -> usize {
        self.arms.load(Ordering::SeqCst)
    }

    pub fn session(&self) -> Arc<FakeSession> {
        Arc::clone(&self.session)
    }
}

/// The session a [`FakeLane`] hands out.
pub struct FakeSession {
    pub source: String,
    pub controls: Mutex<Vec<Value>>,
    pub alive: AtomicBool,
}

impl FakeSession {
    pub fn controls(&self) -> Vec<Value> {
        self.controls.lock().unwrap().clone()
    }
}

impl Lane for FakeLane {
    fn arm(
        &self,
        _device: Device,
    ) -> Pin<Box<dyn Future<Output = Result<Arc<dyn LaneSession>, String>> + Send>> {
        self.arms.fetch_add(1, Ordering::SeqCst);
        // A fresh arm hands back a live session, whatever a previous one's
        // fate was: the real engine never returns a dead one.
        self.session.alive.store(true, Ordering::SeqCst);
        let outcome = self.outcome.lock().unwrap().clone();
        let session: Arc<dyn LaneSession> = self.session.clone();
        Box::pin(async move {
            match outcome {
                FakeOutcome::Ok => Ok(session),
                FakeOutcome::Fail(reason) => Err(reason.to_owned()),
                FakeOutcome::Hang(delay) => {
                    tokio::time::sleep(delay).await;
                    Ok(session)
                }
            }
        })
    }
}

impl LaneSession for FakeSession {
    fn source_for(&self, _name: &str) -> Result<String, String> {
        if self.alive() {
            Ok(self.source.clone())
        } else {
            Err("the session ended".to_owned())
        }
    }

    fn control(&self, envelope: Value) -> Result<(), String> {
        self.controls.lock().unwrap().push(envelope);
        Ok(())
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// A fake go2rtc API: records the stream PUTs and DELETEs it receives and
/// answers GETs with the shape measured from go2rtc 1.9.14.
pub struct FakeApi {
    pub base: reqwest::Url,
    puts: Arc<Mutex<Vec<(String, String)>>>,
    deletes: Arc<Mutex<Vec<String>>>,
    put_status: Arc<AtomicU16>,
    put_refused_marker: Arc<Mutex<Option<String>>>,
    streams_visible: Arc<AtomicBool>,
    streams_error: Arc<AtomicBool>,
    viewers: Arc<Mutex<HashMap<String, usize>>>,
}

impl FakeApi {
    pub async fn start() -> Self {
        let puts = Arc::new(Mutex::new(Vec::new()));
        let deletes = Arc::new(Mutex::new(Vec::new()));
        let put_status = Arc::new(AtomicU16::new(200));
        let put_refused_marker = Arc::new(Mutex::new(None));
        let streams_visible = Arc::new(AtomicBool::new(true));
        let streams_error = Arc::new(AtomicBool::new(false));
        let viewers = Arc::new(Mutex::new(HashMap::new()));
        let app = hls_routes().merge(streams_routes(
            &puts,
            &deletes,
            &put_status,
            &put_refused_marker,
            &streams_visible,
            &streams_error,
            &viewers,
        ));
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an ephemeral port");
        let addr = listener.local_addr().expect("bound address");
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("the fake serves");
        });
        Self {
            base: format!("http://{addr}").parse().expect("a url"),
            puts,
            deletes,
            put_status,
            put_refused_marker,
            streams_visible,
            streams_error,
            viewers,
        }
    }

    /// Refuse PUTs whose source contains `marker` (accept the rest as
    /// configured), so a test can fail one source without failing all.
    pub fn set_put_refused_marker(&self, marker: Option<&str>) {
        *self
            .put_refused_marker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = marker.map(str::to_owned);
    }

    /// Report `count` viewers on the stream, as go2rtc does while a browser
    /// holds it open.
    pub fn set_viewers(&self, name: &str, count: usize) {
        self.viewers.lock().unwrap().insert(name.to_owned(), count);
    }

    /// Answer stream PUTs with `status` from now on.
    pub fn set_put_status(&self, status: u16) {
        self.put_status.store(status, Ordering::SeqCst);
    }

    /// Report nothing from GET /api/streams, as a go2rtc that acked a PUT
    /// it did not take would.
    pub fn set_streams_visible(&self, visible: bool) {
        self.streams_visible.store(visible, Ordering::SeqCst);
    }

    /// Answer GET /api/streams with a body nobody can read, as a go2rtc in
    /// trouble would.
    pub fn set_streams_error(&self, broken: bool) {
        self.streams_error.store(broken, Ordering::SeqCst);
    }

    /// The stream PUTs received so far, as `(name, source)`.
    pub fn puts(&self) -> Vec<(String, String)> {
        self.puts.lock().unwrap().clone()
    }

    /// The stream names dropped so far.
    pub fn deletes(&self) -> Vec<String> {
        self.deletes.lock().unwrap().clone()
    }
}

/// The streams admin surface, as go2rtc 1.9.14 answers it.
fn streams_routes(
    puts: &Arc<Mutex<Vec<(String, String)>>>,
    deletes: &Arc<Mutex<Vec<String>>>,
    put_status: &Arc<AtomicU16>,
    put_refused_marker: &Arc<Mutex<Option<String>>>,
    streams_visible: &Arc<AtomicBool>,
    streams_error: &Arc<AtomicBool>,
    viewers: &Arc<Mutex<HashMap<String, usize>>>,
) -> Router {
    Router::new().route("/api/ws", get(echo)).route(
        "/api/streams",
        put({
            let recorded = Arc::clone(puts);
            let status = Arc::clone(put_status);
            let refused = Arc::clone(put_refused_marker);
            move |Query(params): Query<HashMap<String, String>>| {
                let recorded = Arc::clone(&recorded);
                let status = Arc::clone(&status);
                let refused = Arc::clone(&refused);
                async move {
                    let source = params.get("src").cloned().unwrap_or_default();
                    recorded.lock().unwrap().push((
                        params.get("name").cloned().unwrap_or_default(),
                        source.clone(),
                    ));
                    let marker = refused
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .clone();
                    let code = if marker.is_some_and(|marker| source.contains(&marker)) {
                        StatusCode::INTERNAL_SERVER_ERROR
                    } else {
                        StatusCode::from_u16(status.load(Ordering::SeqCst)).expect("a status")
                    };
                    // A rejected PUT quotes the source back, as
                    // go2rtc's own refusals do - token and all.
                    let body = if code.is_success() {
                        String::new()
                    } else {
                        format!("streams: source from insecure producer: {source}")
                    };
                    (code, body)
                }
            }
        })
        .delete({
            let dropped = Arc::clone(deletes);
            move |Query(params): Query<HashMap<String, String>>| {
                let dropped = Arc::clone(&dropped);
                async move {
                    dropped
                        .lock()
                        .unwrap()
                        .push(params.get("src").cloned().unwrap_or_default());
                }
            }
        })
        .get({
            let recorded = Arc::clone(puts);
            let visible = Arc::clone(streams_visible);
            let viewers = Arc::clone(viewers);
            let error = Arc::clone(streams_error);
            move || {
                let recorded = Arc::clone(&recorded);
                let visible = Arc::clone(&visible);
                let viewers = Arc::clone(&viewers);
                let error = Arc::clone(&error);
                async move {
                    // An unreadable status: what a go2rtc in trouble answers.
                    if error.load(Ordering::SeqCst) {
                        return String::new();
                    }
                    let recorded = recorded.lock().unwrap().clone();
                    if recorded.is_empty() {
                        return include_str!("../fixtures/streams.json").to_owned();
                    }
                    if !visible.load(Ordering::SeqCst) {
                        return "{}".to_owned();
                    }
                    let viewers = viewers.lock().unwrap().clone();
                    // go2rtc materializes a producer as soon as the
                    // source is set, which is what the fixture's
                    // unset case cannot show.
                    let mut streams = serde_json::Map::new();
                    for (name, source) in recorded {
                        let watching = viewers.get(&name).copied().unwrap_or(0);
                        // Measured shape: no viewers is `null`.
                        let consumers = (watching > 0).then(|| {
                            (0..watching)
                                .map(|_| serde_json::json!({ "id": "viewer" }))
                                .collect::<Vec<_>>()
                        });
                        streams.insert(
                            name,
                            serde_json::json!({
                                "producers": [{ "url": source }],
                                "consumers": consumers,
                            }),
                        );
                    }
                    serde_json::Value::Object(streams).to_string()
                }
            }
        }),
    )
}

/// The HLS fallback's endpoints, as go2rtc 1.9.14 answers them.
fn hls_routes() -> Router {
    Router::new()
        .route(
            "/api/stream.m3u8",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                // go2rtc's master playlist: the media playlist is a part.
                match params.get("src") {
                    Some(_) => (
                        StatusCode::OK,
                        "#EXTM3U\napi/hls/playlist.m3u8?id=hls-1\n".to_owned(),
                    ),
                    None => (StatusCode::NOT_FOUND, "stream not found".to_owned()),
                }
            }),
        )
        .route(
            "/api/hls/playlist.m3u8",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                match params.get("id") {
                    Some(_) => (
                        StatusCode::OK,
                        "#EXTM3U\nsegment.m4s?id=hls-1&n=0\n".to_owned(),
                    ),
                    None => (StatusCode::NOT_FOUND, "not found".to_owned()),
                }
            }),
        )
        .route(
            "/api/hls/segment.m4s",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                match params.get("id") {
                    Some(_) => (StatusCode::OK, "m4s".to_owned()),
                    None => (StatusCode::NOT_FOUND, "not found".to_owned()),
                }
            }),
        )
}

/// Echo one frame back then hang up, standing in for go2rtc's signalling
/// socket at its most abrupt.
async fn echo(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket| async move {
        if let Some(Ok(message)) = socket.recv().await {
            let _ = socket.send(message).await;
        }
        let _ = socket.send(Message::Close(None)).await;
    })
}

/// Poll `check` for about two seconds.
pub async fn eventually<F, Fut>(mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    for _ in 0..100 {
        if check().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}
