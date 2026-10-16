//! The stream supervisor: one mint-and-swap loop per live camera.
//!
//! Qubo access tokens last about an hour; a stored 180-day refresh token
//! heals them, spent once per refused call wherever a refusal is visible -
//! a mint, a discovery, the feed's re-resolve. With no refresh left, a
//! refusal routes the UI to login. A live stream is re-minted on a cadence
//! and the session is re-read from the store on every mint. The cloud
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

/// A consumer narar keeps on its own stream, so go2rtc's producer stays
/// warm between viewers: a client that connects finds a picture already
/// flowing instead of waiting out a mint and a dial. It lives exactly as
/// long as the stream's loop, so the idle teardown still parks a stream
/// nobody asks for and the hold goes cold with it - and it never counts
/// as a viewer: `watched` subtracts it.
pub trait StreamHold: Send + Sync {
    /// Begin holding `name`; dropping the last reference to the lease
    /// releases the consumer.
    fn hold(&self, name: &str) -> Arc<dyn HoldLease>;
}

/// A running hold: it releases on drop, and reports whether a real
/// consumer is attached right now.
pub trait HoldLease: Send + Sync {
    /// Whether the consumer actually attached: the subtraction counts
    /// what is attached, never the intent to attach - a hold whose ffmpeg
    /// has not come up (or keeps dying) must not shadow a real viewer.
    fn attached(&self) -> bool;
}

/// The leases narar runs, by stream name: keyed per name but removed by
/// lease identity, so a parked loop's late drop can never erase the
/// registration a respawn just made.
type HoldRegistry = Arc<Mutex<HashMap<String, Vec<Arc<dyn HoldLease>>>>>;

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
    /// the stream is re-minted. A fresh dial takes 6-12s (measured
    /// 2026-10-16), so the grace must clear the whole range: under it, a
    /// merely-slow dial gets its in-flight attach killed by the re-mint,
    /// the state flips to waking, and the tile's player unmounts. The
    /// tradeoff: a genuinely spent URL's heal waits the grace. Still far
    /// under `connect_window`.
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
    /// How long after an ask a viewer still counts as arriving. It only
    /// gates STARTING a run of starvation - a run already open ends on the
    /// producer alone - so it must merely outlast the gap between the
    /// watch's own polls with margin; silence minutes long opens nothing
    /// and closes nothing.
    pub touch_ttl: Duration,
    /// How long a stream warmed by a login or a boot survives without
    /// anyone asking. Generous on purpose: the warmth is bought so the
    /// next client to load finds pictures already flowing, and a session
    /// of browsing is shorter than this - while a stream nobody asks for
    /// past the window still drops, which is the trade Ved named (warm on
    /// access, dropped when unasked).
    pub warm_idle: Duration,
}

impl Default for Pacing {
    fn default() -> Self {
        Self {
            remint: Duration::from_secs(240),
            retry_base: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
            discovery_retry: Duration::from_secs(5),
            stream_idle: Duration::from_secs(120),
            connect_grace: Duration::from_secs(20),
            connect_window: Duration::from_secs(90),
            liveness_poll: Duration::from_secs(2),
            touch_ttl: Duration::from_secs(30),
            warm_idle: Duration::from_mins(30),
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

/// A go2rtc stream source as this server names them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamSrc {
    /// A live camera's quality class.
    Live { camera: String, quality: Quality },
    /// A camera's playback session.
    Vod { camera: String },
}

/// Parse a stream source: `<camera>-high`, `<camera>-low`, or
/// `vod-<camera>`. One grammar for the proxy's validation and the viewer
/// clock's normalization, so the two cannot drift.
pub fn parse_stream_src(src: &str) -> Option<StreamSrc> {
    if let Some(rest) = src.strip_prefix(VOD_PREFIX) {
        return (!rest.is_empty()).then(|| StreamSrc::Vod {
            camera: rest.to_owned(),
        });
    }
    let (camera, quality) = src.rsplit_once('-')?;
    let quality = match quality {
        "high" => Quality::High,
        "low" => Quality::Low,
        _ => return None,
    };
    (!camera.is_empty()).then(|| StreamSrc::Live {
        camera: camera.to_owned(),
        quality,
    })
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

/// A run of starvation: viewers are on a picture whose producer is not up.
/// It ends when the producer connects - or, past the window, when the
/// reason is the state - and only silence opens a new one, never closes an
/// open one: the asker may be a tile that unmounted the moment we told it
/// the stream is waking.
struct Stall {
    since: Option<Instant>,
    reported: bool,
    attempts: u32,
    /// The delay before the next attempt; compounds per attempt.
    wait: Duration,
}

impl Stall {
    fn new() -> Self {
        Self {
            since: None,
            reported: false,
            attempts: 0,
            wait: Duration::ZERO,
        }
    }

    /// Start a run if none is running; the first `started` wins, so the
    /// window measures from the first sighting, not the latest.
    fn open(&mut self, started: Instant) -> Instant {
        *self.since.get_or_insert(started)
    }

    /// One more attempt against the same run: the backoff compounds.
    fn attempted(&mut self, pacing: &Pacing) {
        self.attempts += 1;
        self.wait = (self.wait * 2).max(pacing.retry_base).min(pacing.retry_max);
    }

    /// The producer answered: yield how long the run starved and how many
    /// attempts it took.
    fn close(&mut self) -> Option<(Duration, u32)> {
        let since = self.since.take()?;
        let out = (since.elapsed(), self.attempts);
        self.reported = false;
        self.attempts = 0;
        self.wait = Duration::ZERO;
        Some(out)
    }
}

/// What the liveness watch wants from the loop this slice.
enum StallStep {
    /// Nothing to do.
    Idle,
    /// The run wants its next attempt pulled forward.
    Remint,
    /// A run just closed: a deadline armed for its next attempt would fire
    /// late and swap a working URL, so the pace must be re-armed.
    Closed,
}

/// What the liveness watch can say about the stream's producer.
enum ProducerWatch {
    /// The producer is up: any run of starvation is over.
    Up,
    /// Not up, and somebody is on it: a viewer attached, or an ask fresh
    /// enough to count - start a run at that ask's own clock.
    Stalled(Instant),
    /// Not up, and nobody has asked recently: nothing to judge.
    Quiet,
    /// go2rtc could not say: the run, if any, stands unwitnessed - an
    /// unreadable API is not a recovery.
    Unreadable,
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
    /// When a viewer last asked for a stream, from the server's own
    /// vantage points (the proxy's signalling socket and HLS master): the
    /// only sighting that survives a failed attach, which go2rtc's API
    /// does not trace at all.
    touches: Arc<Mutex<HashMap<String, Instant>>>,
    /// The hold narar keeps on its own streams, when one is configured.
    hold: Option<Arc<dyn StreamHold>>,
    /// Set when a login or a boot wants the house warmed; the first
    /// discovery that succeeds after it starts `warm_all` once.
    warm_wanted: Arc<AtomicBool>,
    /// Whether `warm_all` has run for that wish.
    warmed: Arc<AtomicBool>,
    /// The leases this server currently runs, by stream: a viewer count
    /// subtracts only the leases that actually attached. Keyed by name
    /// with per-lease identity, so a parked loop's late drop can never
    /// erase the registration a respawn just made.
    holds: HoldRegistry,
}

/// narar's own consumer on a stream, held for the loop's lifetime and
/// released on drop.
struct HeldStream {
    name: String,
    holds: HoldRegistry,
    lease: Arc<dyn HoldLease>,
}

impl Drop for HeldStream {
    fn drop(&mut self) {
        let mut holds = self
            .holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(leases) = holds.get_mut(&self.name) {
            leases.retain(|lease| !Arc::ptr_eq(lease, &self.lease));
            if leases.is_empty() {
                holds.remove(&self.name);
            }
        }
    }
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
            touches: Arc::new(Mutex::new(HashMap::new())),
            hold: None,
            holds: Arc::new(Mutex::new(HashMap::new())),
            warm_wanted: Arc::new(AtomicBool::new(false)),
            warmed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Ask that the house be warmed: the first discovery that succeeds
    /// from here on starts every camera, once. A boot whose discovery
    /// fails still warms when a later poll's discovery lands.
    pub fn warm_on_discovery(&self) {
        self.warm_wanted.store(true, Ordering::SeqCst);
    }

    /// Keep a warm consumer on every tracked stream, so connecting clients
    /// find the picture already flowing.
    #[must_use]
    pub fn with_hold(mut self, hold: Arc<dyn StreamHold>) -> Self {
        self.hold = Some(hold);
        self
    }

    /// Start narar's own consumer on `name`, registered so viewer counts
    /// can subtract it. `None` without a configured hold.
    fn start_hold(&self, name: &str) -> Option<HeldStream> {
        let hold = self.hold.as_ref()?;
        let lease = hold.hold(name);
        self.holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .entry(name.to_owned())
            .or_default()
            .push(Arc::clone(&lease));
        Some(HeldStream {
            name: name.to_owned(),
            holds: Arc::clone(&self.holds),
            lease,
        })
    }

    /// Whether narar's own hold is attached on `name` right now.
    fn held(&self, name: &str) -> bool {
        self.holds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .is_some_and(|leases| leases.iter().any(|lease| lease.attached()))
    }

    /// A viewer's ask, as narar itself sees it. The ask is normalized to
    /// the cloud stream: a tile's ask for the low rendition counts for the
    /// high stream that carries it, and a playback stream's ask counts for
    /// nothing here - its own lifecycle judges it.
    pub fn note_viewer_touch(&self, src: &str) {
        let Some(name) = touched_high(src) else {
            return;
        };
        self.touches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(name, Instant::now());
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
        // A refused call spends the stored refresh token once, exactly as
        // the mints do: an idle hub's hour-expired access token must not
        // strand the house at the login screen while a 180-day refresh
        // sits in the store.
        let devices = match self
            .with_fresh_session(|session| async move { list_devices(&self.qubo, &session).await })
            .await
        {
            Ok(devices) => devices,
            Err(err) => {
                self.settle_auth(&err);
                return Err(err.into());
            }
        };
        self.needs_login.store(false, Ordering::SeqCst);
        *self.devices.write().await = devices.clone();
        // Warm on access: a login or a boot asked for it, and this is a
        // discovery that actually landed - on the boot thread, or on a
        // later poll after the boot's discovery failed.
        if self.warm_wanted.load(Ordering::SeqCst) && !self.warmed.swap(true, Ordering::SeqCst) {
            let supervisor = self.clone();
            tokio::spawn(async move { supervisor.warm_all().await });
        }
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
                // Single flight: the boot pair and the feed race here by
                // construction (both refuse on the same expired token),
                // and a second spend of the same pair is a refusal the
                // gateway may well answer with a rotation - leaving the
                // login flag raised on a session that was just healed. A
                // store that no longer holds the refused token is that
                // heal; retrying with it skips the spend.
                if let Some(current) = self.sessions.load()?
                    && current.access_token != session.access_token
                {
                    return call(current).await;
                }
                let refreshed = qubo::auth::refresh_session(&self.qubo, &session).await?;
                self.sessions.save(&refreshed)?;
                call(refreshed).await
            }
            other => other,
        }
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
        let idle = self.pacing.stream_idle;
        tokio::spawn(async move {
            supervisor.remint_loop(device, Duration::ZERO, idle).await;
        });
        Ok(self.status_for(device_id, Quality::High).await)
    }

    /// Start the camera's stream in the background, on the generous idle a
    /// warmed stream gets: the caller is a login or a boot, not a viewer.
    /// Failures are the loop's to log - warming must never fail a login -
    /// and an already-tracked stream is left where it is.
    pub async fn warm(&self, device_id: &str) {
        let name = stream_name(device_id, Quality::High);
        let device = self
            .devices
            .read()
            .await
            .iter()
            .find(|device| device.device_id == device_id)
            .cloned();
        let Some(device) = device else {
            tracing::warn!(device = device_id, "warming skipped an unknown camera");
            return;
        };
        {
            let mut state = self.state.write().await;
            if state.contains_key(&name) {
                return;
            }
            state.insert(name.clone(), new_waking());
        }
        let supervisor = self.clone();
        let idle = self.pacing.warm_idle;
        tokio::spawn(async move {
            supervisor.remint_loop(device, Duration::ZERO, idle).await;
        });
    }

    /// Warm every known camera: the house starts minting and holding as
    /// soon as access is given, so the first client to load finds pictures
    /// already flowing. Best effort by design - each camera runs its own
    /// background loop, and nothing here raises.
    pub async fn warm_all(&self) {
        let cameras: Vec<String> = self
            .devices()
            .await
            .iter()
            .filter(|device| device.is_camera())
            .map(|device| device.device_id.clone())
            .collect();
        tracing::info!(cameras = cameras.len(), "warming every camera");
        for device_id in cameras {
            self.warm(&device_id).await;
        }
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
        // The live path's contract, kept: the entry must be held, or the
        // mint went nowhere. Connectedness is deliberately NOT demanded
        // here, and the live path cannot demand it at this instant either:
        // a producer dials on its first consumer - for a VOD the player
        // that just asked - so neither is connected yet. What the live
        // path has behind the mark and this one does not is the watch,
        // which exists because a backgrounded live tile cannot show its
        // own failure; a playback player is on screen showing the failure
        // itself, and its session is superseded or released, not healed.
        let held = self
            .go2rtc
            .streams()
            .await
            .map_err(|err| SupervisorError::Go2rtc(redact(&err.to_string())))?
            .iter()
            .any(|stream| stream.name == name && stream.live);
        if !held {
            // The source landed but go2rtc lost it between the ack and
            // the read: leave nothing behind for nobody to release.
            if let Err(err) = self.go2rtc.delete_stream(&name).await {
                tracing::warn!(stream = %name, %err, "the untaken playback stream could not be dropped");
            }
            tracing::warn!(stream = %name, "go2rtc did not take the playback stream");
            return Err(SupervisorError::Go2rtc(
                "go2rtc did not take the stream".to_owned(),
            ));
        }
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

    async fn remint_loop(self, device: Device, mut wait: Duration, idle: Duration) {
        let name = stream_name(&device.device_id, Quality::High);
        // narar's own consumer starts with the first landed PUT, not with
        // the loop: a hold dialled against a stream go2rtc has not been
        // given yet 404s, counts as a young death, and delays the warmth.
        // It goes cold with the loop when the idle teardown parks it.
        let mut held: Option<HeldStream> = None;
        let wake = self.wake_signal(&name).await;
        // Every stream idles out when nobody is watching and nobody has
        // asked: a demoted hero's high stream must not run for ever. The
        // consumer check keeps a watched one - a hero - alive regardless.
        let mut next_mint = Instant::now();
        let mut failed = false;
        let mut stall = Stall::new();
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
                match self.stall_response(&name, &mut stall).await {
                    StallStep::Remint => {
                        failed = true;
                        // Pulled forward, never pushed back: a starved
                        // stream replaces its dead URL on the run's own
                        // backoff - so a camera away for minutes is not hit
                        // with a mint a second, and a backoff longer than
                        // the watch's own poll still comes due.
                        next_mint =
                            next_mint.min(Instant::now() + stall.wait.max(self.pacing.retry_base));
                    }
                    StallStep::Closed => {
                        // The recovery itself clears the pace: the deadline
                        // armed for the run's next attempt is stale, and so
                        // is a mint-failure backoff grown while the run was
                        // open.
                        failed = false;
                        wait = Duration::ZERO;
                        next_mint = Instant::now() + self.pacing.remint;
                    }
                    StallStep::Idle => {}
                }
                continue;
            }
            if stall.since.is_some() {
                stall.attempted(&self.pacing);
                tracing::info!(
                    stream = %name,
                    attempt = stall.attempts,
                    starved = ?stall.since.map(|since| since.elapsed()),
                    "re-minting a starved stream"
                );
            }
            next_mint = match self.mint_and_swap(&device).await {
                Ok(()) => {
                    failed = false;
                    if held.is_none() {
                        held = self.start_hold(&name);
                    }
                    self.reconcile_low(&name).await;
                    if stall.since.is_none() {
                        // A recovered stream starts its backoff over: the
                        // next failure waits retry_base, not the grown tail
                        // of the storm before it. While a run is open the
                        // mint was one more attempt at a picture that has
                        // not arrived, and the run's own backoff - not this
                        // one - is what paces the next attempt.
                        wait = Duration::ZERO;
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
        // consumes the stream, and the minted URL stays unspent until its
        // first consumer dials it - a viewer, or narar's own hold, which
        // with a hold configured is the dial that makes the stream warm.
        // Whether the picture is actually up is the loop's liveness watch
        // to judge, from the consumers' side.
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

    /// What the server can see of the stream's producer right now. A
    /// viewer shows up as a consumer once a session opens, or as the ask
    /// itself - the proxy's sighting of a socket or a playlist request -
    /// which is the only trace a failed attach leaves anywhere (measured
    /// live 2026-10-16). A stream entry that is gone reads as not up: the
    /// producer is certainly not, and a mint is what brings the entry
    /// back.
    async fn watch_producer(&self, name: &str) -> ProducerWatch {
        let streams = match self.go2rtc.streams().await {
            Ok(streams) => streams,
            Err(err) => {
                tracing::warn!(stream = %name, %err, "the liveness watch could not read go2rtc");
                return ProducerWatch::Unreadable;
            }
        };
        let stream = streams.iter().find(|stream| stream.name == name);
        if stream.is_some_and(|stream| stream.connected) {
            return ProducerWatch::Up;
        }
        // The ask is the clock: the grace runs from the viewer's own
        // attempt, not from when a poll noticed it.
        let asked = self
            .touches
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(name)
            .copied()
            .filter(|at| at.elapsed() <= self.pacing.touch_ttl);
        if let Some(at) = asked {
            return ProducerWatch::Stalled(at);
        }
        if stream.is_some_and(|stream| stream.consumers > usize::from(self.held(name))) {
            return ProducerWatch::Stalled(Instant::now());
        }
        ProducerWatch::Quiet
    }

    /// The liveness watch, once per loop slice: `Remint` asks the loop for
    /// a fresh mint, on the run's own backoff and well before the next
    /// remint's cadence. A run that is open stays open through silence -
    /// its window and its reason must still land - and only the producer
    /// connecting closes it; an unreadable or absent producer leaves it
    /// standing.
    async fn stall_response(&self, name: &str, stall: &mut Stall) -> StallStep {
        match self.watch_producer(name).await {
            ProducerWatch::Up => {
                let Some((starved, attempts)) = stall.close() else {
                    return StallStep::Idle;
                };
                tracing::info!(
                    stream = %name,
                    ?starved,
                    attempts,
                    "the producer connected; the stall is over"
                );
                self.mark_live(name).await;
                StallStep::Closed
            }
            ProducerWatch::Unreadable => StallStep::Idle,
            ProducerWatch::Quiet => {
                let Some(since) = stall.since else {
                    return StallStep::Idle;
                };
                self.stall_cadence(name, stall, since).await
            }
            ProducerWatch::Stalled(started) => {
                let since = stall.open(started);
                self.stall_cadence(name, stall, since).await
            }
        }
    }

    /// The cadence a starved run keeps: waking past the grace, degraded
    /// past the window - the reason re-asserted every slice, so no later
    /// mint quietly returns the stream to waking. The UI offers Try again
    /// only for degraded. The grace is what covers a merely-slow dial: a
    /// fresh camera push takes 6-12s (measured 2026-10-16), so the first
    /// attempt is armed no earlier than the grace's end rather than on the
    /// base backoff, which would swap the URL out from under the dial.
    async fn stall_cadence(&self, name: &str, stall: &mut Stall, since: Instant) -> StallStep {
        let waiting = since.elapsed();
        if waiting < self.pacing.connect_grace {
            return StallStep::Idle;
        }
        if waiting >= self.pacing.connect_window {
            if !stall.reported {
                tracing::warn!(
                    stream = %name,
                    ?waiting,
                    "the producer has not connected; giving the reason"
                );
                stall.reported = true;
            }
            self.mark_degraded(name, "the producer has not connected".to_owned())
                .await;
        } else {
            self.mark_waking(name).await;
        }
        StallStep::Remint
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
        // Re-asserted only when go2rtc has lost it: every PUT rewrites
        // go2rtc's whole config on disk, and a healthy chain needs none of
        // that on a per-slice heartbeat. An entry that merely exists is
        // trusted - it carries the chain's source from the PUT that made
        // it, and a chain go2rtc refused never got an entry at all.
        let present = self
            .go2rtc
            .streams()
            .await
            .is_ok_and(|streams| streams.iter().any(|stream| stream.name == low));
        if present {
            let live = self.status_for_name(high).await;
            self.settle(&low, live).await;
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
        // narar's own hold is not a viewer: only the consumers beyond it
        // count.
        let own = usize::from(self.held(name));
        self.go2rtc.streams().await.map_or(true, |streams| {
            streams.iter().any(|stream| {
                let theirs = if stream.name == name {
                    stream.consumers.saturating_sub(own)
                } else {
                    stream.consumers
                };
                theirs > 0 && (stream.name == name || low.as_deref() == Some(stream.name.as_str()))
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
    /// tile rendition has no life of its own: it mirrors the stream - and
    /// the mirror is sound because a rendition's producer, like a live
    /// one's, dials on its first consumer (the tile's own player), with
    /// the chain's re-assert behind it when the original PUT never took.
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

/// The cloud stream a viewer's ask belongs to: a live stream's own name,
/// the high stream behind a tile's low rendition, and nothing for a
/// playback stream or anything unrecognized.
fn touched_high(src: &str) -> Option<String> {
    match parse_stream_src(src)? {
        StreamSrc::Live { camera, .. } => Some(stream_name(&camera, Quality::High)),
        StreamSrc::Vod { .. } => None,
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

    #[test]
    fn the_default_window_lands_before_the_idle_teardown() {
        let pacing = Pacing::default();
        assert!(
            pacing.connect_grace < pacing.connect_window,
            "the first attempt waits out a whole slow dial before the window can end it"
        );
        assert!(
            pacing.connect_window < pacing.stream_idle,
            "the degradation must surface while a stream nobody asks for is still alive"
        );
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

    /// A hold whose lease is observable: enough for the registry test.
    struct RegistryHold;

    struct RegistryLease;

    impl HoldLease for RegistryLease {
        fn attached(&self) -> bool {
            true
        }
    }

    impl StreamHold for RegistryHold {
        fn hold(&self, _name: &str) -> Arc<dyn HoldLease> {
            Arc::new(RegistryLease)
        }
    }

    /// Registry identity: a parked loop's late drop must never erase a
    /// fresh registration, or the stream would never park and phantom
    /// runs would open with nobody watching.
    #[test]
    fn a_late_drop_never_erases_a_fresh_registration() {
        let supervisor = quiet_supervisor().with_hold(Arc::new(RegistryHold));
        let first = supervisor.start_hold("cam-high").expect("a hold");
        let second = supervisor.start_hold("cam-high").expect("a respawned hold");
        assert!(supervisor.held("cam-high"), "two leases, held");

        drop(first);
        assert!(
            supervisor.held("cam-high"),
            "the fresh registration stands after the old loop's drop"
        );
        drop(second);
        assert!(
            !supervisor.held("cam-high"),
            "with the last lease gone, nothing is held"
        );
    }

    /// The subtraction counts attachment, not intent: a lease that never
    /// attached is invisible to every viewer count.
    #[test]
    fn an_unattached_lease_is_not_subtracted() {
        struct NeverAttaches;
        struct DeadLease;
        impl HoldLease for DeadLease {
            fn attached(&self) -> bool {
                false
            }
        }
        impl StreamHold for NeverAttaches {
            fn hold(&self, _name: &str) -> Arc<dyn HoldLease> {
                Arc::new(DeadLease)
            }
        }

        let supervisor = quiet_supervisor().with_hold(Arc::new(NeverAttaches));
        let lease = supervisor.start_hold("cam-high").expect("a hold");
        assert!(
            !supervisor.held("cam-high"),
            "a hold that never attached must not shadow a viewer"
        );
        drop(lease);
    }
}
