//! The stream supervisor: one mint-and-swap loop per live camera.
//!
//! Qubo sessions last about five minutes and cannot be refreshed (the OTP
//! route issues no refresh token), so a live stream is re-minted on a
//! cadence and the session is re-read from the store on every mint - a
//! fresh login heals the streams without restarting the server.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use qubo::{Device, Quality, QuboClient, SessionStore, list_devices, mint_live};
use tokio::sync::RwLock;

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

/// The streams the server is keeping alive.
#[derive(Clone)]
pub struct Supervisor {
    qubo: QuboClient,
    sessions: SessionStore,
    go2rtc: Arc<Go2rtc>,
    pacing: Pacing,
    state: Arc<RwLock<HashMap<String, StreamState>>>,
    devices: Arc<RwLock<Vec<Device>>>,
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
        }
    }

    /// Discover the account's devices and remember them.
    pub async fn refresh_devices(&self) -> Result<Vec<Device>, SupervisorError> {
        let session = self.sessions.load()?.ok_or(qubo::QuboError::Auth {
            code: None,
            message: "no Qubo session; log in".to_owned(),
        })?;
        let devices = list_devices(&self.qubo, &session).await?;
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
            .cloned()
            .unwrap_or(StreamState::Offline)
    }

    /// Ensure the camera's stream is running: mint, hand the source to
    /// go2rtc, and keep re-minting until the process ends. Later calls for
    /// the same camera reuse the running loop; a failed first mint leaves
    /// the stream retrying and the state `Degraded`.
    pub async fn ensure_live(&self, device_id: &str) -> Result<(), SupervisorError> {
        {
            let state = self.state.read().await;
            if state.contains_key(device_id) {
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

        let wait = {
            let mut state = self.state.write().await;
            if state.contains_key(device_id) {
                return Ok(());
            }
            state.insert(device_id.to_owned(), StreamState::Waking);
            drop(state);

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
        Ok(())
    }

    /// Reserved for M3: cloud-allocated VOD sessions for recordings. M1
    /// streams the live pipeline only, so this refuses rather than guesses.
    pub fn ensure_vod(&self, _device_id: &str, _timestamp: i64) -> Result<(), SupervisorError> {
        Err(SupervisorError::VodUnavailable)
    }

    async fn remint_loop(self, device: Device, mut wait: Duration) {
        loop {
            tokio::time::sleep(wait).await;
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
        let mint = mint_live(&self.qubo, &session, device, Quality::High)
            .await
            .map_err(|err| err.to_string())?;
        self.go2rtc
            .put_stream(&device.device_id, &live_source(&mint.url))
            .await
            .map_err(|err| err.to_string())
    }

    /// `Live` keeps its original `since` through successful re-mints.
    async fn mark_live(&self, device_id: &str) {
        let mut state = self.state.write().await;
        if !matches!(state.get(device_id), Some(StreamState::Live { .. })) {
            state.insert(
                device_id.to_owned(),
                StreamState::Live {
                    since: Instant::now(),
                },
            );
        }
    }

    async fn mark_degraded(&self, device_id: &str, reason: String) {
        self.state
            .write()
            .await
            .insert(device_id.to_owned(), StreamState::Degraded { reason });
    }
}
