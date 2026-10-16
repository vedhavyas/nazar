//! The P2P fast lane: the ported reference client (`reference/p2p_client.py`).
//!
//! `P2pEngine::arm` runs the reference's chain - allocation, device-token
//! preflight, srflx lookup, the start call over its own broker connection,
//! then the relay/direct UDP handshake - and hands back an [`ArmedSession`]
//! whose decrypted RTP feeds the server's delivery pipeline. Every stage is
//! bounded: a failure is an error for the caller's fallback chain, never a
//! hang. The wire grammar is documented in `docs/qubo-protocol.md`.
//!
//! The fast lane is unproven until Task 2 observes the relay on a genuine
//! login: the media AES mode in particular (`media::AesMode`) is a switch,
//! not a fact.

pub mod media;
pub mod ptt;
pub mod relay;
pub mod signaling;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use rand::Rng as _;
use rumqttc::QoS;
use serde_json::Value;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::client::QuboClient;
use crate::devices::Device;
use crate::error::QuboError;
use crate::session::Session;

pub use media::{AesMode, H264Assembler, MediaDecryptor, RtpHeader, decode_key16, unwrap_rtp};
pub use relay::{Established, HandshakeConfig, Packet, Txid};
pub use signaling::{
    Allocation, Callback, Candidate, MediaStreams, Protocols, StartCall, StreamQuality,
    build_candidate_list, build_end_call, build_start_call, candidate_endpoints,
    parse_p2p_callback,
};

/// The broker the app itself uses (protocol 1.1).
const BROKER_HOST: &str = "mqtt.platform.quboworld.com";
const BROKER_PORT: u16 = 8883;
const BROKER_KEEP_ALIVE: Duration = Duration::from_secs(30);
/// Bound on requests in flight; the engine subscribes to one topic.
const BROKER_CAPACITY: usize = 32;
/// Broker reconnect backoff, the paho shape (the reference's client backs
/// off 1s..120s; a session lives minutes, so the cap is the session's own
/// scale). Without it, a broker that fails fast spins the pump.
const RETRY_MIN: Duration = Duration::from_secs(1);
const RETRY_MAX: Duration = Duration::from_secs(30);

/// The next reconnect delay: 1s, then doubling, capped.
fn next_backoff(current: Duration) -> Duration {
    if current.is_zero() {
        RETRY_MIN
    } else {
        (current * 2).min(RETRY_MAX)
    }
}

/// How long a quiet media socket may be before `recv_media` reports idle.
const MEDIA_IDLE: Duration = Duration::from_millis(500);

/// Everything that can go wrong arming or running the fast lane.
#[derive(Debug, thiserror::Error)]
pub enum P2pError {
    #[error(transparent)]
    Qubo(#[from] QuboError),
    #[error("the p2p allocation failed: {0}")]
    Allocation(String),
    #[error("the session key is not 16 bytes of hex or base64")]
    KeyFormat,
    #[error("the control envelope is too large for a 2001 frame: {len} bytes")]
    EnvelopeTooLarge { len: usize },
    #[error("the handshake did not complete within {timeout:?} (relay engaged: {relay_engaged})")]
    Handshake {
        timeout: Duration,
        relay_engaged: bool,
    },
    #[error("no handshake targets: the answer carried no candidates and the allocation no relay")]
    NoTargets,
    #[error("the broker connection failed: {0}")]
    Broker(String),
    #[error("the broker event loop ended")]
    BrokerClosed,
    #[error("no media within {0:?} - the session is silent")]
    MediaSilence(Duration),
    #[error("talkback has not started")]
    NotTalking,
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// The arm's optional steps, each on or off.
#[derive(Debug, Clone)]
pub struct ArmSwitches {
    /// Fetch the device-access token before the start call, as the app does.
    pub device_token: bool,
    /// Wait for the relay's first packet before syncing the direct
    /// candidates - the app's invariant across all three captures.
    pub relay_gate: bool,
    /// Hold the SDK's keepalive beat on the relay socket once armed: a
    /// 52-byte SYN every ~300ms with a fresh txid every 2s - the cadence
    /// the 09:27 capture shows (83 same-flow SYNs over 57s, the SDK's ka
    /// timer). The reference stops SYN traffic once settled; the M6 Task 2
    /// run turns this on so a silent relay window cannot be dismissed as a
    /// cold-flow artifact.
    pub hold_beat: bool,
}

impl Default for ArmSwitches {
    fn default() -> Self {
        Self {
            device_token: true,
            relay_gate: true,
            hold_beat: false,
        }
    }
}

/// How the engine reaches the camera; the reference client's knobs, minus
/// its CLI-only experiments.
#[derive(Debug, Clone)]
pub struct P2pConfig {
    pub protocols: signaling::Protocols,
    pub media: signaling::MediaStreams,
    pub ptt: bool,
    /// The STUN server for the srflx candidate; `None` skips the lookup.
    pub stun_server: Option<String>,
    /// The optional arm steps.
    pub switches: ArmSwitches,
    /// The local address the media socket binds to.
    pub bind: Option<IpAddr>,
    /// The advertised host ip; defaults to the bind address, else the
    /// default-route address.
    pub host_ip: Option<String>,
    /// Bound on waiting for the camera's answer after the start call.
    pub answer_timeout: Duration,
    /// Bound on the UDP handshake.
    pub handshake_timeout: Duration,
    /// Bound on media silence: an armed session that delivers no media
    /// within this of arming - or stalls this long mid-session - is dead,
    /// so the pass behind it falls back to the cloud path instead of
    /// pinning a viewer to a silent source.
    pub media_timeout: Duration,
    /// The media AES mode (unproven in the captures; CTR continued by
    /// default).
    pub aes_mode: media::AesMode,
    /// The `streamSourceQuality` requested of the camera.
    pub quality: signaling::StreamQuality,
    /// The broker host and port; the defaults are the app's own, and the
    /// seam exists so the broker leg can run against a loopback double.
    pub broker_host: String,
    pub broker_port: u16,
    /// Whether the broker connection speaks TLS (the cloud's does; a
    /// loopback double does not).
    pub broker_tls: bool,
    /// Stands in for Build.MANUFACTURER_MODEL in the broker clientId.
    pub device_name: String,
}

impl Default for P2pConfig {
    fn default() -> Self {
        Self {
            protocols: signaling::Protocols::Udp,
            media: signaling::MediaStreams::Both,
            ptt: false,
            stun_server: Some("3.6.42.244:5349".to_owned()),
            switches: ArmSwitches::default(),
            bind: None,
            host_ip: None,
            answer_timeout: Duration::from_secs(15),
            handshake_timeout: Duration::from_secs(10),
            media_timeout: Duration::from_secs(15),
            aes_mode: media::AesMode::CtrCont,
            quality: signaling::StreamQuality::High,
            broker_host: BROKER_HOST.to_owned(),
            broker_port: BROKER_PORT,
            broker_tls: true,
            device_name: "nazar".to_owned(),
        }
    }
}

/// The app-side identity a session borrows: the unit's `mobile` device (the
/// allocation's `userAppID`, the start call's `mobileUUID`), the
/// Android-ID-style `srcDeviceId`, and the camera's controllerUUID for the
/// device-access-token dest.
#[derive(Debug, Clone)]
pub struct P2pIdentity {
    pub self_device: String,
    pub src_device_id: String,
    pub camera_controller: Option<String>,
}

/// The fast-lane engine for one account session.
#[derive(Debug, Clone)]
pub struct P2pEngine {
    client: QuboClient,
    config: P2pConfig,
}

impl P2pEngine {
    pub fn new(client: QuboClient, config: P2pConfig) -> Self {
        Self { client, config }
    }

    /// Arm a fast-lane session for `device` (allocation, start call,
    /// handshake). On any failure the session is torn down on the cloud side
    /// before the error surfaces, so the camera never holds a half-open call.
    pub async fn arm(
        &self,
        session: &Session,
        device: &Device,
        identity: &P2pIdentity,
    ) -> Result<ArmedSession, P2pError> {
        let allocation = self.allocate(session, device, identity).await?;
        if self.config.switches.device_token {
            self.device_token_preflight(session, device, identity).await;
        }
        let (socket, offer) = self.open_media_socket(&allocation).await?;
        let control_topic = format!(
            "/control/{}/{}/p2pStreaming",
            device.unit_id, device.device_id
        );
        let monitor_topic = format!(
            "/monitor/{}/{}/p2pStreaming",
            device.unit_id, device.device_id
        );
        let mut broker = self.connect_broker(session, &device.unit_id, &monitor_topic);

        // From here on the camera may learn of this session, so every exit -
        // an error return, or the caller's bound dropping this future
        // mid-flight - must publish endP2pCall; the guard owns that.
        let session_id = allocation.app_session_id.clone();
        let end = build_end_call(&device.device_id, &identity.src_device_id, &session_id);
        let teardown = TeardownGuard::new(
            &broker,
            &control_topic,
            serde_json::to_vec(&end).map_err(QuboError::from)?,
        );

        self.publish_start(
            &broker,
            &control_topic,
            &allocation,
            device,
            identity,
            &offer,
        )
        .await?;
        let answer = self
            .wait_for_answer(&mut broker.answers, &session_id)
            .await?;
        if let Some(answer) = &answer {
            tracing::debug!(
                event = %answer.event,
                handle = ?answer.handle_name,
                candidates = ?candidate_endpoints(&answer.candidates),
                "camera answered",
            );
        } else {
            // The reference proceeds on the relay leg alone when the answer
            // never lands - the relay endpoint comes from the allocation,
            // not from the answer.
            tracing::info!(
                window = ?self.config.answer_timeout,
                "no p2pCallStarted within the window; proceeding on the relay leg alone",
            );
        }
        let (targets, relay_endpoint) = handshake_plan(answer.as_ref(), &allocation);
        if targets.is_empty() && relay_endpoint.is_none() {
            return Err(P2pError::NoTargets);
        }
        let established = relay::handshake(
            &socket,
            &targets,
            &HandshakeConfig {
                relay: relay_endpoint,
                relay_gate: self.config.switches.relay_gate,
                timeout: self.config.handshake_timeout,
            },
        )
        .await?;
        let decryptor = media::MediaDecryptor::new(
            self.config.aes_mode,
            &allocation.aes_key,
            &allocation.aes_iv,
        )?;
        let socket = Arc::new(socket);
        let beat = match (self.config.switches.hold_beat, relay_endpoint) {
            (true, Some(relay)) => Some(spawn_beat(Arc::clone(&socket), relay)),
            _ => None,
        };
        tracing::debug!(peer = %established.addr, "fast lane armed");

        // The session is real from here: the guard stands down and the
        // armed session owns the broker and its pump.
        teardown.disarm();
        Ok(ArmedSession {
            socket,
            peer: established.addr,
            decryptor,
            names: relay::NameGen::new(),
            broker,
            control_topic,
            session_id,
            camera: device.device_id.clone(),
            src_device_id: identity.src_device_id.clone(),
            disarmed: false,
            beat,
            talk_context: ptt::TalkContext {
                client: self.client.clone(),
                session: session.clone(),
                unit_id: device.unit_id.clone(),
                user_app_id: identity.self_device.clone(),
                quality: self.config.quality,
            },
            talk: None,
            media_timeout: self.config.media_timeout,
            last_media: std::time::Instant::now(),
        })
    }

    /// The v3 allocation with the v2 fallback (the reference's order).
    async fn allocate(
        &self,
        session: &Session,
        device: &Device,
        identity: &P2pIdentity,
    ) -> Result<Allocation, P2pError> {
        let v3 = signaling::allocate_p2p(
            &self.client,
            session,
            &device.unit_id,
            &device.device_id,
            &identity.self_device,
            self.config.ptt,
        )
        .await?;
        let allocation = match v3 {
            Some(allocation) => allocation,
            None => signaling::allocate_credentials(
                &self.client,
                session,
                &device.unit_id,
                &device.device_id,
                &identity.self_device,
            )
            .await?
            .ok_or_else(|| {
                P2pError::Allocation("no allocation from the v3 or v2 route".to_owned())
            })?,
        };
        tracing::debug!(
            session = %allocation.app_session_id,
            relay = ?allocation.server_ip,
            "p2p allocated",
        );
        Ok(allocation)
    }

    /// The device-access-token preflight; failures are logged, not fatal
    /// (the reference continues too).
    async fn device_token_preflight(
        &self,
        session: &Session,
        device: &Device,
        identity: &P2pIdentity,
    ) {
        let Some(dest) = token_dest(identity, device) else {
            tracing::warn!(
                camera = %device.device_id,
                "no controllerUUID for the device-access token; skipping the preflight",
            );
            return;
        };
        match signaling::fetch_device_access_token(
            &self.client,
            session,
            &device.unit_id,
            &identity.self_device,
            dest,
        )
        .await
        {
            Ok(Some(_)) => tracing::debug!("device-access token acquired"),
            Ok(None) => tracing::warn!("device-access token refused; continuing"),
            Err(err) => tracing::warn!(%err, "device-access token fetch failed; continuing"),
        }
    }

    /// The media socket, its srflx lookup, and the candidate list to offer.
    async fn open_media_socket(
        &self,
        allocation: &Allocation,
    ) -> Result<(UdpSocket, String), P2pError> {
        let bind = self
            .config
            .bind
            .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED));
        let socket = UdpSocket::bind(SocketAddr::new(bind, 0)).await?;
        let local_port = socket.local_addr()?.port();
        let host_ip = match &self.config.host_ip {
            Some(ip) => ip.clone(),
            None => match self.config.bind {
                Some(ip) => ip.to_string(),
                None => default_route_ip().to_string(),
            },
        };
        let mut srflx = Vec::new();
        if let Some(server) = &self.config.stun_server
            && let Some(mapped) = relay::discover_srflx(&socket, server).await
        {
            srflx.push(mapped);
        }
        // The relay is advertised as a candidate too - it is how the camera
        // learns where to register (protocol 5.2/6.3).
        if let (Some(ip), Some(port)) = (&allocation.server_ip, allocation.server_port) {
            srflx.push(format!("{ip}:{port}"));
        }
        let offer = build_candidate_list(&host_ip, local_port, &srflx);
        tracing::debug!(offer, "offering candidates");
        Ok((socket, offer))
    }

    /// The engine's own broker connection (the app's SimpleMQTTClient role):
    /// the start call and the camera's answer ride one session-scoped
    /// connection this engine owns and tears down. The subscription is
    /// re-issued on every connect - a clean session's blip inside the
    /// answer window must not lose the answer, which is exactly what the
    /// reference's paho on_connect does.
    fn connect_broker(&self, session: &Session, unit: &str, monitor_topic: &str) -> Broker {
        let options = broker_options(session, unit, &self.config);
        let (client, mut eventloop) = rumqttc::AsyncClient::new(options, BROKER_CAPACITY);
        let (answers_tx, answers) = mpsc::channel::<(String, String)>(8);
        let subscribe_client = client.clone();
        let monitor_topic = monitor_topic.to_owned();
        let pump = tokio::spawn(async move {
            let mut backoff = Duration::ZERO;
            loop {
                match eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Incoming::ConnAck(_))) => {
                        backoff = Duration::ZERO;
                        if let Err(err) = subscribe_client
                            .subscribe(monitor_topic.clone(), QoS::ExactlyOnce)
                            .await
                        {
                            tracing::debug!(%err, "the p2p broker re-subscribe failed");
                        }
                    }
                    Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(message))) => {
                        let topic = message.topic.clone();
                        let payload = String::from_utf8_lossy(&message.payload).into_owned();
                        if answers_tx.send((topic, payload)).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        // rumqttc's poll reconnects on its next call (its own
                        // contract), and the reference's paho does the same -
                        // but poll carries no delay of its own, so a broker
                        // that refuses fast would spin the loop. Back off
                        // between attempts, and the ConnAck above
                        // re-subscribes on the way back in.
                        backoff = next_backoff(backoff);
                        tracing::debug!(
                            %err,
                            backoff = ?backoff,
                            "the p2p broker connection dropped; retrying",
                        );
                        tokio::time::sleep(backoff).await;
                    }
                }
            }
        });
        Broker {
            client,
            pump,
            answers,
        }
    }

    /// Wait for the camera's answer to our session, bounded; `None` on the
    /// bound running out (the relay leg alone is still a valid attempt).
    async fn wait_for_answer(
        &self,
        answers: &mut mpsc::Receiver<(String, String)>,
        session_id: &str,
    ) -> Result<Option<Callback>, P2pError> {
        let answer = tokio::time::timeout(self.config.answer_timeout, async {
            loop {
                let (topic, payload) = answers.recv().await.ok_or(P2pError::BrokerClosed)?;
                if let Some(callback) = parse_p2p_callback(&topic, &payload, session_id) {
                    return Ok::<_, P2pError>(callback);
                }
            }
        })
        .await;
        match answer {
            Ok(result) => result.map(Some),
            Err(_elapsed) => Ok(None),
        }
    }

    /// Build and publish the start call for this session.
    async fn publish_start(
        &self,
        broker: &Broker,
        control_topic: &str,
        allocation: &Allocation,
        device: &Device,
        identity: &P2pIdentity,
        offer: &str,
    ) -> Result<(), P2pError> {
        let start = build_start_call(&StartCall {
            camera: &device.device_id,
            self_device: &identity.self_device,
            src_device_id: &identity.src_device_id,
            session_id: &allocation.app_session_id,
            offer_data: offer,
            aes_key: &allocation.aes_key,
            aes_iv: &allocation.aes_iv,
            protocols: self.config.protocols,
            quality: self.config.quality,
            media: self.config.media,
            ptt: self.config.ptt,
        });
        let payload = serde_json::to_vec(&start).map_err(QuboError::from)?;
        broker
            .client
            .publish(control_topic, QoS::ExactlyOnce, false, payload)
            .await
            .map_err(|err| P2pError::Broker(err.to_string()))?;
        tracing::debug!(topic = %control_topic, "startP2pCall published");
        Ok(())
    }
}

/// Hold the SDK's keepalive beat on the relay socket: a simple SYN every
/// ~300ms with a fresh txid every 2s - the cadence the 09:27 capture shows
/// (83 same-flow SYNs over 57s, the SDK's ka timer). Never sent to the
/// camera, matching the captures.
fn spawn_beat(socket: Arc<UdpSocket>, relay: SocketAddr) -> tokio::task::AbortHandle {
    const TICK: Duration = Duration::from_millis(300);
    const REFRESH: Duration = Duration::from_secs(2);
    let handle = tokio::spawn(async move {
        let mut names = relay::NameGen::new();
        let mut syn = relay::simple_syn(&names.generate(), &random_nonce());
        let mut refreshed = std::time::Instant::now();
        let mut ticker = tokio::time::interval(TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            ticker.tick().await;
            if refreshed.elapsed() >= REFRESH {
                syn = relay::simple_syn(&names.generate(), &random_nonce());
                refreshed = std::time::Instant::now();
            }
            let _ = socket.send_to(&syn, relay).await;
        }
    });
    handle.abort_handle()
}

/// The handshake's targets: the camera's candidates, and the relay when the
/// allocation named one.
fn handshake_targets(
    answer: &Callback,
    allocation: &Allocation,
) -> (Vec<SocketAddr>, Option<SocketAddr>) {
    let targets: Vec<SocketAddr> = candidate_endpoints(&answer.candidates)
        .iter()
        .filter_map(|(ip, port)| relay::endpoint_of(ip, *port))
        .collect();
    (targets, relay_endpoint(allocation))
}

/// The handshake plan for one arm: the camera's candidates and the relay
/// when the answer arrived; on a missing answer, the relay leg alone - the
/// endpoint comes from the allocation, never from the answer.
fn handshake_plan(
    answer: Option<&Callback>,
    allocation: &Allocation,
) -> (Vec<SocketAddr>, Option<SocketAddr>) {
    match answer {
        Some(answer) => handshake_targets(answer, allocation),
        None => (Vec::new(), relay_endpoint(allocation)),
    }
}

/// The relay endpoint from the allocation, when it names one.
fn relay_endpoint(allocation: &Allocation) -> Option<SocketAddr> {
    match (&allocation.server_ip, allocation.server_port) {
        (Some(ip), Some(port)) => relay::endpoint_of(ip, port),
        _ => None,
    }
}

/// The device-access-token dest: the controllerUUID - from config first,
/// then the device listing - never the deviceUUID (protocol 2.2: the
/// capture's tokens all carry the target's controller as `dest`).
fn token_dest<'a>(identity: &'a P2pIdentity, device: &'a Device) -> Option<&'a str> {
    identity
        .camera_controller
        .as_deref()
        .or(device.controller_id.as_deref())
}

/// The engine's session-scoped broker connection and its answer feed.
struct Broker {
    client: rumqttc::AsyncClient,
    pump: tokio::task::JoinHandle<()>,
    answers: mpsc::Receiver<(String, String)>,
}

/// Cancellation safety for [`P2pEngine::arm`]: explicit error returns are
/// not the only way out - a caller's bound (or any select) drops the arm
/// future mid-flight, and the camera must not be left holding a half-open
/// call. Every exit from the guarded section therefore publishes
/// `endP2pCall`, and Drop covers the exits that error paths cannot see.
/// The publish is async, so the drop spawns it on the current runtime and
/// stops the pump only after a beat for it to reach the wire.
struct TeardownGuard {
    client: rumqttc::AsyncClient,
    pump: tokio::task::AbortHandle,
    control_topic: String,
    payload: Vec<u8>,
    armed: bool,
}

impl TeardownGuard {
    fn new(broker: &Broker, control_topic: &str, payload: Vec<u8>) -> Self {
        Self {
            client: broker.client.clone(),
            pump: broker.pump.abort_handle(),
            control_topic: control_topic.to_owned(),
            payload,
            armed: true,
        }
    }

    /// The session is being handed off alive: the guard stands down.
    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for TeardownGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        spawn_teardown(
            &self.client,
            &std::mem::take(&mut self.control_topic),
            std::mem::take(&mut self.payload),
            self.pump.clone(),
        );
    }
}

/// Publish one `endP2pCall` and then stop the pump - the teardown every
/// dropped piece of a session runs (the arm guard, or an armed session let
/// go without `disarm`). The publish is async, so it rides the current
/// runtime; with no runtime to flush on (shutdown), stopping the pump is
/// all that is left.
fn spawn_teardown(
    client: &rumqttc::AsyncClient,
    control_topic: &str,
    payload: Vec<u8>,
    pump: tokio::task::AbortHandle,
) {
    let client = client.clone();
    let control_topic = control_topic.to_owned();
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        pump.abort();
        return;
    };
    runtime.spawn(async move {
        let _ = client
            .publish(&control_topic, QoS::ExactlyOnce, false, payload)
            .await;
        // The reference's own beat between publishing the teardown and
        // closing the connection.
        tokio::time::sleep(Duration::from_millis(500)).await;
        pump.abort();
    });
}

fn broker_options(session: &Session, unit: &str, config: &P2pConfig) -> rumqttc::MqttOptions {
    let mut options = rumqttc::MqttOptions::new(
        signaling::client_id(&config.device_name, unit, "SimpleMQTTClient"),
        &config.broker_host,
        config.broker_port,
    );
    options.set_credentials(session.uuid.clone(), session.access_token.clone());
    options.set_keep_alive(BROKER_KEEP_ALIVE);
    options.set_transport(if config.broker_tls {
        // The platform's own TLS, as the server's feed uses (rumqttc's
        // rustls path pins a webpki line with unpatched advisories).
        rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Native)
    } else {
        rumqttc::Transport::Tcp
    });
    options
}

/// The default-route address: what the OS would source toward the internet.
/// The route table comes first, because a split-tunnel VPN (0/1 + 128/1
/// routes) leaves the default route on the physical interface while a
/// connect probe would source from the tunnel - the exact failure the
/// reference client's first live runs hit (`docs/qubo-findings.md`).
fn default_route_ip() -> IpAddr {
    preferred_ip(route_table_ip(), udp_connect_ip)
}

/// The table wins whenever it answers: a split-tunnel VPN leaves the
/// default route on the physical interface while the connect probe would
/// source from the tunnel (`docs/qubo-findings.md`), so the probe only
/// stands in when the table does not answer.
fn preferred_ip(table: Option<IpAddr>, probe: impl FnOnce() -> IpAddr) -> IpAddr {
    table.unwrap_or_else(probe)
}

/// The default route's interface address, via the platform's own tools.
#[cfg(target_os = "macos")]
fn route_table_ip() -> Option<IpAddr> {
    let route = command_output("route", &["-n", "get", "default"])?;
    let interface = parse_default_interface_macos(&route)?;
    command_output("ipconfig", &["getifaddr", &interface])?
        .trim()
        .parse()
        .ok()
}

/// The default route's interface address, via the platform's own tools.
#[cfg(target_os = "linux")]
fn route_table_ip() -> Option<IpAddr> {
    let route = command_output("ip", &["-4", "route", "show", "default"])?;
    let interface = parse_default_device_linux(&route)?;
    let address = command_output("ip", &["-4", "addr", "show", "dev", &interface])?;
    parse_inet_linux(&address)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn route_table_ip() -> Option<IpAddr> {
    None
}

fn command_output(program: &str, args: &[&str]) -> Option<String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
}

/// macOS `route -n get default`: the `interface: en0` line.
#[cfg(target_os = "macos")]
fn parse_default_interface_macos(text: &str) -> Option<String> {
    text.lines()
        .find_map(|line| line.trim().strip_prefix("interface:"))
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
}

/// Linux `ip route show default`: the device after `dev`.
#[cfg(target_os = "linux")]
fn parse_default_device_linux(text: &str) -> Option<String> {
    let mut words = text.split_whitespace();
    while let Some(word) = words.next() {
        if word == "dev" {
            return words
                .next()
                .filter(|name| !name.is_empty())
                .map(str::to_owned);
        }
    }
    None
}

/// Linux `ip -4 addr show dev <iface>`: the first `inet` line's address.
#[cfg(target_os = "linux")]
fn parse_inet_linux(text: &str) -> Option<IpAddr> {
    for line in text.lines() {
        let Some(rest) = line.trim().strip_prefix("inet ") else {
            continue;
        };
        let address = rest.split_whitespace().next()?.split('/').next()?;
        return address.parse().ok();
    }
    None
}

/// The UDP-connect probe: what the OS would source toward the internet. A
/// fallback, not the first choice: a full-tunnel VPN makes it return the
/// tunnel address, and the probe sends nothing.
fn udp_connect_ip() -> IpAddr {
    let Ok(socket) = std::net::UdpSocket::bind("0.0.0.0:0") else {
        return IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    };
    if socket.connect("8.8.8.8:80").is_err() {
        return IpAddr::V4(Ipv4Addr::UNSPECIFIED);
    }
    socket
        .local_addr()
        .map_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED), |addr| addr.ip())
}

/// A live fast-lane session: media flows from [`ArmedSession::recv_media`],
/// controls ride [`ArmedSession::send_control`], and [`ArmedSession::disarm`]
/// (or dropping) closes the broker side.
pub struct ArmedSession {
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    decryptor: media::MediaDecryptor,
    names: relay::NameGen,
    broker: Broker,
    control_topic: String,
    session_id: String,
    camera: String,
    src_device_id: String,
    /// Whether `disarm` ended the call; a drop that finds this false ends
    /// it, so abandoning a session never leaves the camera in a call.
    disarmed: bool,
    /// The relay keepalive beat, when `P2pConfig::switches.hold_beat`
    /// asked for one.
    beat: Option<tokio::task::AbortHandle>,
    /// What a talk burst needs from the arm.
    talk_context: ptt::TalkContext,
    /// The running talk burst, one per session.
    talk: Option<ptt::Talk>,
    /// The media-silence bound, and when media last arrived (arming counts
    /// as arrival time zero).
    media_timeout: Duration,
    last_media: std::time::Instant,
}

impl ArmedSession {
    /// The session's cloud id - the correlation key the camera echoes.
    pub fn session_id(&self) -> &str {
        &self.session_id
    }

    /// The peer that finished the handshake (the camera, or the relay).
    pub fn peer(&self) -> SocketAddr {
        self.peer
    }

    /// The media socket's local address.
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// The next decrypted media packet. `Ok(None)` means the socket was idle
    /// for the read window - a liveness tick, not an end - unless no media
    /// has arrived within `media_timeout` of arming or the last packet, in
    /// which case the session is silent: `Err(MediaSilence)`, which is what
    /// lets the caller fall back instead of re-arming into the same silence.
    ///
    /// The bound is checked on every tick - an idle read window, a control
    /// datagram and a rejected one alike - so a peer that keeps talking
    /// without ever sending media cannot defer it.
    ///
    /// The accounting is the reference's receive loop exactly: control
    /// packets (small and cookie-tagged) are answered or skipped without
    /// touching the keystream, and every other datagram whose header-length
    /// arithmetic fits decrypts and is yielded - so under a continuing
    /// keystream no packet the reference processed is ever skipped here.
    pub async fn recv_media(&mut self) -> Result<Option<MediaPacket>, P2pError> {
        let mut buf = vec![0_u8; 4096];
        loop {
            self.media_silence()?;
            let received = tokio::time::timeout(MEDIA_IDLE, self.socket.recv_from(&mut buf)).await;
            let (len, from) = match received {
                Err(_elapsed) => {
                    // The wait itself is a tick; it may have crossed the
                    // bound.
                    self.media_silence()?;
                    return Ok(None);
                }
                Ok(Err(err)) => return Err(P2pError::Io(err)),
                Ok(Ok(pair)) => pair,
            };
            let data = &buf[..len];
            if relay::is_control(data) {
                if let Packet::Probe { txid } = relay::classify(data) {
                    // A late probe mid-media: keep answering so the peer
                    // stays armed, as the reference's receive loop does.
                    if let Some(ack) = peer_ack_of(txid) {
                        let _ = self.socket.send_to(&ack, from).await;
                    }
                }
                // The live windows' transcripts want every control datagram
                // the peer sends mid-session, whatever it turns out to be.
                tracing::debug!(
                    len = data.len(),
                    from = %from,
                    head = %hex_head(data),
                    "control datagram",
                );
                continue;
            }
            let header_len = media::rtp_header_len(data);
            if header_len == 0 || header_len >= data.len() {
                continue;
            }
            let mut packet = data.to_vec();
            self.decryptor.decrypt(&mut packet[header_len..]);
            let header = media::parse_header(&packet, header_len);
            self.last_media = std::time::Instant::now();
            return Ok(Some(MediaPacket {
                packet,
                header_len,
                header,
            }));
        }
    }

    /// The silence bound as of right now: `Err(MediaSilence)` once no media
    /// has arrived within it.
    fn media_silence(&self) -> Result<(), P2pError> {
        if self.last_media.elapsed() >= self.media_timeout {
            return Err(P2pError::MediaSilence(self.media_timeout));
        }
        Ok(())
    }

    /// Send one control envelope as a `2001` data-channel frame - the
    /// low-latency twin of the MQTT `/control` path (protocol 6.7).
    pub async fn send_control(&mut self, envelope: &Value) -> Result<(), P2pError> {
        self.send_envelope(envelope).await
    }

    /// One data-channel frame carrying `envelope`.
    async fn send_envelope(&mut self, envelope: &Value) -> Result<(), P2pError> {
        let json = serde_json::to_vec(envelope).map_err(QuboError::from)?;
        let frame = relay::control_frame(&self.names.generate(), &json, &random_nonce())?;
        self.socket.send_to(&frame, self.peer).await?;
        Ok(())
    }

    /// Begin a talk burst: the PTT update rides the data channel (the
    /// `setStreamState` the native `updateP2PStream` ADD_PTT sends) and the
    /// cloud heartbeat starts. One burst per session; a second call is a
    /// no-op.
    pub async fn start_talk(&mut self) -> Result<(), P2pError> {
        if self.talk.is_some() {
            return Ok(());
        }
        let envelope = ptt::set_stream_state(
            &self.camera,
            &self.session_id,
            true,
            self.talk_context.quality,
        );
        self.send_envelope(&envelope).await?;
        let path = self
            .talk_context
            .heartbeat_path(&self.camera, &self.session_id);
        self.talk = Some(ptt::Talk::start(
            &self.talk_context,
            path,
            ptt::PTT_HEARTBEAT,
        ));
        Ok(())
    }

    /// Push one burst of mic audio over the session's audio channel; `Err`
    /// when no talk is running. The frame is the PT 97 RTP shape
    /// ([UNCERTAIN] the uplink encoding - the live wiring test against the
    /// camera's response settles it).
    pub async fn push_audio(&mut self, samples: &[u8]) -> Result<(), P2pError> {
        let Some(talk) = &mut self.talk else {
            return Err(P2pError::NotTalking);
        };
        let frame = talk.next_frame(samples);
        self.socket.send_to(&frame, self.peer).await?;
        Ok(())
    }

    /// End the burst: the PTT-off update rides the data channel and the
    /// heartbeat stops. A stop without a running burst is a no-op.
    pub async fn stop_talk(&mut self) -> Result<(), P2pError> {
        let Some(talk) = self.talk.take() else {
            return Ok(());
        };
        talk.stop();
        let envelope = ptt::set_stream_state(
            &self.camera,
            &self.session_id,
            false,
            self.talk_context.quality,
        );
        self.send_envelope(&envelope).await
    }

    /// Tear the call down: `endP2pCall` on the control topic, a moment for
    /// the publish to flush, then close.
    pub async fn disarm(mut self) {
        let end = build_end_call(&self.camera, &self.src_device_id, &self.session_id);
        if let Ok(payload) = serde_json::to_vec(&end) {
            let _ = self
                .broker
                .client
                .publish(&self.control_topic, QoS::ExactlyOnce, false, payload)
                .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.disarmed = true;
        if let Some(beat) = self.beat.take() {
            beat.abort();
        }
        if let Some(talk) = self.talk.take() {
            talk.stop();
        }
        self.broker.pump.abort();
    }
}

impl Drop for ArmedSession {
    fn drop(&mut self) {
        if let Some(beat) = self.beat.take() {
            beat.abort();
        }
        if let Some(talk) = self.talk.take() {
            talk.stop();
        }
        if self.disarmed {
            self.broker.pump.abort();
            return;
        }
        // Dropped without disarm: the call still ends, or the camera holds
        // it until it expires.
        let end = build_end_call(&self.camera, &self.src_device_id, &self.session_id);
        match serde_json::to_vec(&end) {
            Ok(payload) => spawn_teardown(
                &self.broker.client,
                &self.control_topic,
                payload,
                self.broker.pump.abort_handle(),
            ),
            Err(_) => self.broker.pump.abort(),
        }
    }
}

fn peer_ack_of(txid: &[u8]) -> Option<[u8; 44]> {
    relay::peer_ack(txid, &random_nonce())
}

/// The first bytes of a datagram, hex, for the debug logs the live windows
/// read.
fn hex_head(data: &[u8]) -> String {
    use std::fmt::Write as _;
    data.iter().take(32).fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

fn random_nonce() -> [u8; 20] {
    let mut nonce = [0_u8; 20];
    rand::rng().fill(&mut nonce);
    nonce
}

/// One decrypted media packet: the RTP bytes with the payload decrypted in
/// place, plus the parsed header fields.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaPacket {
    packet: Vec<u8>,
    header_len: usize,
    header: media::RtpHeader,
}

impl MediaPacket {
    pub fn header(&self) -> &media::RtpHeader {
        &self.header
    }

    /// The decrypted payload.
    pub fn payload(&self) -> &[u8] {
        &self.packet[self.header_len..]
    }

    /// The full RTP packet for a consumer of the media pipeline (go2rtc's
    /// `rtp:` source): the original header, the decrypted payload.
    pub fn to_rtp_bytes(&self) -> &[u8] {
        &self.packet
    }
}

#[cfg(test)]
mod tests {
    //! The arm cancellation guard, which no integration test can reach: it
    //! only fires when the caller drops the arm future mid-flight.

    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use super::*;

    /// A broker that never connects: the pump stays pending, so only the
    /// guard's abort can end it.
    fn idle_broker() -> Broker {
        let (client, _eventloop) = rumqttc::AsyncClient::new(
            rumqttc::MqttOptions::new("guard-test", "192.0.2.1", 1883),
            BROKER_CAPACITY,
        );
        Broker {
            client,
            pump: tokio::spawn(std::future::pending()),
            answers: mpsc::channel(1).1,
        }
    }

    #[tokio::test]
    async fn a_dropped_arm_guard_stops_the_pump() {
        let broker = idle_broker();
        let guard = TeardownGuard::new(&broker, "/control/test/p2pStreaming", b"{}".to_vec());
        drop(guard);
        // The guard publishes the teardown, gives it a beat, then stops the
        // pump; a pump still running after the window means the guard never
        // fired.
        let stopped = tokio::time::timeout(Duration::from_secs(2), broker.pump).await;
        assert!(stopped.is_ok(), "a dropped guard stops the pump");
    }

    #[tokio::test]
    async fn a_disarmed_guard_leaves_the_session_alone() {
        let broker = idle_broker();
        TeardownGuard::new(&broker, "/control/test/p2pStreaming", b"{}".to_vec()).disarm();
        // Well past the guard's teardown delay: the pump must still run.
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert!(
            !broker.pump.is_finished(),
            "a disarmed guard never stops the pump"
        );
        broker.pump.abort();
    }

    /// A guard that records when the pump task it rides in is dropped or
    /// aborted - the only way to observe a task's halting from outside.
    struct Halted(Arc<AtomicBool>);

    impl Drop for Halted {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A broker whose pump is observably halted when it ends.
    fn stoppable_broker() -> (Broker, Arc<AtomicBool>) {
        let (client, _eventloop) = rumqttc::AsyncClient::new(
            rumqttc::MqttOptions::new("stop-test", "192.0.2.1", 1883),
            BROKER_CAPACITY,
        );
        let halted = Arc::new(AtomicBool::new(false));
        let guard = Halted(Arc::clone(&halted));
        let pump = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        (
            Broker {
                client,
                pump,
                answers: mpsc::channel(1).1,
            },
            halted,
        )
    }

    #[tokio::test]
    async fn a_dropped_armed_session_stops_its_pump() {
        // Dropped without disarm: the teardown publishes endP2pCall, waits
        // its beat, then stops the pump.
        let (broker, halted) = stoppable_broker();
        let session = ArmedSession {
            socket: Arc::new(
                UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                    .await
                    .expect("a socket"),
            ),
            peer: SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 1),
            decryptor: MediaDecryptor::new(
                media::AesMode::Plain,
                "00000000000000000000000000000000",
                "11111111111111111111111111111111",
            )
            .expect("a plain decryptor"),
            names: relay::NameGen::new(),
            broker,
            control_topic: "/control/unit/cam/p2pStreaming".to_owned(),
            session_id: "session".to_owned(),
            camera: "cam".to_owned(),
            src_device_id: "src".to_owned(),
            disarmed: false,
            beat: None,
            talk_context: ptt::TalkContext {
                client: idle_client(),
                session: Session {
                    access_token: "token".to_owned(),
                    uuid: "user".to_owned(),
                    email: None,
                    refresh_token: None,
                },
                unit_id: "unit".to_owned(),
                user_app_id: "self".to_owned(),
                quality: signaling::StreamQuality::High,
            },
            talk: None,
            media_timeout: Duration::from_secs(60),
            last_media: std::time::Instant::now(),
        };
        drop(session);
        // The teardown publishes first and stops the pump only after its
        // beat; a bare abort would stop it immediately.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            !halted.load(Ordering::SeqCst),
            "the pump outlives the teardown's publish beat"
        );
        let stopped = tokio::time::timeout(Duration::from_secs(2), async {
            while !halted.load(Ordering::SeqCst) {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        assert!(
            stopped.is_ok(),
            "a dropped session's teardown stops the pump"
        );
    }

    /// A client that answers nothing: the paths under test make no call it
    /// would have to fake.
    fn idle_client() -> QuboClient {
        QuboClient::new(
            "http://127.0.0.1:9".parse().expect("a url"),
            crate::client::ServiceProvider::default(),
        )
    }

    /// A session reading a fresh loopback socket, its target address, and a
    /// sender that can feed it datagrams and read what it sends back (the
    /// session's peer is the sender).
    async fn media_session(
        key: &str,
        iv: &str,
        client: QuboClient,
        media_timeout: Duration,
    ) -> (ArmedSession, SocketAddr, UdpSocket) {
        let socket = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a socket");
        let target = socket.local_addr().expect("bound address");
        let sender = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a sender");
        let peer = sender.local_addr().expect("bound address");
        let session = ArmedSession {
            socket: Arc::new(socket),
            peer,
            decryptor: MediaDecryptor::new(media::AesMode::CtrCont, key, iv).expect("a decryptor"),
            names: relay::NameGen::new(),
            broker: idle_broker(),
            control_topic: "/control/unit/cam/p2pStreaming".to_owned(),
            session_id: "session".to_owned(),
            camera: "cam".to_owned(),
            src_device_id: "src".to_owned(),
            disarmed: false,
            beat: None,
            talk_context: ptt::TalkContext {
                client,
                session: Session {
                    access_token: "token".to_owned(),
                    uuid: "user".to_owned(),
                    email: None,
                    refresh_token: None,
                },
                unit_id: "unit".to_owned(),
                user_app_id: "self".to_owned(),
                quality: signaling::StreamQuality::High,
            },
            talk: None,
            media_timeout,
            last_media: std::time::Instant::now(),
        };
        (session, target, sender)
    }

    #[tokio::test]
    async fn the_beat_holds_the_relay_cadence_and_refreshes_its_txid() {
        // The SDK keeps a 52-byte SYN running at ~300ms with a fresh txid
        // every 2s; the Task 2 run's beat variant must reproduce it.
        let relay = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a relay socket");
        let relay_addr = relay.local_addr().expect("bound address");
        let media = Arc::new(
            UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
                .await
                .expect("a media socket"),
        );
        let beat = spawn_beat(Arc::clone(&media), relay_addr);

        let mut buf = [0_u8; 64];
        let mut seen = 0_u32;
        let mut txids = std::collections::BTreeSet::new();
        let deadline = std::time::Instant::now() + Duration::from_millis(2700);
        while std::time::Instant::now() < deadline {
            let Ok(Ok((len, _))) =
                tokio::time::timeout(Duration::from_millis(500), relay.recv_from(&mut buf)).await
            else {
                break;
            };
            seen += 1;
            assert_eq!(len, 52, "the beat is the 52-byte simple SYN");
            assert_eq!(&buf[4..8], &relay::COOKIE, "the beat is cookie-tagged");
            txids.insert(buf[8..20].to_vec());
        }
        beat.abort();
        assert!(
            seen >= 5,
            "the ~300ms cadence ran through the window (saw {seen})"
        );
        assert!(
            txids.len() >= 2,
            "the txid refreshed inside 2.7s (saw {} distinct)",
            txids.len()
        );
    }

    /// A 44-byte cookie-tagged probe, the shape the camera sends mid-media.
    fn media_probe() -> Vec<u8> {
        let mut probe = vec![0x00, 0x01, 0x00, 0x18];
        probe.extend_from_slice(&relay::COOKIE);
        probe.extend_from_slice(b"T4inG06T1005");
        probe.extend_from_slice(&[0x00, 0x08, 0x00, 0x14]);
        probe.extend_from_slice(&[0xAB; 20]);
        probe
    }

    fn fixture_bytes(value: &serde_json::Value) -> Vec<u8> {
        let text = value.as_str().expect("hex text");
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).expect("a hex byte"))
            .collect()
    }

    #[tokio::test]
    async fn recv_media_keeps_the_reference_accounting() {
        // Which datagrams advance the ctr-cont keystream is the reference's
        // receive loop's business: controls never do, anything else that
        // passes the header-length gate does. The captured pair's
        // plaintexts pin both halves.
        let v: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/p2p_byte_vectors.json"))
                .expect("the vector fixture is JSON");
        let key = v["aes"]["key"].as_str().expect("a key");
        let iv = v["aes"]["iv"].as_str().expect("an iv");
        let p1 = fixture_bytes(&v["rtp_frames"][0]["bytes"]);
        let p2 = fixture_bytes(&v["rtp_frames"][1]["bytes"]);
        let cont1 = fixture_bytes(&v["rtp_frames"][0]["plaintext_ctr_cont"]);
        let cont2 = fixture_bytes(&v["rtp_frames"][1]["plaintext_ctr_cont"]);

        // A control probe between the two captured packets must not
        // advance the keystream.
        let (mut session, target, sender) =
            media_session(key, iv, idle_client(), Duration::from_secs(60)).await;
        sender.send_to(&p1, target).await.expect("send p1");
        sender
            .send_to(&media_probe(), target)
            .await
            .expect("send probe");
        sender.send_to(&p2, target).await.expect("send p2");
        let first = session.recv_media().await.expect("read").expect("a packet");
        assert_eq!(
            first.payload(),
            cont1,
            "the first captured packet decrypts as the fixture says"
        );
        let second = session.recv_media().await.expect("read").expect("a packet");
        assert_eq!(
            second.payload(),
            cont2,
            "a control probe between packets does not advance the keystream"
        );

        // A non-control datagram with a valid header length does advance
        // it, exactly as the reference's loop would.
        let (mut session, target, sender) =
            media_session(key, iv, idle_client(), Duration::from_secs(60)).await;
        let garbage = vec![0x40_u8; 20];
        sender.send_to(&p1, target).await.expect("send p1");
        sender
            .send_to(&garbage, target)
            .await
            .expect("send garbage");
        sender.send_to(&p2, target).await.expect("send p2");
        let first = session.recv_media().await.expect("read").expect("a packet");
        assert_eq!(first.payload(), cont1, "a fresh keystream starts at p1");
        let junk = session.recv_media().await.expect("read").expect("a packet");
        assert_eq!(
            junk.payload().len(),
            8,
            "the garbage datagram's payload was decrypt-streamed"
        );
        let second = session.recv_media().await.expect("read").expect("a packet");
        assert_ne!(
            second.payload(),
            cont2,
            "a datagram the reference processed advances the keystream here too"
        );
    }

    /// One datagram off the sender's socket, with a bound.
    async fn next_frame(sender: &UdpSocket, buf: &mut [u8; 4096]) -> Vec<u8> {
        let (len, _) = tokio::time::timeout(Duration::from_secs(1), sender.recv_from(buf))
            .await
            .expect("a frame within the bound")
            .expect("a datagram");
        buf[..len].to_vec()
    }

    /// The JSON inside a `2001` frame.
    fn frame_envelope(frame: &[u8]) -> serde_json::Value {
        assert_eq!(
            frame[0..2],
            [0x20, 0x01],
            "the frame is a 2001 control frame"
        );
        let len = usize::from(u16::from_be_bytes([frame[22], frame[23]]));
        serde_json::from_slice(&frame[24..24 + len]).expect("the frame carries JSON")
    }

    #[tokio::test]
    async fn the_talk_sequence_rides_the_data_channel() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::header("User-UUID", "user"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;
        let client = QuboClient::new(
            server.uri().parse().expect("a url"),
            crate::client::ServiceProvider::default(),
        );

        let (mut session, _target, sender) = media_session(
            "82458EA37F4740361F47B4E68946546F",
            "A3B6C1B75EEB65ACF59802CE5A6995B3",
            client,
            Duration::from_secs(60),
        )
        .await;
        let mut buf = [0_u8; 4096];

        session.start_talk().await.expect("the talk starts");
        let update = frame_envelope(&next_frame(&sender, &mut buf).await);
        let set_state = "/command/devices/services/p2pStreaming/commands/setStreamState/parameters";
        assert_eq!(
            update.pointer(&format!("{set_state}/pttEnabled")),
            Some(&serde_json::json!(true)),
            "the start rides as pttEnabled true"
        );
        assert_eq!(
            update.pointer(&format!("{set_state}/sessionId")),
            Some(&serde_json::json!("session")),
            "the update names the armed session"
        );

        // The heartbeat's first beat goes out with the start: the exact
        // path the decompile pins, authenticated, on the port's GET
        // reading of the verb (ptt.rs's [UNCERTAIN httpMethod]).
        let beat = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let beats = server.received_requests().await.expect("recorded requests");
                if beats.iter().any(|request| {
                    request.url.path()
                        == "/stream-manager/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/unit/unit/device/cam/ptt/heartbeat"
                }) {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            beat.is_ok(),
            "the first heartbeat goes out with the start, on the decompiled path"
        );

        // The mic rides the session's audio channel as PT 97.
        session
            .push_audio(&[0xAA; 320])
            .await
            .expect("the audio rides");
        let audio = next_frame(&sender, &mut buf).await;
        assert_eq!(audio[1], 97, "the audio frame is PT 97");
        assert_eq!(audio.len(), 12 + 320, "the samples follow the header");

        session.stop_talk().await.expect("the stop lands");
        let off = frame_envelope(&next_frame(&sender, &mut buf).await);
        assert_eq!(
            off.pointer(&format!("{set_state}/pttEnabled")),
            Some(&serde_json::json!(false)),
            "the stop rides as pttEnabled false"
        );
        assert!(
            matches!(session.push_audio(&[0xBB]).await, Err(P2pError::NotTalking)),
            "audio after the stop is refused, never swallowed"
        );
    }

    #[tokio::test]
    async fn a_silent_session_ends_at_the_media_bound() {
        // Armed but never delivers media: the session reports silence, so
        // a caller can fall back instead of ticking on a dead source.
        let (mut session, _target, _sender) = media_session(
            "82458EA37F4740361F47B4E68946546F",
            "A3B6C1B75EEB65ACF59802CE5A6995B3",
            idle_client(),
            Duration::from_millis(200),
        )
        .await;
        let outcome = session.recv_media().await;
        assert!(
            matches!(outcome, Err(P2pError::MediaSilence(_))),
            "a session that never delivered media reports silence, got {outcome:?}",
        );
    }

    #[tokio::test]
    async fn arriving_media_resets_the_silence_clock() {
        let (mut session, target, sender) = media_session(
            "82458EA37F4740361F47B4E68946546F",
            "A3B6C1B75EEB65ACF59802CE5A6995B3",
            idle_client(),
            Duration::from_millis(600),
        )
        .await;
        let feeder = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            let _ = sender
                .send_to(
                    &[0x80, 0x60, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0xAA, 0xBB],
                    target,
                )
                .await;
            sender
        });

        let first = session
            .recv_media()
            .await
            .expect("arrives inside the bound");
        assert!(first.is_some(), "a packet inside the bound is not silence");
        let idle = session.recv_media().await.expect("still inside the bound");
        assert!(
            idle.is_none(),
            "an idle tick inside the bound is not silence"
        );
        let outcome = session.recv_media().await;
        assert!(
            matches!(outcome, Err(P2pError::MediaSilence(_))),
            "past the bound the session is silent again, got {outcome:?}",
        );
        drop(feeder);
    }

    #[tokio::test]
    async fn control_traffic_cannot_defer_the_silence_bound() {
        // A peer that keeps answering with control datagrams (and junk)
        // without ever sending media: the bound is evaluated on those ticks
        // too, so the session still reports silence rather than ticking
        // forever.
        let (mut session, target, sender) = media_session(
            "82458EA37F4740361F47B4E68946546F",
            "A3B6C1B75EEB65ACF59802CE5A6995B3",
            idle_client(),
            Duration::from_millis(300),
        )
        .await;
        let chatter = tokio::spawn(async move {
            let mut control = vec![0x20, 0x03, 0x00, 0x00];
            control.extend_from_slice(&relay::COOKIE);
            let junk = [0x00_u8; 4];
            loop {
                let _ = sender.send_to(&control, target).await;
                let _ = sender.send_to(&junk, target).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let outcome = tokio::time::timeout(Duration::from_secs(2), session.recv_media())
            .await
            .expect("the bound fires on control ticks, not only on idle windows");
        assert!(
            matches!(outcome, Err(P2pError::MediaSilence(_))),
            "control traffic with no media is still silence, got {outcome:?}",
        );
        chatter.abort();
    }

    #[test]
    fn the_route_table_wins_over_the_connect_probe() {
        // The split-tunnel VPN case: the table says the LAN address, the
        // probe would say the tunnel's - the table's is what the camera
        // must be offered.
        let table: IpAddr = "192.0.2.140".parse().expect("an ip");
        let tunnel: IpAddr = "198.51.100.15".parse().expect("an ip");
        assert_eq!(
            preferred_ip(Some(table), || tunnel),
            table,
            "the table's address wins whenever it answers"
        );
        assert_eq!(
            preferred_ip(None, || tunnel),
            tunnel,
            "without a table answer the probe stands in"
        );
    }

    #[test]
    fn a_missing_answer_still_plans_the_relay_leg() {
        let allocation = Allocation {
            app_session_id: "session".to_owned(),
            aes_key: "key".to_owned(),
            aes_iv: "iv".to_owned(),
            server_ip: Some("13.204.183.153".to_owned()),
            server_port: Some(56329),
        };
        let relay: SocketAddr = "13.204.183.153:56329".parse().expect("an addr");

        let (targets, plan) = handshake_plan(None, &allocation);
        assert!(targets.is_empty(), "no answer means no camera candidates");
        assert_eq!(
            plan,
            Some(relay),
            "the relay leg comes from the allocation even with no answer"
        );

        let bare = Allocation {
            server_ip: None,
            server_port: None,
            ..allocation.clone()
        };
        let (targets, plan) = handshake_plan(None, &bare);
        assert!(
            targets.is_empty() && plan.is_none(),
            "an answer-less arm with no relay endpoint has no plan at all"
        );

        let answer = Callback {
            event: "p2pCallStarted".to_owned(),
            session_id: "session".to_owned(),
            handle_name: None,
            candidates: vec![Candidate {
                host: Some("192.0.2.21:56839".to_owned()),
                srflx: Vec::new(),
            }],
        };
        let (targets, plan) = handshake_plan(Some(&answer), &allocation);
        assert_eq!(
            targets,
            vec!["192.0.2.21:56839".parse().expect("an addr")],
            "an answered arm plans the camera's candidates"
        );
        assert_eq!(plan, Some(relay), "and keeps the relay leg");
    }

    #[test]
    fn the_reconnect_backoff_doubles_and_caps() {
        assert_eq!(
            next_backoff(Duration::ZERO),
            Duration::from_secs(1),
            "the first retry waits a second, the paho minimum"
        );
        assert_eq!(
            next_backoff(Duration::from_secs(1)),
            Duration::from_secs(2),
            "then it doubles"
        );
        assert_eq!(
            next_backoff(Duration::from_secs(30)),
            Duration::from_secs(30),
            "and it caps at the session's scale"
        );
    }

    #[tokio::test]
    async fn a_fast_failing_broker_does_not_spin_the_pump() {
        // A broker that accepts and immediately closes: every poll fails
        // fast, and only the backoff bounds the attempts (measured at ~39k/s
        // unbacked against a refused port).
        let listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("a port");
        let port = listener.local_addr().expect("bound address").port();
        let accepts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&accepts);
        let acceptor = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        counter.fetch_add(1, Ordering::SeqCst);
                        drop(socket);
                    }
                    Err(_) => return,
                }
            }
        });

        let engine = P2pEngine::new(
            QuboClient::new(
                "http://127.0.0.1:9".parse().expect("a url"),
                crate::client::ServiceProvider::default(),
            ),
            P2pConfig {
                broker_host: "127.0.0.1".to_owned(),
                broker_port: port,
                broker_tls: false,
                ..P2pConfig::default()
            },
        );
        let session = Session {
            access_token: "t".to_owned(),
            uuid: "u".to_owned(),
            email: None,
            refresh_token: None,
        };
        let broker = engine.connect_broker(&session, "unit", "/monitor/unit/cam/p2pStreaming");
        tokio::time::sleep(Duration::from_millis(1500)).await;
        broker.pump.abort();
        acceptor.abort();

        let attempts = accepts.load(Ordering::SeqCst);
        assert!(attempts >= 1, "the pump did try to connect");
        assert!(
            attempts <= 3,
            "the backoff bounds the attempts over 1.5s (saw {attempts})"
        );
    }

    #[test]
    fn the_token_dest_is_always_a_controller_uuid() {
        let device = |controller: Option<&str>| Device {
            device_id: "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01".to_owned(),
            name: "Hall".to_owned(),
            device_type: "HCP01".to_owned(),
            model: "HCP01".to_owned(),
            unit_id: "unit".to_owned(),
            entity_id: "default".to_owned(),
            controller_id: controller.map(str::to_owned),
        };
        let identity = |controller: Option<&str>| P2pIdentity {
            self_device: "self".to_owned(),
            src_device_id: "src".to_owned(),
            camera_controller: controller.map(str::to_owned),
        };

        assert_eq!(
            token_dest(
                &identity(Some("config-controller")),
                &device(Some("listed-controller"))
            ),
            Some("config-controller"),
            "the configured controller wins"
        );
        assert_eq!(
            token_dest(&identity(None), &device(Some("listed-controller"))),
            Some("listed-controller"),
            "the listing's controllerUUID is the fallback"
        );
        assert_eq!(
            token_dest(&identity(None), &device(None)),
            None,
            "without a controllerUUID there is no dest - the deviceUUID is never sent"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_macos_route_parser_reads_the_interface_line() {
        let route = "   route to: default\ndestination: default\n       mask: default\n    gateway: 192.0.2.1\n  interface: en0\n      flags: <UP,GATEWAY,DONE,STATIC,PRCLONING,GLOBAL>\n";
        assert_eq!(
            parse_default_interface_macos(route).as_deref(),
            Some("en0"),
            "the interface line is the default route's device"
        );
        assert_eq!(
            parse_default_interface_macos("nothing here"),
            None,
            "a route without an interface line is none"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn the_linux_route_parsers_read_the_device_and_address() {
        assert_eq!(
            parse_default_device_linux("default via 192.0.2.1 dev eth0 proto dhcp metric 100\n")
                .as_deref(),
            Some("eth0"),
            "the device after dev is the default route's interface"
        );
        assert_eq!(
            parse_default_device_linux("no default here"),
            None,
            "a table without a default route is none"
        );
        let show = "2: eth0: <BROADCAST,MULTICAST,UP,LOWER_UP> mtu 1500\n    inet 192.0.2.140/24 brd 192.0.2.255 scope global eth0\n";
        assert_eq!(
            parse_inet_linux(show),
            Some("192.0.2.140".parse().expect("an ip")),
            "the first inet line is the interface's address"
        );
        assert_eq!(
            parse_inet_linux("no addresses"),
            None,
            "a device with no inet line is none"
        );
    }

    #[tokio::test]
    async fn an_unanswered_start_call_yields_no_callback() {
        // The bound running out is not an error: the reference proceeds on
        // the relay leg alone, so arm must too.
        let engine = P2pEngine::new(
            QuboClient::new(
                "http://127.0.0.1:9".parse().expect("a url"),
                crate::client::ServiceProvider::default(),
            ),
            P2pConfig {
                answer_timeout: Duration::from_millis(50),
                ..P2pConfig::default()
            },
        );
        let (_sender, mut answers) = mpsc::channel::<(String, String)>(1);
        let answer = engine
            .wait_for_answer(&mut answers, "session")
            .await
            .expect("a quiet broker is not a channel error");
        assert!(
            answer.is_none(),
            "no answer within the bound yields None, not an error"
        );
    }
}
