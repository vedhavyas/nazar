//! The stream supervisor: one mint-and-swap loop per live camera.
//!
//! Qubo sessions last about five minutes and cannot be refreshed (the OTP
//! route issues no refresh token), so a live stream is re-minted on a
//! cadence and the session is re-read from the store on every mint - a
//! fresh login heals the streams without restarting the server.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
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
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            remint: Duration::from_secs(240),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
        }
    }
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

/// A camera the supervisor is following: its state and the signal that
/// forces an immediate retry.
struct Tracked {
    state: StreamState,
    wake: Arc<Notify>,
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
        }
    }

    /// Whether Qubo has refused the stored session; the UI must log in.
    pub fn needs_login(&self) -> bool {
        self.needs_login.load(Ordering::SeqCst)
    }

    /// Discover the account's devices and remember them.
    pub async fn refresh_devices(&self) -> Result<Vec<Device>, SupervisorError> {
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

    /// The camera's stream state.
    pub async fn status(&self, device_id: &str) -> StreamState {
        self.state
            .read()
            .await
            .get(device_id)
            .map_or(StreamState::Offline, |tracked| tracked.state.clone())
    }

    /// Remember a refused session so the UI can send the browser to login.
    fn settle_auth(&self, err: &qubo::QuboError) {
        if matches!(err, qubo::QuboError::Auth { .. }) {
            self.needs_login.store(true, Ordering::SeqCst);
        }
    }

    /// Ensure the camera's stream is running: mint, hand the source to
    /// go2rtc, and keep re-minting until the process ends. Later calls for
    /// the same camera reuse the running loop; a failed first mint leaves
    /// the stream retrying and the state `Degraded`.
    pub async fn ensure_live(&self, device_id: &str) -> Result<(), SupervisorError> {
        {
            let state = self.state.read().await;
            if let Some(tracked) = state.get(device_id) {
                // A manual retry forces the loop's next attempt now instead
                // of leaving the person waiting out the backoff.
                if matches!(tracked.state, StreamState::Degraded { .. }) {
                    tracked.wake.notify_one();
                }
                return Ok(());
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

        // A cancelled request must not strand the device as Waking: the
        // registration is undone unless the loop below takes it over.
        let mut registration = Registration {
            supervisor: self.clone(),
            device_id: device_id.to_owned(),
            armed: false,
        };

        let wait = {
            let mut state = self.state.write().await;
            if state.contains_key(device_id) {
                return Ok(());
            }
            state.insert(
                device_id.to_owned(),
                Tracked {
                    state: StreamState::Waking,
                    wake: Arc::new(Notify::new()),
                },
            );
            drop(state);
            registration.armed = true;

            match self.mint_and_swap(&device).await {
                Ok(()) => {
                    self.mark_live(&device.device_id).await;
                    self.pacing.remint
                }
                Err(reason) => {
                    self.mark_degraded(&device.device_id, reason).await;
                    self.pacing.retry_base
                }
            }
        };

        let supervisor = self.clone();
        tokio::spawn(async move { supervisor.remint_loop(device, wait).await });
        registration.armed = false;
        Ok(())
    }

    /// Reserved for M3: cloud-allocated VOD sessions for recordings. M1
    /// streams the live pipeline only, so this refuses rather than guesses.
    pub fn ensure_vod(&self, _device_id: &str, _timestamp: i64) -> Result<(), SupervisorError> {
        Err(SupervisorError::VodUnavailable)
    }

    async fn remint_loop(self, device: Device, mut wait: Duration) {
        let wake = self.wake_signal(&device.device_id).await;
        loop {
            tokio::select! {
                () = tokio::time::sleep(wait) => {}
                () = wake.notified() => {
                    tracing::info!(device = %device.device_id, "remint forced by a retry");
                }
            }
            wait = match self.mint_and_swap(&device).await {
                Ok(()) => {
                    self.mark_live(&device.device_id).await;
                    self.pacing.remint
                }
                Err(reason) => {
                    tracing::warn!(device = %device.device_id, %reason, "stream re-mint failed");
                    self.mark_degraded(&device.device_id, reason).await;
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
    async fn mint_and_swap(&self, device: &Device) -> Result<(), String> {
        let session = self
            .sessions
            .load()
            .map_err(|err| err.to_string())?
            .ok_or_else(|| "no Qubo session; log in".to_owned())?;
        let mint = match mint_live(&self.qubo, &session, device, Quality::High).await {
            Ok(mint) => mint,
            Err(err) => {
                self.settle_auth(&err);
                return Err(redact(&err.to_string()));
            }
        };
        self.go2rtc
            .put_stream(&device.device_id, &live_source(&mint.url))
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
            .any(|stream| stream.name == device.device_id && stream.live);
        if !held {
            return Err("go2rtc did not take the stream".to_owned());
        }
        self.needs_login.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// The tracked entry's wake handle, created if the entry is missing.
    async fn wake_signal(&self, device_id: &str) -> Arc<Notify> {
        self.state
            .write()
            .await
            .entry(device_id.to_owned())
            .or_insert_with(new_tracked)
            .wake
            .clone()
    }

    /// `Live` keeps its original `since` through successful re-mints.
    async fn mark_live(&self, device_id: &str) {
        let mut state = self.state.write().await;
        let entry = state
            .entry(device_id.to_owned())
            .or_insert_with(new_tracked);
        if !matches!(entry.state, StreamState::Live { .. }) {
            entry.state = StreamState::Live {
                since: Instant::now(),
            };
        }
    }

    async fn mark_degraded(&self, device_id: &str, reason: String) {
        self.state
            .write()
            .await
            .entry(device_id.to_owned())
            .or_insert_with(new_tracked)
            .state = StreamState::Degraded { reason };
    }
}

fn new_tracked() -> Tracked {
    Tracked {
        state: StreamState::Offline,
        wake: Arc::new(Notify::new()),
    }
}

/// Undoes a registration whose `ensure_live` future was dropped before the
/// re-mint loop took over.
struct Registration {
    supervisor: Supervisor,
    device_id: String,
    armed: bool,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut state) = self.supervisor.state.try_write() {
            state.remove(&self.device_id);
            return;
        }
        // The map is held elsewhere right now; finish the cleanup from a
        // task rather than leaking the registration.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let supervisor = self.supervisor.clone();
            let device_id = self.device_id.clone();
            handle.spawn(async move {
                supervisor.state.write().await.remove(&device_id);
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
        let end = after
            .find(['&', ' ', '"', '\'', ')', ']', '\\'])
            .unwrap_or(after.len());
        rest = &after[end..];
    }
    out.push_str(rest);
    out
}
