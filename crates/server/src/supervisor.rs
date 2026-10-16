//! The stream supervisor: one mint-and-swap loop per live camera and
//! quality.
//!
//! Qubo sessions last about five minutes and cannot be refreshed (the OTP
//! route issues no refresh token), so a live stream is re-minted on a
//! cadence and the session is re-read from the store on every mint - a
//! fresh login heals the streams without restarting the server. A camera
//! may hold two streams at once: the hero's high one, and a tile's low one
//! which is dropped once nobody has asked for it for a while.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qubo::{Device, Quality, QuboClient, SessionStore, list_devices, mint_live};
use serde_json::Value;
use tokio::sync::{Notify, RwLock};

use crate::go2rtc::{Go2rtc, live_source};

/// How the supervisor paces itself.
#[derive(Debug, Clone, Copy)]
pub struct Pacing {
    /// How often a live stream is re-minted.
    pub remint: Duration,
    /// The first retry delay after a failure; doubles up to `retry_max`.
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// How often a refused session is re-discovered; the UI is already
    /// routed to login, so this is just how soon recovery is noticed.
    pub discovery_retry: Duration,
    /// How long a stream may go both unasked-for and unwatched before it
    /// is dropped. A stream anyone is watching is never idle.
    pub stream_idle: Duration,
    /// The bound on one fast-lane arm attempt; past it the cloud path
    /// takes over. The engine's stages can sum past this on a bad network,
    /// so the bound cancels rather than waits.
    pub arm_timeout: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            remint: Duration::from_secs(240),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            discovery_retry: Duration::from_secs(5),
            stream_idle: Duration::from_secs(120),
            arm_timeout: Duration::from_secs(15),
        }
    }
}

/// A boxed future: the shape the [`Lane`] seam hands between tasks.
pub type BoxFuture<T> = Pin<Box<dyn Future<Output = T> + Send>>;

/// The fast lane as the supervisor drives it: one seam, so the live engine
/// and the tests' fake both fit.
pub trait Lane: Send + Sync + 'static {
    /// Arm a session for the camera. The supervisor bounds this with
    /// [`Pacing::arm_timeout`] and falls back to the cloud path on failure.
    fn arm(&self, device: Device) -> BoxFuture<Result<Arc<dyn LaneSession>, String>>;
}

/// An armed fast-lane session, as the supervisor uses it.
pub trait LaneSession: Send + Sync + 'static {
    /// The go2rtc source for `name` that reads this session's decrypted
    /// media; every stream name gets its own reader.
    fn source_for(&self, name: &str) -> Result<String, String>;
    /// Send one control envelope over the session's data channel.
    fn control(&self, envelope: Value) -> Result<(), String>;
    /// Whether the session is still up.
    fn alive(&self) -> bool;
    /// End the session (`endP2pCall`); called when the camera's last stream
    /// is torn down.
    fn shutdown(&self);
}

/// The go2rtc stream name for a camera's quality class; the browser asks
/// for exactly this name over the proxy.
pub fn stream_name(device_id: &str, quality: Quality) -> String {
    format!("{device_id}-{}", quality.as_str())
}

/// A camera's stream state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamState {
    Offline,
    Waking,
    Live { since: Instant },
    Degraded { reason: String },
}

/// Everything that can go wrong starting supervision.
#[derive(Debug, thiserror::Error)]
pub enum SupervisorError {
    #[error("unknown device {0}")]
    UnknownDevice(String),
    #[error("VOD playback lands in M3")]
    VodUnavailable,
    #[error("discovery: {0}")]
    Qubo(#[from] qubo::QuboError),
}

/// A stream the supervisor is following: its state, the signal that forces
/// an immediate retry, and when it was last asked for.
struct Tracked {
    state: StreamState,
    wake: Arc<Notify>,
    last_request: Instant,
}

/// The streams the server is keeping alive.
#[derive(Clone)]
pub struct Supervisor {
    qubo: QuboClient,
    sessions: SessionStore,
    go2rtc: Arc<Go2rtc>,
    pacing: Pacing,
    state: Arc<RwLock<HashMap<String, Tracked>>>,
    devices: Arc<RwLock<Vec<Device>>>,
    /// Set when Qubo turns the session away; `/api/cameras` carries it so
    /// the UI can route back to login instead of holding a dead screen.
    needs_login: Arc<AtomicBool>,
    /// When discovery last ran, so a refused session is re-tried on a
    /// cadence rather than on every gated request.
    last_discovery: Arc<Mutex<Option<Instant>>>,
    /// The fast lane, when the deployment enables it.
    lane: Option<Arc<dyn Lane>>,
    /// Cameras that try the fast lane before the cloud mint.
    prefer_p2p: Arc<HashSet<String>>,
    /// The armed fast-lane session per camera - one, never two.
    armed: Arc<RwLock<HashMap<String, Arc<dyn LaneSession>>>>,
    /// Serializes arm-or-reuse decisions per camera, so two streams for one
    /// camera asking at the same moment cannot both arm - and one camera's
    /// arm never queues behind another's.
    arm_locks: Arc<RwLock<HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl Supervisor {
    pub fn new(qubo: QuboClient, sessions: SessionStore, go2rtc: Go2rtc, pacing: Pacing) -> Self {
        Self {
            qubo,
            sessions,
            go2rtc: Arc::new(go2rtc),
            pacing,
            state: Arc::new(RwLock::new(HashMap::new())),
            devices: Arc::new(RwLock::new(Vec::new())),
            needs_login: Arc::new(AtomicBool::new(false)),
            last_discovery: Arc::new(Mutex::new(None)),
            lane: None,
            prefer_p2p: Arc::new(HashSet::new()),
            armed: Arc::new(RwLock::new(HashMap::new())),
            arm_locks: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Enable the fast lane: cameras in `prefer` try it before the cloud
    /// mint, and a failure or an over-bound arm falls back without the
    /// viewer noticing.
    #[must_use]
    pub fn with_fast_lane(mut self, lane: Arc<dyn Lane>, prefer: Vec<String>) -> Self {
        self.lane = Some(lane);
        self.prefer_p2p = Arc::new(prefer.into_iter().collect());
        self
    }

    /// The camera's live fast-lane session, when one is armed; in-session
    /// controls ride it, and the REST path resumes when it ends.
    pub async fn fast_lane_for(&self, device_id: &str) -> Option<Arc<dyn LaneSession>> {
        let session = self.armed.read().await.get(device_id).cloned()?;
        session.alive().then_some(session)
    }

    /// The camera's own arm lock, created on first use.
    async fn arm_lock_for(&self, device_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        Arc::clone(
            self.arm_locks
                .write()
                .await
                .entry(device_id.to_owned())
                .or_default(),
        )
    }

    /// The camera's fast-lane session, arming one if none is live: one
    /// session per camera, and a second ask reuses it rather than stacking.
    async fn lane_session(
        &self,
        lane: &Arc<dyn Lane>,
        device: &Device,
    ) -> Result<Arc<dyn LaneSession>, String> {
        let _serialized = self
            .arm_lock_for(&device.device_id)
            .await
            .lock_owned()
            .await;
        if let Some(session) = self.fast_lane_for(&device.device_id).await {
            return Ok(session);
        }
        let session = tokio::time::timeout(self.pacing.arm_timeout, lane.arm(device.clone()))
            .await
            .map_err(|_| format!("the fast-lane arm exceeded {:?}", self.pacing.arm_timeout))??;
        self.armed
            .write()
            .await
            .insert(device.device_id.clone(), Arc::clone(&session));
        Ok(session)
    }

    /// Whether a refused session's re-discovery is worth trying yet.
    pub fn discovery_due(&self) -> bool {
        let last = *self
            .last_discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        last.is_none_or(|last| last.elapsed() >= self.pacing.discovery_retry)
    }

    /// Whether Qubo has refused the stored session; the UI must log in.
    pub fn needs_login(&self) -> bool {
        self.needs_login.load(Ordering::SeqCst)
    }

    /// Discover the account's devices and remember them.
    pub async fn refresh_devices(&self) -> Result<Vec<Device>, SupervisorError> {
        *self
            .last_discovery
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(Instant::now());
        let session = self.sessions.load()?.ok_or(qubo::QuboError::Auth {
            code: None,
            message: "no Qubo session; log in".to_owned(),
        })?;
        let devices = match list_devices(&self.qubo, &session).await {
            Ok(devices) => devices,
            Err(err) => {
                self.settle_auth(&err);
                return Err(err.into());
            }
        };
        self.needs_login.store(false, Ordering::SeqCst);
        *self.devices.write().await = devices.clone();
        Ok(devices)
    }

    /// The known cameras.
    pub async fn devices(&self) -> Vec<Device> {
        self.devices.read().await.clone()
    }

    /// The camera's stream state across its quality classes: live if
    /// anything of it is, so a camera held only as a tile does not read as
    /// offline.
    pub async fn status(&self, device_id: &str) -> StreamState {
        let state = self.state.read().await;
        let mut degraded = None;
        let mut waking = false;
        for quality in [Quality::High, Quality::Low] {
            match state
                .get(&stream_name(device_id, quality))
                .map(|tracked| &tracked.state)
            {
                Some(StreamState::Live { since }) => return StreamState::Live { since: *since },
                Some(StreamState::Waking) => waking = true,
                Some(StreamState::Degraded { reason }) => {
                    degraded.get_or_insert_with(|| reason.clone());
                }
                _ => {}
            }
        }
        if waking {
            return StreamState::Waking;
        }
        degraded.map_or(StreamState::Offline, |reason| StreamState::Degraded {
            reason,
        })
    }

    /// The state of one quality class alone.
    pub async fn status_for(&self, device_id: &str, quality: Quality) -> StreamState {
        self.state
            .read()
            .await
            .get(&stream_name(device_id, quality))
            .map_or(StreamState::Offline, |tracked| tracked.state.clone())
    }

    /// Remember a refused session so the UI can send the browser to login.
    fn settle_auth(&self, err: &qubo::QuboError) {
        if matches!(err, qubo::QuboError::Auth { .. }) {
            self.needs_login.store(true, Ordering::SeqCst);
        }
    }

    /// Remember that Qubo refused the session. The MQTT feed raises the
    /// same flag when its own reconnect is turned away.
    pub fn mark_needs_login(&self) {
        self.needs_login.store(true, Ordering::SeqCst);
    }

    /// Ensure the camera's stream of `quality` is running: mint, hand the
    /// source to go2rtc, and keep re-minting until the process ends. Later
    /// calls for the same stream reuse the running loop, and count as the
    /// request that keeps an idle tile alive.
    ///
    /// `retry` is the person's intent - a Try again - and only it skips the
    /// loop's backoff; a page that keeps asking must not hammer a camera
    /// that is down.
    pub async fn ensure(
        &self,
        device_id: &str,
        quality: Quality,
        retry: bool,
    ) -> Result<StreamState, SupervisorError> {
        let name = stream_name(device_id, quality);
        {
            let mut state = self.state.write().await;
            if let Some(tracked) = state.get_mut(&name) {
                tracked.last_request = Instant::now();
                if retry && matches!(tracked.state, StreamState::Degraded { .. }) {
                    tracked.wake.notify_one();
                }
                return Ok(tracked.state.clone());
            }
        }
        let device = {
            let devices = self.devices.read().await;
            devices
                .iter()
                .find(|device| device.device_id == device_id)
                .cloned()
        }
        .ok_or_else(|| SupervisorError::UnknownDevice(device_id.to_owned()))?;

        {
            let mut state = self.state.write().await;
            if state.contains_key(&name) {
                return Ok(state
                    .get(&name)
                    .map_or(StreamState::Offline, |tracked| tracked.state.clone()));
            }
            // Registered as Waking, and the loop owns it from here: the
            // first mint happens inside the loop, so a request that is
            // cancelled cannot strand the stream (the loop finishes its
            // mint and keeps the source), and a camera never reads offline
            // while its first mint is in flight.
            state.insert(name.clone(), new_waking());
        }
        let supervisor = self.clone();
        tokio::spawn(async move {
            supervisor
                .remint_loop(device, quality, Duration::ZERO)
                .await;
        });
        Ok(self.status_for(device_id, quality).await)
    }

    /// Reserved for M3: cloud-allocated VOD sessions for recordings. M1
    /// streams the live pipeline only, so this refuses rather than guesses.
    pub fn ensure_vod(&self, _device_id: &str, _timestamp: i64) -> Result<(), SupervisorError> {
        Err(SupervisorError::VodUnavailable)
    }

    async fn remint_loop(self, device: Device, quality: Quality, mut wait: Duration) {
        let name = stream_name(&device.device_id, quality);
        let wake = self.wake_signal(&name).await;
        // Every stream idles out when nobody is watching and nobody has
        // asked: a demoted hero's high stream must not run for ever. The
        // consumer check keeps a watched one - a hero - alive regardless.
        let idle = self.pacing.stream_idle;
        loop {
            // A stream with a viewer is not idle, whatever this server was
            // last asked: the viewer holds it open through go2rtc.
            let idle_since = if self.watched(&name).await {
                None
            } else {
                self.idle_for(&name).await
            };
            let seen_at = self.last_request_at(&name).await;
            let sleep_for = match idle_since {
                Some(idle_for) => wait.min(idle.saturating_sub(idle_for)),
                // Watched: wake again within a window, so a viewer who
                // leaves is noticed without waiting out a re-mint.
                None => wait.min(idle),
            };
            tokio::select! {
                () = tokio::time::sleep(sleep_for) => {}
                () = wake.notified() => {
                    tracing::info!(stream = %name, "remint forced by a retry");
                }
            }
            if let (Some(idle_for), Some(seen_at)) = (idle_since, seen_at)
                && idle_for >= idle
                && self.teardown_if_idle(&name, seen_at).await
            {
                return;
            }
            wait = match self.mint_and_swap(&device, quality).await {
                Ok(()) => {
                    self.mark_live(&name).await;
                    self.pacing.remint
                }
                Err(reason) => {
                    tracing::warn!(stream = %name, %reason, "stream re-mint failed");
                    self.mark_degraded(&name, reason).await;
                    if wait >= self.pacing.remint {
                        self.pacing.retry_base
                    } else {
                        // The first attempt waits no time at all, so the
                        // floor matters: doubling zero is still zero.
                        (wait * 2)
                            .max(self.pacing.retry_base)
                            .min(self.pacing.retry_max)
                    }
                }
            };
        }
    }

    /// One mint, handed straight to go2rtc. The source swaps under the
    /// stream's own name, so a viewer's stream URL survives re-mints. A
    /// camera set to prefer the fast lane tries it first; any fast-lane
    /// failure (arm refused, over its bound, or the session ended) falls
    /// back to the cloud mint within the loop's own retry cadence.
    async fn mint_and_swap(&self, device: &Device, quality: Quality) -> Result<(), String> {
        let name = stream_name(&device.device_id, quality);
        if let Some(lane) = &self.lane
            && self.prefer_p2p.contains(&device.device_id)
        {
            match self.lane_session(lane, device).await {
                Ok(session) => match session.source_for(&name) {
                    Ok(source) => match self.put_and_confirm(&name, &source).await {
                        Ok(()) => {
                            tracing::info!(stream = %name, "fast lane armed; feeding go2rtc its decrypted RTP");
                            return Ok(());
                        }
                        Err(reason) => {
                            // go2rtc refused the source: this session cannot
                            // serve the pass, so end it and take the cloud
                            // path now rather than retrying it blindly.
                            tracing::info!(
                                stream = %name,
                                %reason,
                                "go2rtc refused the fast-lane source; the cloud path takes this pass"
                            );
                            session.shutdown();
                            self.armed.write().await.remove(&device.device_id);
                        }
                    },
                    Err(reason) => {
                        tracing::info!(stream = %name, %reason, "the fast-lane session ended; using the cloud path");
                        self.armed.write().await.remove(&device.device_id);
                    }
                },
                Err(reason) => {
                    tracing::info!(stream = %name, %reason, "fast lane unavailable; using the cloud path");
                }
            }
        }
        let session = self
            .sessions
            .load()
            .map_err(|err| err.to_string())?
            .ok_or_else(|| "no Qubo session; log in".to_owned())?;
        let mint = match mint_live(&self.qubo, &session, device, quality).await {
            Ok(mint) => mint,
            Err(err) => {
                self.settle_auth(&err);
                return Err(redact(&err.to_string()));
            }
        };
        self.put_and_confirm(&name, &live_source(&mint.url)).await?;
        self.needs_login.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Hand the source to go2rtc and confirm it took. Live means go2rtc
    /// actually holds a producer for the stream, not that a PUT was
    /// acknowledged. Measured 2026-10-15: the producer appears when the
    /// source is set and ffmpeg starts on the first viewer, so a held
    /// producer is the strongest liveness the API exposes - frames being on
    /// screen stays the tile's own business.
    async fn put_and_confirm(&self, name: &str, source: &str) -> Result<(), String> {
        self.go2rtc
            .put_stream(name, source)
            .await
            .map_err(|err| redact(&err.to_string()))?;
        let held = self
            .go2rtc
            .streams()
            .await
            .map_err(|err| redact(&err.to_string()))?
            .iter()
            .any(|stream| stream.name == name && stream.live);
        if !held {
            return Err("go2rtc did not take the stream".to_owned());
        }
        Ok(())
    }

    /// Drop the stream only if the entry this loop decided about is still
    /// the entry that is there: a request that landed while the loop was
    /// deciding belongs to somebody who is watching, and must not have its
    /// stream deleted under it. The last request is the entry's identity -
    /// an elapsed duration just keeps growing and can never be compared.
    async fn teardown_if_idle(&self, name: &str, seen_at: Instant) -> bool {
        if self.last_request_at(name).await != Some(seen_at) {
            tracing::debug!(stream = name, "a request landed; the stream is not idle");
            return false;
        }
        self.teardown(name).await;
        true
    }

    /// When the stream was last asked for, if it is still known.
    async fn last_request_at(&self, name: &str) -> Option<Instant> {
        self.state
            .read()
            .await
            .get(name)
            .map(|tracked| tracked.last_request)
    }

    /// Drop a stream nobody asked for: go2rtc's copy and the tracked
    /// entry. No reference shows an API that releases a minted session -
    /// it expires on its own - so dropping the stream is the whole release.
    /// The camera's fast-lane session ends with its last stream.
    async fn teardown(&self, name: &str) {
        if let Err(err) = self.go2rtc.delete_stream(name).await {
            tracing::warn!(stream = name, %err, "the idle stream could not be dropped");
        }
        self.state.write().await.remove(name);
        tracing::info!(stream = name, "idle tile stream dropped");
        let Some((device_id, _)) = name.rsplit_once('-') else {
            return;
        };
        let prefix = format!("{device_id}-");
        let remaining = self
            .state
            .read()
            .await
            .keys()
            .any(|key| key.starts_with(&prefix));
        if !remaining && let Some(session) = self.armed.write().await.remove(device_id) {
            session.shutdown();
            tracing::info!(
                camera = device_id,
                "fast-lane session ended with its last stream"
            );
        }
    }

    /// Whether go2rtc has a viewer on the stream. An API that cannot say
    /// keeps the stream: a late teardown beats a wrong one.
    async fn watched(&self, name: &str) -> bool {
        self.go2rtc.streams().await.map_or(true, |streams| {
            streams
                .iter()
                .any(|stream| stream.name == name && stream.consumers > 0)
        })
    }

    /// How long since the stream was last asked for, if it is still known.
    async fn idle_for(&self, name: &str) -> Option<Duration> {
        self.state
            .read()
            .await
            .get(name)
            .map(|tracked| tracked.last_request.elapsed())
    }

    /// The tracked entry's wake handle, created if the entry is missing.
    async fn wake_signal(&self, name: &str) -> Arc<Notify> {
        self.state
            .write()
            .await
            .entry(name.to_owned())
            .or_insert_with(new_tracked)
            .wake
            .clone()
    }

    /// `Live` keeps its original `since` through successful re-mints.
    async fn mark_live(&self, name: &str) {
        let mut state = self.state.write().await;
        let entry = state.entry(name.to_owned()).or_insert_with(new_tracked);
        if !matches!(entry.state, StreamState::Live { .. }) {
            entry.state = StreamState::Live {
                since: Instant::now(),
            };
        }
    }

    async fn mark_degraded(&self, name: &str, reason: String) {
        self.state
            .write()
            .await
            .entry(name.to_owned())
            .or_insert_with(new_tracked)
            .state = StreamState::Degraded { reason };
    }
}

fn new_tracked() -> Tracked {
    Tracked {
        state: StreamState::Offline,
        wake: Arc::new(Notify::new()),
        last_request: Instant::now(),
    }
}

/// A stream that has been asked for and whose loop has not yet taken a
/// position: the state a person sees between the tap and the first frame.
fn new_waking() -> Tracked {
    Tracked {
        state: StreamState::Waking,
        ..new_tracked()
    }
}

/// Minted URLs carry a session token and errors echo the URL they failed
/// on; nothing with a token attached reaches logs or the browser's reason.
fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("token=") {
        out.push_str(&rest[..start]);
        out.push_str("token=<redacted>");
        let after = &rest[start + "token=".len()..];
        // `#` matters: the source rides as `?token=..#input=nazar..`, and
        // without it the whole tail of the message is swallowed.
        let end = after
            .find(['&', ' ', '"', '\'', ')', ']', '\\', '#'])
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    //! The idle teardown's decision window, which no integration test can
    //! land in: the loop's look and its teardown are microseconds apart.
    //! Here the window is opened by hand, between the two calls.

    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// A supervisor that answers nothing: the test path makes no call it
    /// would have to fake.
    fn quiet_supervisor() -> Supervisor {
        Supervisor::new(
            QuboClient::new(
                "http://127.0.0.1:9".parse().expect("a url"),
                qubo::ServiceProvider::default(),
            ),
            SessionStore::new(std::env::temp_dir().join("nazar-guard-test.json")),
            Go2rtc::connect("http://127.0.0.1:9".parse().expect("a url")),
            Pacing::default(),
        )
    }

    #[tokio::test]
    async fn a_teardown_stands_down_for_a_request_that_landed_meanwhile() {
        let supervisor = quiet_supervisor();
        let name = stream_name("cam", Quality::Low);
        let seen = Instant::now();
        supervisor.state.write().await.insert(
            name.clone(),
            Tracked {
                state: StreamState::Live { since: seen },
                wake: Arc::new(Notify::new()),
                last_request: seen,
            },
        );

        // The person asks again while the loop is deciding.
        supervisor
            .state
            .write()
            .await
            .get_mut(&name)
            .expect("the entry")
            .last_request = seen + Duration::from_millis(5);

        assert!(
            !supervisor.teardown_if_idle(&name, seen).await,
            "a request that landed in the window stands the teardown down"
        );
        assert!(
            supervisor.state.read().await.contains_key(&name),
            "and the stream survives it"
        );

        // Nothing new since: the same decision drops the stream.
        let settled = supervisor.last_request_at(&name).await.expect("the entry");
        assert!(
            supervisor.teardown_if_idle(&name, settled).await,
            "a stream with no new request is dropped"
        );
        assert!(
            !supervisor.state.read().await.contains_key(&name),
            "and forgotten"
        );
    }
}
