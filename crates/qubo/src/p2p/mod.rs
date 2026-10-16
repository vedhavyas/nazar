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
pub mod relay;
pub mod signaling;

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
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
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
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
    /// Fetch the device-access token before the start call, as the app does.
    pub device_token: bool,
    /// The local address the media socket binds to.
    pub bind: Option<IpAddr>,
    /// The advertised host ip; defaults to the bind address, else the
    /// default-route address.
    pub host_ip: Option<String>,
    /// Wait for the relay's first packet before syncing the direct
    /// candidates - the app's invariant across all three captures.
    pub relay_gate: bool,
    /// Bound on waiting for the camera's answer after the start call.
    pub answer_timeout: Duration,
    /// Bound on the UDP handshake.
    pub handshake_timeout: Duration,
    /// The media AES mode (unproven in the captures; CTR continued by
    /// default).
    pub aes_mode: media::AesMode,
    /// The `streamSourceQuality` requested of the camera.
    pub quality: signaling::StreamQuality,
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
            device_token: true,
            bind: None,
            host_ip: None,
            relay_gate: true,
            answer_timeout: Duration::from_secs(15),
            handshake_timeout: Duration::from_secs(10),
            aes_mode: media::AesMode::CtrCont,
            quality: signaling::StreamQuality::High,
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
        if self.config.device_token {
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
        let mut broker = self
            .connect_broker(session, &device.unit_id, &monitor_topic)
            .await?;

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
        let (targets, relay_endpoint) = if let Some(answer) = &answer {
            tracing::debug!(
                event = %answer.event,
                handle = ?answer.handle_name,
                candidates = ?candidate_endpoints(&answer.candidates),
                "camera answered",
            );
            handshake_targets(answer, &allocation)
        } else {
            // The reference proceeds on the relay leg alone when the answer
            // never lands - the relay endpoint comes from the allocation,
            // not from the answer.
            tracing::info!(
                window = ?self.config.answer_timeout,
                "no p2pCallStarted within the window; proceeding on the relay leg alone",
            );
            (Vec::new(), relay_endpoint(&allocation))
        };
        if targets.is_empty() && relay_endpoint.is_none() {
            return Err(P2pError::NoTargets);
        }
        let established = relay::handshake(
            &socket,
            &targets,
            &HandshakeConfig {
                relay: relay_endpoint,
                relay_gate: self.config.relay_gate,
                timeout: self.config.handshake_timeout,
            },
        )
        .await?;
        let decryptor = media::MediaDecryptor::new(
            self.config.aes_mode,
            &allocation.aes_key,
            &allocation.aes_iv,
        )?;
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
    /// connection this engine owns and tears down.
    async fn connect_broker(
        &self,
        session: &Session,
        unit: &str,
        monitor_topic: &str,
    ) -> Result<Broker, P2pError> {
        let options = broker_options(session, unit, &self.config.device_name);
        let (client, mut eventloop) = rumqttc::AsyncClient::new(options, BROKER_CAPACITY);
        let (answers_tx, answers) = mpsc::channel::<(String, String)>(8);
        let pump = tokio::spawn(async move {
            loop {
                match eventloop.poll().await {
                    Ok(rumqttc::Event::Incoming(rumqttc::Incoming::Publish(message))) => {
                        let topic = message.topic.clone();
                        let payload = String::from_utf8_lossy(&message.payload).into_owned();
                        if answers_tx.send((topic, payload)).await.is_err() {
                            return;
                        }
                    }
                    Ok(_) => {}
                    Err(err) => {
                        tracing::debug!(%err, "the p2p broker event loop ended");
                        return;
                    }
                }
            }
        });
        client
            .subscribe(monitor_topic, QoS::ExactlyOnce)
            .await
            .map_err(|err| P2pError::Broker(err.to_string()))?;
        Ok(Broker {
            client,
            pump,
            answers,
        })
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
        let client = self.client.clone();
        let pump = self.pump.clone();
        let control_topic = std::mem::take(&mut self.control_topic);
        let payload = std::mem::take(&mut self.payload);
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            // No runtime to flush the teardown on (shutdown): stopping the
            // pump is all that is left.
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
}

fn broker_options(session: &Session, unit: &str, device_name: &str) -> rumqttc::MqttOptions {
    let mut options = rumqttc::MqttOptions::new(
        signaling::client_id(device_name, unit, "SimpleMQTTClient"),
        BROKER_HOST,
        BROKER_PORT,
    );
    options.set_credentials(session.uuid.clone(), session.access_token.clone());
    options.set_keep_alive(BROKER_KEEP_ALIVE);
    // The platform's own TLS, as the server's feed uses (rumqttc's rustls
    // path pins a webpki line with unpatched advisories).
    options.set_transport(rumqttc::Transport::Tls(rumqttc::TlsConfiguration::Native));
    options
}

/// The default-route address: what the OS would source toward the internet.
/// The route table comes first, because a split-tunnel VPN (0/1 + 128/1
/// routes) leaves the default route on the physical interface while a
/// connect probe would source from the tunnel - the exact failure the
/// reference client's first live runs hit (`docs/qubo-findings.md`).
fn default_route_ip() -> IpAddr {
    route_table_ip().unwrap_or_else(udp_connect_ip)
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
    socket: UdpSocket,
    peer: SocketAddr,
    decryptor: media::MediaDecryptor,
    names: relay::NameGen,
    broker: Broker,
    control_topic: String,
    session_id: String,
    camera: String,
    src_device_id: String,
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
    /// for the read window - a liveness tick, not an end.
    pub async fn recv_media(&mut self) -> Result<Option<MediaPacket>, P2pError> {
        let mut buf = vec![0_u8; 4096];
        loop {
            let received = tokio::time::timeout(MEDIA_IDLE, self.socket.recv_from(&mut buf)).await;
            let (len, from) = match received {
                Err(_elapsed) => return Ok(None),
                Ok(Err(err)) => return Err(P2pError::Io(err)),
                Ok(Ok(pair)) => pair,
            };
            let data = &buf[..len];
            match relay::classify(data) {
                Packet::Rtp => {
                    let Some(header) = media::unwrap_rtp(data) else {
                        continue;
                    };
                    let mut packet = data.to_vec();
                    self.decryptor.decrypt(&mut packet[header.header_len..]);
                    return Ok(Some(MediaPacket {
                        packet,
                        header_len: header.header_len,
                        header,
                    }));
                }
                Packet::Probe { txid } => {
                    // A late probe mid-media: keep answering so the peer
                    // stays armed, as the reference's receive loop does.
                    if let Some(ack) = peer_ack_of(txid) {
                        let _ = self.socket.send_to(&ack, from).await;
                    }
                }
                Packet::Bye | Packet::Echo { .. } | Packet::ExtendedAck { .. } | Packet::Other => {}
            }
        }
    }

    /// Send one control envelope as a `2001` data-channel frame - the
    /// low-latency twin of the MQTT `/control` path (protocol 6.7).
    pub async fn send_control(&mut self, envelope: &Value) -> Result<(), P2pError> {
        let json = serde_json::to_vec(envelope).map_err(QuboError::from)?;
        let frame = relay::control_frame(&self.names.generate(), &json, &random_nonce())?;
        self.socket.send_to(&frame, self.peer).await?;
        Ok(())
    }

    /// Tear the call down: `endP2pCall` on the control topic, a moment for
    /// the publish to flush, then close.
    pub async fn disarm(self) {
        let end = build_end_call(&self.camera, &self.src_device_id, &self.session_id);
        if let Ok(payload) = serde_json::to_vec(&end) {
            let _ = self
                .broker
                .client
                .publish(&self.control_topic, QoS::ExactlyOnce, false, payload)
                .await;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        self.broker.pump.abort();
    }
}

impl Drop for ArmedSession {
    fn drop(&mut self) {
        self.broker.pump.abort();
    }
}

fn peer_ack_of(txid: &[u8]) -> Option<[u8; 44]> {
    relay::peer_ack(txid, &random_nonce())
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

    #[test]
    fn the_token_dest_is_always_a_controller_uuid() {
        let device = |controller: Option<&str>| Device {
            device_id: "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01".to_owned(),
            name: "Hall".to_owned(),
            device_type: "HCP01".to_owned(),
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
