//! Test scaffolding shared by the server's integration tests.

#![allow(dead_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::{Query, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{any, get, put};
use tokio::net::TcpListener;

/// A fake go2rtc API: records the stream PUTs it receives and answers GETs
/// with the shape measured from go2rtc 1.9.14.
pub struct FakeApi {
    pub base: reqwest::Url,
    puts: Arc<Mutex<Vec<(String, String)>>>,
    put_status: Arc<AtomicU16>,
}

impl FakeApi {
    pub async fn start() -> Self {
        let puts = Arc::new(Mutex::new(Vec::new()));
        let put_status = Arc::new(AtomicU16::new(200));
        let app = Router::new()
            .route(
                "/api/streams",
                put({
                    let recorded = Arc::clone(&puts);
                    let status = Arc::clone(&put_status);
                    move |Query(params): Query<HashMap<String, String>>| {
                        let recorded = Arc::clone(&recorded);
                        let status = Arc::clone(&status);
                        async move {
                            recorded.lock().unwrap().push((
                                params.get("name").cloned().unwrap_or_default(),
                                params.get("src").cloned().unwrap_or_default(),
                            ));
                            let code = StatusCode::from_u16(status.load(Ordering::SeqCst))
                                .expect("a status");
                            let body = if code.is_success() {
                                String::new()
                            } else {
                                "streams: source from insecure producer".to_owned()
                            };
                            (code, body)
                        }
                    }
                })
                .get(|| async { include_str!("../fixtures/streams.json") }),
            )
            .route("/api/ws", get(echo))
            // The SDP exchange the proxy is allowed to carry.
            .route("/api/webrtc", any(|| async { "sdp-answer" }));
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
            put_status,
        }
    }

    /// Answer stream PUTs with `status` from now on.
    pub fn set_put_status(&self, status: u16) {
        self.put_status.store(status, Ordering::SeqCst);
    }

    /// The stream PUTs received so far, as `(name, source)`.
    pub fn puts(&self) -> Vec<(String, String)> {
        self.puts.lock().unwrap().clone()
    }
}

/// Echo every frame back, standing in for go2rtc's signalling socket.
async fn echo(ws: WebSocketUpgrade) -> Response {
    ws.on_upgrade(|mut socket| async move {
        while let Some(Ok(message)) = socket.recv().await {
            if socket.send(message).await.is_err() {
                break;
            }
        }
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
