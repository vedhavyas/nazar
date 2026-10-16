//! The stream supervisor: one mint-and-swap loop per live camera.
//!
//! Qubo sessions last about five minutes and cannot be refreshed (the OTP
//! route issues no refresh token), so a live stream is re-minted on a
//! cadence and the session is re-read from the store on every mint - a
//! fresh login heals the streams without restarting the server. The cloud
//! keeps one stream per camera (mints are last-mint-wins), so only the
//! high stream is minted; a tile's low rides it as a chain inside go2rtc
//! and is dropped once nobody has asked for it for a while.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use qubo::{Device, Quality, QuboClient, SessionStore, list_devices, mint_live, mint_vod};
use tokio::sync::{Notify, RwLock};

use crate::go2rtc::{Go2rtc, live_source, low_source};

/// The shortest wait between a playback stream's idle re-checks: a zero
/// window would spin.
const WATCH_FLOOR: Duration = Duration::from_millis(50);

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
    /// How long one minted URL is given for its producer to connect before
    /// the stream is re-minted. A fresh mint connects in about a second
    /// (measured 2026-10-16); this is several times that.
    pub connect_grace: Duration,
    /// How long a watched producer may stay unconnected before the stream
    /// surfaces degraded - the camera's push can lag for minutes (measured
    /// 52s on the hub 2026-10-16), so the window must outlast the lag. The
    /// watch evaluates it on every `liveness_poll` slice, so with the
    /// defaults the reason lands about 90s into the stall. Deliberately
    /// under `stream_idle`: a stream nobody watches is torn down on that
    /// window and a reason that only appeared after the teardown would
    /// reach nobody - while a watched stream is never torn down at all,
    /// so the reason keeps its whole margin in front of it.
    pub connect_window: Duration,
    /// How often, while waiting between mints, the loop asks go2rtc
    /// whether a watched stream's producer is actually up. Cheap - the API
    /// is loopback - and it is what notices a dead picture before the next
    /// mint's cadence would.
    pub liveness_poll: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            remint: Duration::from_secs(240),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            discovery_retry: Duration::from_secs(5),
            stream_idle: Duration::from_secs(120),
            connect_grace: Duration::from_secs(8),
            connect_window: Duration::from_secs(90),
            liveness_poll: Duration::from_secs(2),
        }
    }
}

/// The go2rtc stream name for a camera's quality class; the browser asks
/// for exactly this name over the proxy.
pub fn stream_name(device_id: &str, quality: Quality) -> String {
    format!("{device_id}-{}", quality.as_str())
}

/// A playback stream is a camera's name under this prefix: `vod-<device>`.
pub const VOD_PREFIX: &str = "vod-";

/// The go2rtc stream name for a camera's playback session.
pub fn vod_name(device_id: &str) -> String {
    format!("{VOD_PREFIX}{device_id}")
}

/// A started playback: the stream the player asks for, and how long the
/// server holds it without any sign of life.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VodStream {
    pub name: String,
    pub expires_in: u64,
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
    #[error("go2rtc: {0}")]
    Go2rtc(String),
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
        self.status_for_name(&stream_name(device_id, quality)).await
    }

    /// The state of one tracked stream by its go2rtc name.
    async fn status_for_name(&self, name: &str) -> StreamState {
        self.state
            .read()
            .await
            .get(name)
            .map_or(StreamState::Offline, |tracked| tracked.state.clone())
    }

    /// Remember a refused session so the UI can send the browser to login.
    fn settle_auth(&self, err: &qubo::QuboError) {
        if matches!(err, qubo::QuboError::Auth { .. }) {
            self.needs_login.store(true, Ordering::SeqCst);
        }
    }

    /// Run a session-authenticated call; a refusal spends the stored refresh
    /// token once and runs it again with the fresh session - the reference
    /// client's own rule (spec §3.2). What the refresh heals is persisted,
    /// so every later call starts from it.
    async fn with_fresh_session<T, F, Fut>(&self, call: F) -> Result<T, qubo::QuboError>
    where
        F: Fn(qubo::Session) -> Fut,
        Fut: std::future::Future<Output = Result<T, qubo::QuboError>>,
    {
        let session = self.sessions.load()?.ok_or(qubo::QuboError::Auth {
            code: None,
            message: "no Qubo session; log in".to_owned(),
        })?;
        match call(session.clone()).await {
            Err(qubo::QuboError::Auth { .. }) if session.refresh_token.is_some() => {
                let refreshed = qubo::auth::refresh_session(&self.qubo, &session).await?;
                self.sessions.save(&refreshed)?;
                call(refreshed).await
            }
            other => other,
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
    /// that is down. A stale-live stream needs no retry from the person:
    /// the loop's own liveness watch re-mints the moment a watched
    /// producer is found down, and Try again is for the degraded case.
    pub async fn ensure(
        &self,
        device_id: &str,
        quality: Quality,
        retry: bool,
    ) -> Result<StreamState, SupervisorError> {
        match quality {
            Quality::High => self.ensure_high(device_id, retry).await,
            Quality::Low => self.ensure_low(device_id, retry).await,
        }
    }

    /// Ensure the camera's cloud stream: the only stream minted.
    async fn ensure_high(
        &self,
        device_id: &str,
        retry: bool,
    ) -> Result<StreamState, SupervisorError> {
        let name = stream_name(device_id, Quality::High);
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
            supervisor.remint_loop(device, Duration::ZERO).await;
        });
        Ok(self.status_for(device_id, Quality::High).await)
    }

    /// Ensure the camera's tile rendition: a chain inside go2rtc off the
    /// cloud stream, which is asked for here too - the tile rides it, so it
    /// is what keeps the cloud stream alive.
    async fn ensure_low(
        &self,
        device_id: &str,
        retry: bool,
    ) -> Result<StreamState, SupervisorError> {
        let high_state = self.ensure_high(device_id, retry).await?;
        let name = stream_name(device_id, Quality::Low);
        let fresh = {
            let mut state = self.state.write().await;
            let fresh = !state.contains_key(&name);
            state
                .entry(name.clone())
                .or_insert_with(new_waking)
                .last_request = Instant::now();
            fresh
        };
        // The chain lands with the first ask, not with the loop's next
        // wake: the loop may be minutes into a re-mint cadence, and a tile
        // nobody will look at until then is a tile that never starts. A
        // re-ask re-asserts it too, which is what heals a chain go2rtc
        // refused while it was down.
        if fresh || retry {
            let high = stream_name(device_id, Quality::High);
            match self.go2rtc.put_stream(&name, &low_source(&high)).await {
                Ok(()) => self.settle(&name, high_state).await,
                Err(err) => {
                    let reason = redact(&err.to_string());
                    tracing::warn!(stream = %name, %reason, "the tile's chain did not take");
                    self.mark_degraded(&name, reason).await;
                }
            }
        }
        Ok(self.status_for(device_id, Quality::Low).await)
    }

    /// Start playback of `device` at `timestamp_ms`: mint a cloud session
    /// at the clip's instant, hand it to go2rtc as `vod-<device>`, and
    /// hold it until the player releases it or it goes both unasked-for
    /// and unwatched for the idle window. A second seek re-mints - the old
    /// session is released first, so one camera never runs two at once.
    pub async fn ensure_vod(
        &self,
        device_id: &str,
        timestamp_ms: i64,
    ) -> Result<VodStream, SupervisorError> {
        let device = {
            let devices = self.devices.read().await;
            devices
                .iter()
                .find(|device| device.device_id == device_id)
                .cloned()
        }
        .ok_or_else(|| SupervisorError::UnknownDevice(device_id.to_owned()))?;
        let name = vod_name(device_id);
        // A reference, so the retry after a refresh can run the same call.
        let device = &device;
        let mint = match self
            .with_fresh_session(|session| async move {
                mint_vod(&self.qubo, &session, device, timestamp_ms).await
            })
            .await
        {
            Ok(mint) => mint,
            Err(err) => {
                self.settle_auth(&err);
                return Err(err.into());
            }
        };
        // A seek replaces the session: the old stream is released before
        // the new source is set, so the cloud slot is never held twice.
        if let Some(seen_at) = self.last_request_at(&name).await {
            self.release_vod_if_idle(&name, seen_at).await;
        }
        self.go2rtc
            .put_stream(&name, &live_source(&mint.url))
            .await
            .map_err(|err| SupervisorError::Go2rtc(redact(&err.to_string())))?;
        let fresh = {
            let mut state = self.state.write().await;
            let fresh = !state.contains_key(&name);
            let entry = state.entry(name.clone()).or_insert_with(new_tracked);
            entry.last_request = Instant::now();
            entry.state = StreamState::Live {
                since: Instant::now(),
            };
            fresh
        };
        if fresh {
            let supervisor = self.clone();
            let reaped = name.clone();
            tokio::spawn(async move { supervisor.vod_reaper(reaped).await });
        }
        Ok(VodStream {
            name,
            // The only expiry this server enforces itself: the cloud's own
            // session lifetime was not measured, and the player releases
            // long before either matters.
            expires_in: self.pacing.stream_idle.as_secs(),
        })
    }

    /// Release the camera's playback stream now: the player left.
    pub async fn release_vod(&self, device_id: &str) -> bool {
        let name = vod_name(device_id);
        match self.last_request_at(&name).await {
            Some(seen_at) => self.release_vod_if_idle(&name, seen_at).await,
            None => false,
        }
    }

    /// Release a playback session once nobody asks for it and nobody
    /// watches it: a leaked VOD session burns a camera slot.
    async fn vod_reaper(self, name: String) {
        let idle = self.pacing.stream_idle;
        loop {
            let Some(last) = self.last_request_at(&name).await else {
                return; // released by the player
            };
            tokio::time::sleep(idle.saturating_sub(last.elapsed()).max(WATCH_FLOOR)).await;
            if self.last_request_at(&name).await != Some(last) {
                continue; // asked for again; wait out its own window
            }
            if self.watched(&name).await {
                // A viewer holds it open: re-check within a window, so
                // their leaving is noticed at all.
                tokio::time::sleep(idle.max(WATCH_FLOOR)).await;
                continue;
            }
            self.release_vod_if_idle(&name, last).await;
            return;
        }
    }

    /// Release a playback stream, if the entry this reaper decided about
    /// is still the entry that is there: the claim and the delete run
    /// under the state lock, so a seek landing mid-release waits and
    /// starts its own session rather than having it deleted under it.
    async fn release_vod_if_idle(&self, name: &str, seen_at: Instant) -> bool {
        let mut state = self.state.write().await;
        match state.get(name) {
            Some(tracked) if tracked.last_request == seen_at => {
                state.remove(name);
            }
            _ => return false,
        }
        if let Err(err) = self.go2rtc.delete_stream(name).await {
            tracing::warn!(stream = name, %err, "the playback stream could not be released");
        }
        tracing::info!(stream = name, "playback released");
        true
    }

    async fn remint_loop(self, device: Device, mut wait: Duration) {
        let name = stream_name(&device.device_id, Quality::High);
        let wake = self.wake_signal(&name).await;
        // Every stream idles out when nobody is watching and nobody has
        // asked: a demoted hero's high stream must not run for ever. The
        // consumer check keeps a watched one - a hero - alive regardless.
        let idle = self.pacing.stream_idle;
        let mut next_mint = Instant::now();
        let mut failed = false;
        // When the current run of watched-but-unconnected producers began,
        // and whether the window's degradation has been told.
        let mut stalled_since: Option<Instant> = None;
        let mut degraded_reported = false;
        loop {
            let now = Instant::now();
            // A stream with a viewer is not idle, whatever this server was
            // last asked: the viewer holds it open through go2rtc.
            let idle_since = if self.watched(&name).await {
                None
            } else {
                self.idle_for(&name).await
            };
            // Waited until the mint is due, or until the idle window is
            // reached - whichever comes first, so a viewer who leaves is
            // noticed without waiting out a re-mint. Sliced, so the
            // producer's health is checked while waiting too, not only
            // when the next mint comes due.
            let sleep_for = match idle_since {
                Some(idle_for) => next_mint
                    .saturating_duration_since(now)
                    .min(idle.saturating_sub(idle_for)),
                None => next_mint.saturating_duration_since(now).min(idle),
            }
            .min(self.pacing.liveness_poll);
            let forced = tokio::select! {
                () = tokio::time::sleep(sleep_for) => false,
                () = wake.notified() => {
                    tracing::info!(stream = %name, "remint forced by a retry");
                    true
                }
            };
            // The decision is made on what is true now, not on what was
            // true before the wait: a stream that reached its idle window
            // during the wait must not mint once more on its way out.
            let idle_since = if self.watched(&name).await {
                None
            } else {
                self.idle_for(&name).await
            };
            let seen_at = self.last_request_at(&name).await;
            if let (Some(idle_for), Some(seen_at)) = (idle_since, seen_at)
                && idle_for >= idle
                && self.teardown_if_idle(&name, seen_at).await
            {
                return;
            }
            // A wake that is not due to mint is only a re-check: the mint
            // waits for its own cadence, or for a person's Try again whose
            // last mint failed - that one skips the backoff. A retry that
            // arrived while a mint was already in flight is consumed when
            // that mint succeeded: the person's wish is satisfied, and a
            // stored permit must not mint again. A retired tile is dropped
            // on its own window rather than on the mint's.
            let retry_due = forced && failed;
            if !retry_due && Instant::now() < next_mint {
                self.reconcile_low(&name).await;
                if self
                    .stall_response(&name, &mut stalled_since, &mut degraded_reported)
                    .await
                {
                    failed = true;
                    // Paced, not immediate: a starved stream replaces its
                    // dead URL on the backoff, so a camera that is away for
                    // minutes is not hit with a mint a second.
                    wait = (wait * 2)
                        .max(self.pacing.retry_base)
                        .min(self.pacing.retry_max);
                    next_mint = Instant::now() + wait;
                }
                continue;
            }
            next_mint = match self.mint_and_swap(&device).await {
                Ok(()) => {
                    failed = false;
                    // A recovered stream starts its backoff over: the next
                    // failure waits retry_base, not the grown tail of the
                    // storm before it. A stall already established keeps
                    // its clock and its state - the window measures how
                    // long viewers have been starved, not how old one URL
                    // is - and the liveness watch is what ends it.
                    wait = Duration::ZERO;
                    self.reconcile_low(&name).await;
                    if stalled_since.is_none() {
                        degraded_reported = false;
                        self.mark_live(&name).await;
                    }
                    Instant::now() + self.pacing.remint
                }
                Err(reason) => {
                    tracing::warn!(stream = %name, %reason, "stream re-mint failed");
                    self.mark_degraded(&name, reason).await;
                    failed = true;
                    // The starvation clock is not restarted: a refused mint
                    // is another way the picture is dead, not a fresh
                    // window, and the reason must not flicker away from the
                    // person's Try again on a blip. Only the mint-failure
                    // backoff restarts.
                    wait = (wait * 2)
                        .max(self.pacing.retry_base)
                        .min(self.pacing.retry_max);
                    Instant::now() + wait
                }
            };
        }
    }

    /// One mint, handed straight to go2rtc. The source swaps under the
    /// stream's own name, so a viewer's stream URL survives re-mints. A mint
    /// the gateway turns away spends the stored refresh token once and runs
    /// again before the refusal is believed.
    async fn mint_and_swap(&self, device: &Device) -> Result<(), String> {
        let name = stream_name(&device.device_id, Quality::High);
        let mint = match self
            .with_fresh_session(|session| async move {
                mint_live(&self.qubo, &session, device, Quality::High).await
            })
            .await
        {
            Ok(mint) => mint,
            Err(err) => {
                self.settle_auth(&err);
                return Err(redact(&err.to_string()));
            }
        };
        self.needs_login.store(false, Ordering::SeqCst);
        self.go2rtc
            .put_stream(&name, &live_source(&mint.url))
            .await
            .map_err(|err| redact(&err.to_string()))?;
        // No liveness check here: go2rtc keeps no producer while nothing
        // consumes the stream, and the minted URL stays unspent until a
        // viewer is the one to dial it. Whether the picture is actually up
        // is the loop's liveness watch to judge, from the consumers' side.
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

    /// When the stream's producer is not up while viewers are attached -
    /// the moment that state began, as `now`. `None` when there is nothing
    /// to judge: no consumers (the URL is still unspent, a fresh one is
    /// what any viewer dials), or go2rtc unreadable (the next check will
    /// say).
    async fn watched_but_unconnected(&self, name: &str) -> Option<Instant> {
        let streams = self.go2rtc.streams().await.ok()?;
        let stream = streams.iter().find(|stream| stream.name == name)?;
        (stream.consumers > 0 && !stream.connected).then(Instant::now)
    }

    /// The liveness watch, once per loop slice: a viewer on a picture whose
    /// producer is not up can never recover on the current mint - the URL
    /// is one-shot - so `true` asks the loop for a fresh one, on the retry
    /// cadence and well before the next remint's. The window's degradation
    /// is re-asserted every slice past it, so no later mint quietly
    /// returns a starved stream to waking - the UI offers Try again only
    /// for degraded.
    async fn stall_response(
        &self,
        name: &str,
        stalled_since: &mut Option<Instant>,
        degraded_reported: &mut bool,
    ) -> bool {
        let Some(stalled) = self.watched_but_unconnected(name).await else {
            if stalled_since.take().is_some() {
                // The picture is back: the stall is over.
                *degraded_reported = false;
                self.mark_live(name).await;
            }
            return false;
        };
        let waiting = stalled_since.get_or_insert(stalled).elapsed();
        if waiting < self.pacing.connect_grace {
            return false;
        }
        if waiting >= self.pacing.connect_window {
            if !*degraded_reported {
                tracing::warn!(
                    stream = %name,
                    ?waiting,
                    "the producer has not connected; giving the reason"
                );
                *degraded_reported = true;
            }
            self.mark_degraded(name, "the producer has not connected".to_owned())
                .await;
        } else {
            tracing::warn!(
                stream = %name,
                ?waiting,
                "the producer has not connected; re-minting"
            );
            self.mark_waking(name).await;
        }
        true
    }

    /// Drop the stream only if the entry this loop decided about is still
    /// the entry that is there: a request that landed while the loop was
    /// deciding belongs to somebody who is watching, and must not have its
    /// stream deleted under it. The last request is the entry's identity -
    /// an elapsed duration just keeps growing and can never be compared.
    ///
    /// The entry is claimed - removed from the state - and the state lock
    /// is held until go2rtc has forgotten the stream: a request that lands
    /// while the deletes are in flight waits for the claim, finds no
    /// entry, and spawns a fresh stream, rather than being told Live about
    /// one that is being deleted under it. Every await under that lock is
    /// bounded by the go2rtc client's own timeouts (and the two deletes run
    /// together rather than one after the other), so an API that accepts
    /// and then never answers wedges a status read for one timeout, never
    /// for the process's lifetime.
    async fn teardown_if_idle(&self, name: &str, seen_at: Instant) -> bool {
        let mut state = self.state.write().await;
        match state.get(name) {
            Some(tracked) if tracked.last_request == seen_at => {
                state.remove(name);
            }
            _ => {
                tracing::debug!(stream = name, "a request landed; the stream is not idle");
                return false;
            }
        }
        // Only the tile that is still tracked: one its own reconcile
        // already retired is gone from go2rtc too.
        let tile = low_name_of(name).filter(|low| state.remove(low).is_some());
        match tile {
            Some(low) => {
                let (tile_deleted, dropped) = tokio::join!(
                    self.go2rtc.delete_stream(&low),
                    self.go2rtc.delete_stream(name),
                );
                if let Err(err) = tile_deleted {
                    tracing::warn!(stream = %low, %err, "the tile rendition could not be dropped");
                }
                if let Err(err) = dropped {
                    tracing::warn!(stream = name, %err, "the idle stream could not be dropped");
                }
            }
            None => {
                if let Err(err) = self.go2rtc.delete_stream(name).await {
                    tracing::warn!(stream = name, %err, "the idle stream could not be dropped");
                }
            }
        }
        tracing::info!(stream = name, "idle stream dropped");
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

    /// Keep the tile rendition honest after a mint: re-assert its chain, or
    /// retire it once nobody asks for it and nobody watches it. The cloud
    /// stream it rides may still be needed by a hero.
    async fn reconcile_low(&self, high: &str) {
        let Some(low) = low_name_of(high) else {
            return;
        };
        let Some(last) = self.last_request_at(&low).await else {
            return;
        };
        if last.elapsed() >= self.pacing.stream_idle {
            if !self.watched(&low).await {
                self.drop_low_if_idle(&low, last).await;
            }
            return;
        }
        match self.go2rtc.put_stream(&low, &low_source(high)).await {
            Ok(()) => {
                let live = self.status_for_name(high).await;
                self.settle(&low, live).await;
            }
            Err(err) => {
                let reason = redact(&err.to_string());
                tracing::warn!(stream = %low, %reason, "the tile's chain did not take");
                self.mark_degraded(&low, reason).await;
            }
        }
    }

    /// Drop a retired tile rendition, if the entry this cycle decided
    /// about is still the entry that is there: a tile asked for while the
    /// cycle was deciding must not be dropped under it. The claim and the
    /// delete both run under the state lock, the same shape as the cloud
    /// stream's teardown: a tile asked for mid-drop waits and registers a
    /// fresh chain, rather than having the chain it just asked for deleted
    /// under it.
    async fn drop_low_if_idle(&self, low: &str, seen_at: Instant) -> bool {
        let mut state = self.state.write().await;
        match state.get(low) {
            Some(tracked) if tracked.last_request == seen_at => {
                state.remove(low);
            }
            _ => return false,
        }
        if let Err(err) = self.go2rtc.delete_stream(low).await {
            tracing::warn!(stream = low, %err, "the retired tile rendition could not be dropped");
        }
        tracing::info!(stream = low, "retired tile rendition dropped");
        true
    }

    /// Whether go2rtc has a viewer on the stream, or - for a cloud stream -
    /// on the tile rendition that rides it. An API that cannot say keeps
    /// the stream: a late teardown beats a wrong one.
    async fn watched(&self, name: &str) -> bool {
        let low = low_name_of(name);
        self.go2rtc.streams().await.map_or(true, |streams| {
            streams.iter().any(|stream| {
                stream.consumers > 0
                    && (stream.name == name || low.as_deref() == Some(stream.name.as_str()))
            })
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

    /// `Live` keeps its original `since` through successful re-mints. A
    /// tile rendition has no life of its own: it mirrors the stream.
    async fn mark_live(&self, name: &str) {
        let mut state = self.state.write().await;
        let entry = state.entry(name.to_owned()).or_insert_with(new_tracked);
        if !matches!(entry.state, StreamState::Live { .. }) {
            entry.state = StreamState::Live {
                since: Instant::now(),
            };
        }
        let live = entry.state.clone();
        if let Some(low) = low_name_of(name)
            && let Some(tile) = state.get_mut(&low)
        {
            tile.state = live;
        }
    }

    /// A stream whose producer has not connected yet: waking, not live, and
    /// not yet a failure - the camera's push can lag for minutes, and the
    /// loop keeps re-minting a fresh URL until it lands. The tile mirrors
    /// it, as with every other state.
    async fn mark_waking(&self, name: &str) {
        let mut state = self.state.write().await;
        let entry = state.entry(name.to_owned()).or_insert_with(new_tracked);
        entry.state = StreamState::Waking;
        if let Some(low) = low_name_of(name)
            && let Some(tile) = state.get_mut(&low)
        {
            tile.state = StreamState::Waking;
        }
    }

    async fn mark_degraded(&self, name: &str, reason: String) {
        let mut state = self.state.write().await;
        state
            .entry(name.to_owned())
            .or_insert_with(new_tracked)
            .state = StreamState::Degraded {
            reason: reason.clone(),
        };
        if let Some(low) = low_name_of(name)
            && let Some(tile) = state.get_mut(&low)
        {
            tile.state = StreamState::Degraded { reason };
        }
    }

    /// Set a tracked stream's state to the state of the stream it mirrors.
    async fn settle(&self, name: &str, state: StreamState) {
        self.state
            .write()
            .await
            .entry(name.to_owned())
            .or_insert_with(new_tracked)
            .state = state;
    }
}

/// The tile rendition's name for a cloud stream's name.
fn low_name_of(high: &str) -> Option<String> {
    high.strip_suffix("-high")
        .map(|device| format!("{device}-low"))
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
    //! Here the window is opened by hand, between the two calls. The other
    //! half of the race - a request landing while the teardown's deletes
    //! are in flight - is pinned in supervisor_quality, where the fake
    //! go2rtc can be made slow to delete.

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
            Go2rtc::connect("http://127.0.0.1:9".parse().expect("a url")).expect("a client"),
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
        // The entry is gone with the decision: a request arriving after
        // the teardown finds nothing to be told Live about, and spawns a
        // fresh stream instead.
        assert_eq!(
            supervisor.last_request_at(&name).await,
            None,
            "the claim takes the entry before the stream is deleted"
        );
    }
}
