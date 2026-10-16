//! Test scaffolding shared by the server's integration tests.

#![allow(dead_code)]

pub mod mqtt;

/// A motion-episode store on a scratch path: tests that never read the
/// timeline still need one wired.
pub fn episodes_in(dir: &std::path::Path) -> std::sync::Arc<server::episodes::EpisodeStore> {
    std::sync::Arc::new(server::episodes::EpisodeStore::open(
        dir.join("episodes.json"),
        std::time::Duration::from_secs(30),
        std::time::Duration::from_hours(30 * 24),
    ))
}

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::ws::Message;
use axum::extract::{Query, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, put};
use tokio::net::TcpListener;

/// A fake go2rtc API: records the stream PUTs and DELETEs it receives and
/// answers GETs with the shape measured from go2rtc 1.9.14.
pub struct FakeApi {
    pub base: reqwest::Url,
    puts: Arc<Mutex<Vec<(String, String)>>>,
    deletes: Arc<Mutex<Vec<String>>>,
    put_status: Arc<AtomicU16>,
    streams_visible: Arc<AtomicBool>,
    streams_error: Arc<AtomicBool>,
    producers: Arc<AtomicBool>,
    viewers: Arc<Mutex<HashMap<String, usize>>>,
    /// How long a DELETE takes to answer, in milliseconds: a teardown that
    /// can be caught mid-flight.
    delete_delay: Arc<AtomicU64>,
}

impl FakeApi {
    pub async fn start() -> Self {
        let puts = Arc::new(Mutex::new(Vec::new()));
        let deletes = Arc::new(Mutex::new(Vec::new()));
        let put_status = Arc::new(AtomicU16::new(200));
        let streams_visible = Arc::new(AtomicBool::new(true));
        let streams_error = Arc::new(AtomicBool::new(false));
        let producers = Arc::new(AtomicBool::new(true));
        let viewers = Arc::new(Mutex::new(HashMap::new()));
        let delete_delay = Arc::new(AtomicU64::new(0));
        let app = hls_routes().merge(streams_routes(
            &puts,
            &deletes,
            &Knobs {
                put_status: &put_status,
                streams_visible: &streams_visible,
                streams_error: &streams_error,
                producers: &producers,
                delete_delay: &delete_delay,
            },
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
            streams_visible,
            streams_error,
            producers,
            viewers,
            delete_delay,
        }
    }

    /// Report every stream without a producer, as a go2rtc whose sources
    /// are configured but not running would.
    pub fn set_producers(&self, running: bool) {
        self.producers.store(running, Ordering::SeqCst);
    }

    /// Make every DELETE take `delay` to answer, as a go2rtc under load
    /// would: a teardown can then be caught mid-flight.
    pub fn set_delete_delay(&self, delay: Duration) {
        self.delete_delay.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
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

/// The answer-mode knobs a test turns, bundled so the fake's surfaces take
/// them as one argument.
struct Knobs<'a> {
    put_status: &'a Arc<AtomicU16>,
    streams_visible: &'a Arc<AtomicBool>,
    streams_error: &'a Arc<AtomicBool>,
    producers: &'a Arc<AtomicBool>,
    delete_delay: &'a Arc<AtomicU64>,
}

/// The streams admin surface, as go2rtc 1.9.14 answers it.
fn streams_routes(
    puts: &Arc<Mutex<Vec<(String, String)>>>,
    deletes: &Arc<Mutex<Vec<String>>>,
    knobs: &Knobs<'_>,
    viewers: &Arc<Mutex<HashMap<String, usize>>>,
) -> Router {
    Router::new()
        .route(
            "/api/frame.jpeg",
            get(|Query(params): Query<HashMap<String, String>>| async move {
                // A still, as go2rtc serves one for a running stream.
                match params.get("src") {
                    Some(_) => (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "image/jpeg")],
                        vec![0xFF_u8, 0xD8, 0xFF, 0xE0],
                    ),
                    None => (
                        StatusCode::NOT_FOUND,
                        [(axum::http::header::CONTENT_TYPE, "text/plain")],
                        b"stream not found".to_vec(),
                    ),
                }
            }),
        )
        .route("/api/ws", get(echo))
        .route(
            "/api/streams",
            streams_methods(puts, deletes, knobs, viewers),
        )
}

/// The streams endpoint's three methods, as go2rtc 1.9.14 answers them.
fn streams_methods(
    puts: &Arc<Mutex<Vec<(String, String)>>>,
    deletes: &Arc<Mutex<Vec<String>>>,
    knobs: &Knobs<'_>,
    viewers: &Arc<Mutex<HashMap<String, usize>>>,
) -> axum::routing::MethodRouter {
    let recorded = Arc::clone(puts);
    let status = Arc::clone(knobs.put_status);
    let put_stream = put(move |Query(params): Query<HashMap<String, String>>| {
        let recorded = Arc::clone(&recorded);
        let status = Arc::clone(&status);
        async move {
            recorded.lock().unwrap().push((
                params.get("name").cloned().unwrap_or_default(),
                params.get("src").cloned().unwrap_or_default(),
            ));
            let code = StatusCode::from_u16(status.load(Ordering::SeqCst)).expect("a status");
            // A rejected PUT quotes the source back, as go2rtc's own
            // refusals do - token and all.
            let body = if code.is_success() {
                String::new()
            } else {
                format!(
                    "streams: source from insecure producer: {}",
                    params.get("src").cloned().unwrap_or_default()
                )
            };
            (code, body)
        }
    });
    let dropped = Arc::clone(deletes);
    let delay = Arc::clone(knobs.delete_delay);
    let drop_stream =
        axum::routing::delete(move |Query(params): Query<HashMap<String, String>>| {
            let dropped = Arc::clone(&dropped);
            let delay = Arc::clone(&delay);
            async move {
                let millis = delay.load(Ordering::SeqCst);
                if millis > 0 {
                    tokio::time::sleep(Duration::from_millis(millis)).await;
                }
                dropped
                    .lock()
                    .unwrap()
                    .push(params.get("src").cloned().unwrap_or_default());
            }
        });
    let recorded = Arc::clone(puts);
    let visible = Arc::clone(knobs.streams_visible);
    let viewers = Arc::clone(viewers);
    let error = Arc::clone(knobs.streams_error);
    let running = Arc::clone(knobs.producers);
    let list_streams = get(move || {
        let recorded = Arc::clone(&recorded);
        let visible = Arc::clone(&visible);
        let viewers = Arc::clone(&viewers);
        let error = Arc::clone(&error);
        let running = Arc::clone(&running);
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
            // go2rtc materializes a producer as soon as the source is
            // set, which is what the fixture's unset case cannot show.
            let mut streams = serde_json::Map::new();
            for (name, source) in recorded {
                let watching = viewers.get(&name).copied().unwrap_or(0);
                // Measured shape: no viewers is `null`.
                let consumers = (watching > 0).then(|| {
                    (0..watching)
                        .map(|_| serde_json::json!({ "id": "viewer" }))
                        .collect::<Vec<_>>()
                });
                let held = running.load(Ordering::SeqCst);
                streams.insert(
                    name,
                    serde_json::json!({
                        "producers": if held {
                            vec![serde_json::json!({ "url": source })]
                        } else {
                            Vec::new()
                        },
                        "consumers": consumers,
                    }),
                );
            }
            serde_json::Value::Object(streams).to_string()
        }
    });
    put_stream.delete(drop_stream).get(list_streams)
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
