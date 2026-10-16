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

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    armed_devices: Arc<Mutex<Vec<String>>>,
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
                released: Mutex::new(Vec::new()),
            }),
            arms: Arc::new(AtomicUsize::new(0)),
            armed_devices: Arc::new(Mutex::new(Vec::new())),
            outcome: Arc::new(Mutex::new(outcome)),
        }
    }

    /// The devices whose arms were invoked, in order.
    pub fn armed_devices(&self) -> Vec<String> {
        self.armed_devices.lock().unwrap().clone()
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
    pub released: Mutex<Vec<String>>,
}

impl FakeSession {
    pub fn controls(&self) -> Vec<Value> {
        self.controls.lock().unwrap().clone()
    }

    /// The stream names released back to the session, in order.
    pub fn released(&self) -> Vec<String> {
        self.released.lock().unwrap().clone()
    }
}

impl Lane for FakeLane {
    fn arm(
        &self,
        device: Device,
    ) -> Pin<Box<dyn Future<Output = Result<Arc<dyn LaneSession>, String>> + Send>> {
        self.arms.fetch_add(1, Ordering::SeqCst);
        self.armed_devices.lock().unwrap().push(device.device_id);
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

    fn release(&self, name: &str) {
        self.released.lock().unwrap().push(name.to_owned());
    }

    fn shutdown(&self) {
        self.alive.store(false, Ordering::SeqCst);
    }
}

/// A fake go2rtc API: records the stream PUTs and DELETEs it receives and
/// answers GETs with the shape measured from go2rtc 1.9.14.
#[derive(Clone)]
pub struct FakeApi {
    pub base: reqwest::Url,
    puts: Arc<Mutex<Vec<(String, String)>>>,
    /// The puts go2rtc actually took: what the streams view lists, as a
    /// refused put leaves no entry behind in the real one.
    taken: Arc<Mutex<Vec<(String, String)>>>,
    deletes: Arc<Mutex<Vec<String>>>,
    put_status: Arc<AtomicU16>,
    put_refused_marker: Arc<Mutex<Option<String>>>,
    streams_visible: Arc<AtomicBool>,
    streams_error: Arc<AtomicBool>,
    producers: Arc<AtomicBool>,
    /// Streams whose producer is held but not connected: the entry carries
    /// a source and reads no `remote_addr`, as a go2rtc whose ffmpeg cannot
    /// reach the camera. Empty means every held producer is connected.
    unconnected: Arc<Mutex<HashSet<String>>>,
    /// The attached viewers, by stream: count and when the first one
    /// attached. A producer is up only while a consumer is, so this is
    /// both the consumers count and the connect clock.
    viewers: Arc<Mutex<HashMap<String, (usize, Instant)>>>,
    /// The minted-URL bookkeeping: (the put generation, the generation a
    /// consumer has dialled). The wire's URL is one-shot, and the fake
    /// models it: an attach in an already-dialled generation never
    /// connects until a fresh put.
    url_state: Arc<Mutex<HashMap<String, (u64, u64)>>>,
    /// The streams whose current attach burst dialled the fresh URL.
    dialed: Arc<Mutex<HashSet<String>>>,
    /// Per-stream delay between a consumer attaching and the producer
    /// connecting, for tests that need a slow source.
    connect_after: Arc<Mutex<HashMap<String, Duration>>>,
    /// The streams narar itself holds a consumer on, by hold start: the
    /// producer stays up exactly while some consumer - a viewer or a hold
    /// - is attached.
    held: Arc<Mutex<HashMap<String, Instant>>>,
    /// How long a DELETE takes to answer, in milliseconds: a teardown that
    /// can be caught mid-flight.
    delete_delay: Arc<AtomicU64>,
    /// Every streams-admin operation in arrival order, `put:<name>` and
    /// `delete:<name>`: the order tests otherwise cannot see.
    ops: Arc<Mutex<Vec<String>>>,
}

impl FakeApi {
    pub async fn start() -> Self {
        let puts = Arc::new(Mutex::new(Vec::new()));
        let taken = Arc::new(Mutex::new(Vec::new()));
        let deletes = Arc::new(Mutex::new(Vec::new()));
        let put_status = Arc::new(AtomicU16::new(200));
        let put_refused_marker = Arc::new(Mutex::new(None));
        let streams_visible = Arc::new(AtomicBool::new(true));
        let streams_error = Arc::new(AtomicBool::new(false));
        let producers = Arc::new(AtomicBool::new(true));
        let unconnected = Arc::new(Mutex::new(HashSet::new()));
        let connect_after = Arc::new(Mutex::new(HashMap::new()));
        let held = Arc::new(Mutex::new(HashMap::new()));
        let viewers = Arc::new(Mutex::new(HashMap::new()));
        let url_state = Arc::new(Mutex::new(HashMap::new()));
        let dialed = Arc::new(Mutex::new(HashSet::new()));
        let delete_delay = Arc::new(AtomicU64::new(0));
        let ops = Arc::new(Mutex::new(Vec::new()));
        let app = hls_routes().merge(streams_routes(
            &puts,
            &deletes,
            &Knobs {
                put_status: &put_status,
                put_refused_marker: &put_refused_marker,
                streams_visible: &streams_visible,
                streams_error: &streams_error,
                producers: &producers,
                unconnected: &unconnected,
                url_state: &url_state,
                dialed: &dialed,
                connect_after: &connect_after,
                delete_delay: &delete_delay,
                ops: &ops,
                taken: &taken,
                held: &held,
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
            taken,
            deletes,
            put_status,
            put_refused_marker,
            streams_visible,
            streams_error,
            producers,
            unconnected,
            viewers,
            url_state,
            dialed,
            connect_after,
            held,
            delete_delay,
            ops,
        }
    }

    /// The streams-admin operations received so far, in arrival order, as
    /// `put:<name>` and `delete:<name>`.
    pub fn ops(&self) -> Vec<String> {
        self.ops.lock().unwrap().clone()
    }

    /// Report every stream without a producer, as a go2rtc whose sources
    /// are configured but not running would.
    pub fn set_producers(&self, running: bool) {
        self.producers.store(running, Ordering::SeqCst);
    }

    /// The stream's producer holds its entry but never connects: what a
    /// go2rtc whose ffmpeg the camera keeps turning away reads like.
    pub fn disconnect_producer(&self, name: &str) {
        self.unconnected.lock().unwrap().insert(name.to_owned());
    }

    /// The stream's producer connects, as one whose source finally
    /// answered does.
    pub fn connect_producer(&self, name: &str) {
        self.unconnected.lock().unwrap().remove(name);
    }

    /// The stream's producer connects `delay` after its first consumer
    /// attaches, as a slow camera's push does.
    pub fn connect_after(&self, name: &str, delay: Duration) {
        self.connect_after
            .lock()
            .unwrap()
            .insert(name.to_owned(), delay);
    }

    /// Make every DELETE take `delay` to answer, as a go2rtc under load
    /// would: a teardown can then be caught mid-flight.
    pub fn set_delete_delay(&self, delay: Duration) {
        self.delete_delay.store(
            u64::try_from(delay.as_millis()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
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
    /// holds it open. The first attach is the clock the producer's connect
    /// delay runs from; zero viewers clears it, as a departed consumer's
    /// producer would stop. An attach after the URL's one dial, with no
    /// fresh mint since, is what the wire answers with silence.
    pub fn set_viewers(&self, name: &str, count: usize) {
        let mut viewers = self.viewers.lock().unwrap();
        if count == 0 {
            viewers.remove(name);
            // A hold on the stream keeps the producer's dial spent: the
            // consumer never actually left.
            if !self.held.lock().unwrap().contains_key(name) {
                self.dialed.lock().unwrap().remove(name);
            }
            return;
        }
        if let Some((seen, _)) = viewers.get_mut(name) {
            *seen = count;
            return;
        }
        let mut urls = self.url_state.lock().unwrap();
        let state = urls.entry(name.to_owned()).or_insert((0, 0));
        if state.1 < state.0 {
            state.1 = state.0;
            self.dialed.lock().unwrap().insert(name.to_owned());
        }
        viewers.insert(name.to_owned(), (count, Instant::now()));
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

    /// narar's own consumer on the stream, or its release: while held the
    /// producer stays up exactly as it would for a viewer.
    pub fn hold(&self, name: &str, on: bool) {
        if on {
            self.held
                .lock()
                .unwrap()
                .entry(name.to_owned())
                .or_insert_with(Instant::now);
            self.dialed.lock().unwrap().insert(name.to_owned());
            return;
        }
        self.held.lock().unwrap().remove(name);
        if !self.viewers.lock().unwrap().contains_key(name) {
            self.dialed.lock().unwrap().remove(name);
        }
    }

    /// The stream's consumer count and producer state, as the listing
    /// would report them.
    pub fn stream_view(&self, name: &str) -> (usize, bool) {
        let viewers = self.viewers.lock().unwrap().get(name).copied();
        let held = self.held.lock().unwrap().get(name).copied();
        let watching = viewers.map_or(0, |(count, _)| count) + usize::from(held.is_some());
        let since = viewers.map(|(_, at)| at).or(held);
        let delay = self
            .connect_after
            .lock()
            .unwrap()
            .get(name)
            .copied()
            .unwrap_or_default();
        let connected = self.dialed.lock().unwrap().contains(name)
            && since.is_some_and(|at| at.elapsed() >= delay)
            && !self.unconnected.lock().unwrap().contains(name);
        (watching, connected)
    }
}

/// A stand-in for the production hold: narar's consumer registered on the
/// fake, so the producer stays warm while it is held.
pub struct FakeHold {
    pub api: FakeApi,
    attach: bool,
}

impl FakeHold {
    /// A hold whose consumer attaches, as the ffmpeg does.
    pub fn attaching(api: FakeApi) -> Self {
        Self { api, attach: true }
    }

    /// A hold that never attaches - a wrong binary, a dead listener: the
    /// registration exists, no consumer does.
    pub fn never_attaching(api: FakeApi) -> Self {
        Self { api, attach: false }
    }
}

impl server::supervisor::StreamHold for FakeHold {
    fn hold(&self, name: &str) -> std::sync::Arc<dyn server::supervisor::HoldLease> {
        if self.attach {
            self.api.hold(name, true);
        }
        std::sync::Arc::new(FakeLease {
            api: self.api.clone(),
            name: name.to_owned(),
            attached: self.attach,
        })
    }
}

struct FakeLease {
    api: FakeApi,
    name: String,
    attached: bool,
}

impl server::supervisor::HoldLease for FakeLease {
    fn attached(&self) -> bool {
        self.attached
    }
}

impl Drop for FakeLease {
    fn drop(&mut self) {
        if self.attached {
            self.api.hold(&self.name, false);
        }
    }
}

/// The answer-mode knobs a test turns, bundled so the fake's surfaces take
/// them as one argument.
struct Knobs<'a> {
    put_status: &'a Arc<AtomicU16>,
    put_refused_marker: &'a Arc<Mutex<Option<String>>>,
    streams_visible: &'a Arc<AtomicBool>,
    streams_error: &'a Arc<AtomicBool>,
    producers: &'a Arc<AtomicBool>,
    unconnected: &'a Arc<Mutex<HashSet<String>>>,
    url_state: &'a Arc<Mutex<HashMap<String, (u64, u64)>>>,
    dialed: &'a Arc<Mutex<HashSet<String>>>,
    connect_after: &'a Arc<Mutex<HashMap<String, Duration>>>,
    delete_delay: &'a Arc<AtomicU64>,
    ops: &'a Arc<Mutex<Vec<String>>>,
    taken: &'a Arc<Mutex<Vec<(String, String)>>>,
    held: &'a Arc<Mutex<HashMap<String, Instant>>>,
}

/// The streams admin surface, as go2rtc 1.9.14 answers it.
fn streams_routes(
    puts: &Arc<Mutex<Vec<(String, String)>>>,
    deletes: &Arc<Mutex<Vec<String>>>,
    knobs: &Knobs<'_>,
    viewers: &Arc<Mutex<HashMap<String, (usize, Instant)>>>,
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
    viewers: &Arc<Mutex<HashMap<String, (usize, Instant)>>>,
) -> axum::routing::MethodRouter {
    let recorded = Arc::clone(puts);
    let status = Arc::clone(knobs.put_status);
    let refused = Arc::clone(knobs.put_refused_marker);
    let ops = Arc::clone(knobs.ops);
    let urls = Arc::clone(knobs.url_state);
    let dialed = Arc::clone(knobs.dialed);
    let served = Arc::clone(viewers);
    let taken = Arc::clone(knobs.taken);
    let held = Arc::clone(knobs.held);
    let put_stream = put(move |Query(params): Query<HashMap<String, String>>| {
        let recorded = Arc::clone(&recorded);
        let status = Arc::clone(&status);
        let refused = Arc::clone(&refused);
        let ops = Arc::clone(&ops);
        let urls = Arc::clone(&urls);
        let dialed = Arc::clone(&dialed);
        let served = Arc::clone(&served);
        let taken = Arc::clone(&taken);
        let held = Arc::clone(&held);
        async move {
            let name = params.get("name").cloned().unwrap_or_default();
            let source = params.get("src").cloned().unwrap_or_default();
            recorded.lock().unwrap().push((name.clone(), source.clone()));
            // A fresh source is a fresh URL. Attached consumers - viewers
            // or narar's own hold - ride the swap, as go2rtc swaps the
            // source under the stream's name: their producer restarts and
            // dials the new URL, spending its one dial for this
            // generation. With nobody attached the URL stays unspent, and
            // the next new attach is the one that dials it.
            let riding = served.lock().unwrap().contains_key(&name)
                || held.lock().unwrap().contains_key(&name);
            {
                let mut urls = urls.lock().unwrap();
                let state = urls.entry(name.clone()).or_insert((0, 0));
                state.0 += 1;
                if riding {
                    state.1 = state.0;
                }
            }
            if riding {
                dialed.lock().unwrap().insert(name.clone());
            } else {
                dialed.lock().unwrap().remove(&name);
            }
            ops.lock().unwrap().push(format!("put:{name}"));
            let marker = refused
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let code = if marker.is_some_and(|marker| source.contains(&marker)) {
                StatusCode::INTERNAL_SERVER_ERROR
            } else {
                StatusCode::from_u16(status.load(Ordering::SeqCst)).expect("a status")
            };
            if code.is_success() {
                taken.lock().unwrap().push((name.clone(), source.clone()));
            }
            // A rejected PUT quotes the source back, as go2rtc's own
            // refusals do - token and all.
            let body = if code.is_success() {
                String::new()
            } else {
                format!("streams: source from insecure producer: {source}")
            };
            (code, body)
        }
    });
    let dropped = Arc::clone(deletes);
    let delay = Arc::clone(knobs.delete_delay);
    let ops = Arc::clone(knobs.ops);
    let drop_stream =
        axum::routing::delete(move |Query(params): Query<HashMap<String, String>>| {
            let dropped = Arc::clone(&dropped);
            let delay = Arc::clone(&delay);
            let ops = Arc::clone(&ops);
            async move {
                // Recorded on arrival, not on answer: a slow DELETE is
                // still an operation that happened when it was sent, and
                // the op order is what some tests read.
                let src = params.get("src").cloned().unwrap_or_default();
                dropped.lock().unwrap().push(src.clone());
                ops.lock().unwrap().push(format!("delete:{src}"));
                let millis = delay.load(Ordering::SeqCst);
                if millis > 0 {
                    tokio::time::sleep(Duration::from_millis(millis)).await;
                }
            }
        });
    let list_streams = list_streams_handler(StreamsAnswer {
        recorded: Arc::clone(knobs.taken),
        visible: Arc::clone(knobs.streams_visible),
        viewers: Arc::clone(viewers),
        error: Arc::clone(knobs.streams_error),
        running: Arc::clone(knobs.producers),
        unconnected: Arc::clone(knobs.unconnected),
        dialed: Arc::clone(knobs.dialed),
        connect_after: Arc::clone(knobs.connect_after),
        held: Arc::clone(knobs.held),
    });
    put_stream.delete(drop_stream).get(list_streams)
}

/// Everything the `GET /api/streams` answer reads; bundled, because eight
/// handles as eight arguments is a clippy verdict.
#[derive(Clone)]
struct StreamsAnswer {
    recorded: Arc<Mutex<Vec<(String, String)>>>,
    visible: Arc<AtomicBool>,
    viewers: Arc<Mutex<HashMap<String, (usize, Instant)>>>,
    error: Arc<AtomicBool>,
    running: Arc<AtomicBool>,
    unconnected: Arc<Mutex<HashSet<String>>>,
    dialed: Arc<Mutex<HashSet<String>>>,
    connect_after: Arc<Mutex<HashMap<String, Duration>>>,
    held: Arc<Mutex<HashMap<String, Instant>>>,
}

/// The `GET /api/streams` answer, as go2rtc 1.9.14 shapes it.
fn list_streams_handler(state: StreamsAnswer) -> axum::routing::MethodRouter {
    get(move || {
        let StreamsAnswer {
            recorded,
            visible,
            viewers,
            error,
            running,
            unconnected,
            dialed,
            connect_after,
            held,
        } = state.clone();
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
            let held = held.lock().unwrap().clone();
            let connect_after = connect_after.lock().unwrap().clone();
            // go2rtc materializes a producer as soon as the source is set.
            let mut streams = serde_json::Map::new();
            for (name, source) in recorded {
                let (mut watching, mut since) = viewers
                    .get(&name)
                    .map_or((0, None), |(count, at)| (*count, Some(*at)));
                // narar's own hold is a consumer to go2rtc: it counts, and
                // its attach dials the producer like a viewer's would.
                if let Some(at) = held.get(&name) {
                    watching += 1;
                    since = Some(since.map_or(*at, |viewer_at| viewer_at.min(*at)));
                }
                // Measured shape: no consumers is `null`.
                let consumers = (watching > 0).then(|| {
                    (0..watching)
                        .map(|_| serde_json::json!({ "id": "viewer" }))
                        .collect::<Vec<_>>()
                });
                let running_now = running.load(Ordering::SeqCst);
                // Connects once a consumer is attached AND the source
                // answers AND the URL still has its one dial; the deny-list
                // is the camera turning the ffmpeg away, and
                // `connect_after` is a slow push.
                let delay = connect_after.get(&name).copied().unwrap_or_default();
                let connected = dialed.lock().unwrap().contains(&name)
                    && since.is_some_and(|at| at.elapsed() >= delay)
                    && !unconnected.lock().unwrap().contains(&name);
                streams.insert(
                    name,
                    serde_json::json!({
                        "producers": if running_now {
                            // Connected carries a peer address; waiting does not.
                            let mut producer = serde_json::json!({ "url": source });
                            if connected {
                                producer["remote_addr"] = serde_json::json!("127.0.0.1:0");
                            }
                            vec![producer]
                        } else {
                            Vec::new()
                        },
                        "consumers": consumers,
                    }),
                );
            }
            serde_json::Value::Object(streams).to_string()
        }
    })
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
