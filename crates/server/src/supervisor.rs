//! The stream supervisor: one mint-and-swap loop per live camera and
//! quality.
//!
//! Qubo sessions last about five minutes and cannot be refreshed (the OTP
//! route issues no refresh token), so a live stream is re-minted on a
//! cadence and the session is re-read from the store on every mint - a
//! fresh login heals the streams without restarting the server. A camera
//! may hold two streams at once: the hero's high one, and a tile's low one
//! which is dropped once nobody has asked for it for a while.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qubo::{Device, Quality, QuboClient, SessionStore, list_devices, mint_live};
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
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            remint: Duration::from_secs(240),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            discovery_retry: Duration::from_secs(5),
            stream_idle: Duration::from_secs(120),
        }
    }
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
        }
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

        // A cancelled request must not strand the stream as Waking: the
        // registration is undone unless the loop below takes it over.
        let mut registration = Registration {
            supervisor: self.clone(),
            name: name.clone(),
            armed: false,
        };

        let wait = {
            let mut state = self.state.write().await;
            if state.contains_key(&name) {
                return Ok(state
                    .get(&name)
                    .map_or(StreamState::Offline, |tracked| tracked.state.clone()));
            }
            state.insert(name.clone(), new_tracked());
            drop(state);
            registration.armed = true;

            match self.mint_and_swap(&device, quality).await {
                Ok(()) => {
                    self.mark_live(&name).await;
                    self.pacing.remint
                }
                Err(reason) => {
                    self.mark_degraded(&name, reason).await;
                    self.pacing.retry_base
                }
            }
        };

        let supervisor = self.clone();
        tokio::spawn(async move { supervisor.remint_loop(device, quality, wait).await });
        registration.armed = false;
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
            if let Some(idle_for) = idle_since
                && idle_for >= idle
            {
                self.teardown(&name).await;
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
                        (wait * 2).min(self.pacing.retry_max)
                    }
                }
            };
        }
    }

    /// One mint, handed straight to go2rtc. The source swaps under the
    /// stream's own name, so a viewer's stream URL survives re-mints.
    async fn mint_and_swap(&self, device: &Device, quality: Quality) -> Result<(), String> {
        let name = stream_name(&device.device_id, quality);
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
        self.go2rtc
            .put_stream(&name, &live_source(&mint.url))
            .await
            .map_err(|err| redact(&err.to_string()))?;
        // Live means go2rtc actually holds a producer for the stream, not
        // that a PUT was acknowledged. Measured 2026-10-15: the producer
        // appears when the source is set and ffmpeg starts on the first
        // viewer, so a held producer is the strongest liveness the API
        // exposes - frames being on screen stays the tile's own business.
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
        self.needs_login.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Drop a stream nobody asked for: go2rtc's copy and the tracked
    /// entry. No reference shows an API that releases a minted session -
    /// it expires on its own - so dropping the stream is the whole release.
    async fn teardown(&self, name: &str) {
        if let Err(err) = self.go2rtc.delete_stream(name).await {
            tracing::warn!(stream = name, %err, "the idle stream could not be dropped");
        }
        self.state.write().await.remove(name);
        tracing::info!(stream = name, "idle tile stream dropped");
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

/// Undoes a registration whose `ensure` future was dropped before the
/// re-mint loop took over.
struct Registration {
    supervisor: Supervisor,
    name: String,
    armed: bool,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = self.supervisor.state.try_write() {
            state.remove(&self.name);
            return;
        }
        // The map is held elsewhere right now; finish the cleanup from a
        // task rather than leaking the registration.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let supervisor = self.supervisor.clone();
            let name = self.name.clone();
            handle.spawn(async move {
                supervisor.state.write().await.remove(&name);
            });
        }
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
