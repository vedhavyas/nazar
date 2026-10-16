//! The relay/direct UDP leg: STUN-framed packets under the Qubo cookie.
//!
//! Wire grammar from `docs/qubo-protocol.md` 6.1-6.7, ported from
//! `reference/p2p_client.py`. Every packet is standard STUN framing with the
//! Qubo cookie `244a2112`: type(2) length(2) cookie(4) txid(12), then
//! attributes (type 2B, length 2B, value, padded to 4).

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use rand::Rng as _;
use tokio::net::UdpSocket;

use crate::p2p::P2pError;

/// The Qubo cookie that replaces the STUN magic in every Qubo packet.
pub const COOKIE: [u8; 4] = [0x24, 0x4A, 0x21, 0x12];

/// A 12-byte transaction id: 8 alphanumerics plus a 4-digit counter, with a
/// fresh random prefix per generation (the SDK's `generateStunTransactionId`;
/// what the earlier captures read as "peer names").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Txid(String);

impl Txid {
    /// A txid from its literal 12-byte text, as the captures recorded it.
    pub fn parse(text: &str) -> Option<Self> {
        (text.len() == 12).then(|| Self(text.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// The SDK's txid generator: a fresh random prefix and a monotonic 4-digit
/// counter, so consecutive calls produce the simple/extended pairs the
/// captures show (1055 then 1056).
pub struct NameGen {
    counter: u32,
}

impl NameGen {
    pub fn new() -> Self {
        Self {
            counter: rand::rng().random_range(1040..1100),
        }
    }

    pub fn generate(&mut self) -> Txid {
        // Session counters live in the 1000s; wrap well before the 4-digit
        // field would grow a fifth byte.
        if self.counter > 9999 {
            self.counter = 1000;
        }
        let counter = self.counter;
        self.counter += 1;
        let prefix: String = rand::rng()
            .sample_iter(&rand::distr::Alphanumeric)
            .take(8)
            .map(char::from)
            .collect();
        Txid(format!("{prefix}{counter:04}"))
    }
}

impl Default for NameGen {
    fn default() -> Self {
        Self::new()
    }
}

/// The 52-byte simple SYN, capture-exact (frame 6293): the txid, attr `7002`
/// (len 4, value 0) and attr `0008` (len 0x14, the 20-byte nonce).
pub fn simple_syn(txid: &Txid, nonce: &[u8; 20]) -> [u8; 52] {
    let mut packet = [0_u8; 52];
    packet[..4].copy_from_slice(&[0x00, 0x01, 0x00, 0x20]);
    packet[4..8].copy_from_slice(&COOKIE);
    packet[8..20].copy_from_slice(txid.as_str().as_bytes());
    packet[20..22].copy_from_slice(&[0x70, 0x02]);
    packet[22..24].copy_from_slice(&[0x00, 0x04]);
    packet[24..28].copy_from_slice(&[0x00; 4]);
    packet[28..30].copy_from_slice(&[0x00, 0x08]);
    packet[30..32].copy_from_slice(&[0x00, 0x14]);
    packet[32..].copy_from_slice(nonce);
    packet
}

/// The 60-byte extended SYN, capture-exact (frame 6301): attrs `0025` and
/// `7002` (both len 4, value 0) then the nonce. This is the packet the camera
/// waits for before it starts RTP.
pub fn extended_syn(txid: &Txid, nonce: &[u8; 20]) -> [u8; 60] {
    let mut packet = [0_u8; 60];
    packet[..4].copy_from_slice(&[0x00, 0x01, 0x00, 0x28]);
    packet[4..8].copy_from_slice(&COOKIE);
    packet[8..20].copy_from_slice(txid.as_str().as_bytes());
    packet[20..22].copy_from_slice(&[0x00, 0x25]);
    packet[22..24].copy_from_slice(&[0x00, 0x04]);
    packet[24..28].copy_from_slice(&[0x00; 4]);
    packet[28..30].copy_from_slice(&[0x70, 0x02]);
    packet[30..32].copy_from_slice(&[0x00, 0x04]);
    packet[32..36].copy_from_slice(&[0x00; 4]);
    packet[36..38].copy_from_slice(&[0x00, 0x08]);
    packet[38..40].copy_from_slice(&[0x00, 0x14]);
    packet[40..].copy_from_slice(nonce);
    packet
}

/// The 24-byte TURN-connect register, capture-exact (frame 893): type `2004`
/// and one empty string attribute `7005`. Relay only, never the camera.
pub fn register(txid: &Txid) -> [u8; 24] {
    let mut packet = [0_u8; 24];
    packet[..4].copy_from_slice(&[0x20, 0x04, 0x00, 0x04]);
    packet[4..8].copy_from_slice(&COOKIE);
    packet[8..20].copy_from_slice(txid.as_str().as_bytes());
    packet[20..24].copy_from_slice(&[0x70, 0x05, 0x00, 0x00]);
    packet
}

/// The 44-byte reply to a peer's probe, capture-exact (frame 6312): `0101
/// 0018` plus the probe's 12-byte txid echoed and a fresh nonce.
pub fn peer_ack(peer_txid: &[u8], nonce: &[u8; 20]) -> Option<[u8; 44]> {
    let txid: [u8; 12] = peer_txid.try_into().ok()?;
    let mut packet = [0_u8; 44];
    packet[..4].copy_from_slice(&[0x01, 0x01, 0x00, 0x18]);
    packet[4..8].copy_from_slice(&COOKIE);
    packet[8..20].copy_from_slice(&txid);
    packet[20..24].copy_from_slice(&[0x00, 0x08, 0x00, 0x14]);
    packet[24..].copy_from_slice(nonce);
    Some(packet)
}

/// A `2001` data-channel frame carrying a control envelope, capture-exact
/// (frame 1886): the txid, attr `7001` whose value is the plaintext JSON
/// (zero-padded to the 4-byte boundary), then the nonce attribute.
pub fn control_frame(txid: &Txid, json: &[u8], nonce: &[u8; 20]) -> Result<Vec<u8>, P2pError> {
    let padding = (4 - json.len() % 4) % 4;
    let length = u16::try_from(json.len() + padding + 28)
        .map_err(|_| P2pError::EnvelopeTooLarge { len: json.len() })?;
    let json_len =
        u16::try_from(json.len()).map_err(|_| P2pError::EnvelopeTooLarge { len: json.len() })?;
    let mut frame = Vec::with_capacity(20 + 4 + json.len() + padding + 24);
    frame.extend_from_slice(&[0x20, 0x01]);
    frame.extend_from_slice(&length.to_be_bytes());
    frame.extend_from_slice(&COOKIE);
    frame.extend_from_slice(txid.as_str().as_bytes());
    frame.extend_from_slice(&[0x70, 0x01]);
    frame.extend_from_slice(&json_len.to_be_bytes());
    frame.extend_from_slice(json);
    frame.extend(std::iter::repeat_n(0_u8, padding));
    frame.extend_from_slice(&[0x00, 0x08, 0x00, 0x14]);
    frame.extend_from_slice(nonce);
    Ok(frame)
}

/// What an incoming datagram is (`docs/qubo-protocol.md` 6.1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet<'a> {
    /// `0001 0018`, 44B: the peer's own query; answered with a peer ack.
    Probe {
        txid: &'a [u8],
    },
    /// `0101 0018`, 44B: the peer echoing our simple SYN.
    Echo {
        txid: &'a [u8],
    },
    /// `0101 0020`, 52B: the ack of our extended SYN - the session is up.
    ExtendedAck {
        txid: &'a [u8],
    },
    /// `2003 0004`: the relay's session teardown.
    Bye,
    /// An RTP datagram (first byte `>> 6 == 2`).
    Rtp,
    Other,
}

/// Classify a datagram. Control packets are small and cookie-tagged; the
/// length gate mirrors the reference's receive loop, which reads any
/// cookie-tagged datagram under 100 bytes as control.
pub fn classify(data: &[u8]) -> Packet<'_> {
    if data.len() >= 8 && data.len() < 100 && data[4..8] == COOKIE {
        return match (data[0], data[1], data.len()) {
            (0x00, 0x01, 44) => Packet::Probe { txid: &data[8..20] },
            (0x01, 0x01, 44) => Packet::Echo { txid: &data[8..20] },
            (0x01, 0x01, 52) => Packet::ExtendedAck { txid: &data[8..20] },
            (0x20, 0x03, _) => Packet::Bye,
            _ => Packet::Other,
        };
    }
    if data.len() >= 12 && data[0] >> 6 == 2 {
        return Packet::Rtp;
    }
    Packet::Other
}

/// How the handshake paces itself and when it gives up.
#[derive(Debug, Clone)]
pub struct HandshakeConfig {
    /// The relay endpoint from the allocation: the register goes there and
    /// the SYN train runs there too.
    pub relay: Option<SocketAddr>,
    /// Wait for the relay's first packet before syncing the direct
    /// candidates - the app's invariant across all three captures.
    pub relay_gate: bool,
    /// Ceiling on the whole exchange.
    pub timeout: Duration,
}

/// The peer that finished the handshake - the camera on the direct leg, or
/// the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Established {
    pub addr: SocketAddr,
}

/// One path's handshake state (per candidate, plus the relay leg's own).
#[derive(Debug)]
struct PathState {
    txid: Option<Txid>,
    syn: Option<[u8; 52]>,
    sent: Option<Instant>,
    tries: u32,
    echo: bool,
    probed: Option<Instant>,
    ext: Option<ExtState>,
}

#[derive(Debug)]
struct ExtState {
    txid: Txid,
    syn: [u8; 60],
    sent: Option<Instant>,
    tries: u32,
}

impl PathState {
    fn new() -> Self {
        Self {
            txid: None,
            syn: None,
            sent: None,
            tries: 0,
            echo: false,
            probed: None,
            ext: None,
        }
    }
}

/// A fresh 20-byte nonce for one packet.
fn nonce() -> [u8; 20] {
    let mut nonce = [0_u8; 20];
    rand::rng().fill(&mut nonce);
    nonce
}

const RETRY_GAP: Duration = Duration::from_secs(1);
const EXT_GAP: Duration = Duration::from_millis(200);
const PROBE_GRACE: Duration = Duration::from_millis(500);
const REGISTER_AT: Duration = Duration::from_millis(2100);
const RELAY_EXT_AT: Duration = Duration::from_secs(3);
const READ_WINDOW: Duration = Duration::from_millis(250);

/// Run the app's handshake, capture-exact: a plain SYN train on the relay
/// (same txid every packet) with the `0x2004` register at +2.1s; the direct
/// (camera) candidates join only once the relay has engaged this socket, one
/// fresh txid per path; the extended SYN follows the echo (or a probe at
/// +0.5s), repeated every 200ms up to four times; the session is up on the
/// `0101/0020` ack of that extended SYN - or on RTP, if the ack was lost.
pub async fn handshake(
    socket: &UdpSocket,
    targets: &[SocketAddr],
    config: &HandshakeConfig,
) -> Result<Established, P2pError> {
    let t0 = Instant::now();
    let mut names = NameGen::new();
    let relay_syn = simple_syn(&names.generate(), &nonce());
    let relay_ext = extended_syn(&names.generate(), &nonce());
    let register = register(&names.generate());
    let mut registered = false;
    let mut relay_engaged = config.relay.is_none() || !config.relay_gate;
    let mut relay_echo = false;
    let mut states: Vec<PathState> = targets.iter().map(|_| PathState::new()).collect();
    let deadline = t0 + config.timeout;
    let mut buf = [0_u8; 4096];

    while Instant::now() < deadline {
        let now = Instant::now();
        if let Some(relay) = config.relay {
            let _ = socket.send_to(&relay_syn, relay).await;
            if relay_echo || now - t0 >= RELAY_EXT_AT {
                let _ = socket.send_to(&relay_ext, relay).await;
            }
        }
        if !registered && now - t0 >= REGISTER_AT {
            if let Some(relay) = config.relay {
                let _ = socket.send_to(&register, relay).await;
            }
            registered = true;
        }
        if relay_engaged {
            drive_direct_paths(socket, targets, &mut states, &mut names, now).await;
        }

        let received = tokio::time::timeout(READ_WINDOW, socket.recv_from(&mut buf)).await;
        let (len, from) = match received {
            Err(_elapsed) => continue,
            Ok(Err(err)) => return Err(P2pError::Io(err)),
            Ok(Ok(pair)) => pair,
        };
        if config.relay == Some(from) && !relay_engaged {
            relay_engaged = true;
            tracing::debug!(from = %from, "relay engaged; releasing the direct leg");
        }
        let done = handle_datagram(
            socket,
            &buf[..len],
            from,
            targets,
            &mut states,
            config.relay,
            &mut relay_echo,
        )
        .await;
        if let Some(established) = done {
            return Ok(established);
        }
    }
    Err(P2pError::Handshake {
        timeout: config.timeout,
        relay_engaged,
    })
}

/// One sending tick of the direct (camera) paths: one fresh txid per path,
/// retried at most four times a second apart and never after the echo lands;
/// the extended SYN follows the echo (or a probe at +0.5s), repeated every
/// 200ms up to four times.
async fn drive_direct_paths(
    socket: &UdpSocket,
    targets: &[SocketAddr],
    states: &mut [PathState],
    names: &mut NameGen,
    now: Instant,
) {
    for (state, target) in states.iter_mut().zip(targets) {
        let syn_due = !state.echo
            && (state.syn.is_none()
                || (state.tries < 4 && state.sent.is_some_and(|sent| now - sent > RETRY_GAP)));
        if syn_due {
            let txid = names.generate();
            state.syn = Some(simple_syn(&txid, &nonce()));
            state.txid = Some(txid);
            state.sent = Some(now);
            state.tries += 1;
            if let Some(syn) = &state.syn {
                let _ = socket.send_to(syn, target).await;
            }
        }
        let ext_due = state.echo
            || state
                .probed
                .is_some_and(|probed| now - probed > PROBE_GRACE);
        if ext_due && state.ext.is_none() {
            let txid = names.generate();
            let syn = extended_syn(&txid, &nonce());
            state.ext = Some(ExtState {
                txid,
                syn,
                sent: None,
                tries: 0,
            });
        }
        if let Some(ext) = &mut state.ext {
            let due = ext.tries == 0
                || (ext.tries < 4 && ext.sent.is_some_and(|sent| now - sent > EXT_GAP));
            if due {
                ext.sent = Some(now);
                ext.tries += 1;
                let _ = socket.send_to(&ext.syn, target).await;
            }
        }
    }
}

/// Classify and answer one datagram; `Some` when the session comes up.
async fn handle_datagram(
    socket: &UdpSocket,
    data: &[u8],
    from: SocketAddr,
    targets: &[SocketAddr],
    states: &mut [PathState],
    relay: Option<SocketAddr>,
    relay_echo: &mut bool,
) -> Option<Established> {
    match classify(data) {
        Packet::Probe { txid } => {
            // The peer's own query: answer it, as the app does.
            if let Some(ack) = peer_ack(txid, &nonce()) {
                let _ = socket.send_to(&ack, from).await;
            }
            if let Some(index) = targets.iter().position(|target| *target == from) {
                states[index].probed = Some(Instant::now());
            }
            None
        }
        Packet::ExtendedAck { txid } => {
            let ours = states.iter().any(|state| {
                state
                    .ext
                    .as_ref()
                    .is_some_and(|ext| ext.txid.as_str().as_bytes() == txid)
            });
            if ours {
                tracing::debug!(from = %from, "extended SYN acked - session up");
                return Some(Established { addr: from });
            }
            None
        }
        Packet::Echo { txid } => {
            if relay == Some(from) {
                *relay_echo = true;
            }
            for state in states.iter_mut() {
                if state
                    .txid
                    .as_ref()
                    .is_some_and(|own| own.as_str().as_bytes() == txid)
                {
                    state.echo = true;
                }
            }
            None
        }
        // The ack was lost, or the peer armed early: media already flows.
        Packet::Rtp => {
            tracing::debug!(from = %from, "RTP before a clean ack - proceeding");
            Some(Established { addr: from })
        }
        Packet::Bye | Packet::Other => None,
    }
}

/// One-shot STUN binding request from the media socket; `None` when nothing
/// answers in time. Sent from the same socket that carries the offered port,
/// so the mapped address is the one the camera should punch to (the app's own
/// srflx candidates look exactly like this).
pub async fn discover_srflx(socket: &UdpSocket, server: &str) -> Option<String> {
    let mut txid = [0_u8; 12];
    rand::rng().fill(&mut txid);
    let mut request = Vec::with_capacity(20);
    request.extend_from_slice(&[0x00, 0x01, 0x00, 0x00]);
    request.extend_from_slice(&[0x21, 0x12, 0xA4, 0x42]);
    request.extend_from_slice(&txid);
    if socket.send_to(&request, server).await.is_err() {
        return None;
    }
    let deadline = Instant::now() + Duration::from_millis(2500);
    let mut buf = [0_u8; 2048];
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let Ok(Ok((len, _))) = tokio::time::timeout(remaining, socket.recv_from(&mut buf)).await
        else {
            return None;
        };
        let data = &buf[..len];
        if data.len() < 20
            || data[..2] != [0x01, 0x01]
            || data[4..8] != [0x21, 0x12, 0xA4, 0x42]
            || data[8..20] != txid
        {
            continue;
        }
        return mapped_address(&data[20..]);
    }
    None
}

/// Pull the mapped address out of a binding success response's attributes.
fn mapped_address(mut attributes: &[u8]) -> Option<String> {
    while attributes.len() >= 4 {
        let attr_type = u16::from_be_bytes([attributes[0], attributes[1]]);
        let attr_len = usize::from(u16::from_be_bytes([attributes[2], attributes[3]]));
        let value = attributes.get(4..4 + attr_len)?;
        if attr_type == 0x0020 && value.len() >= 8 {
            // XOR-MAPPED-ADDRESS: the port XORed with the magic's high half,
            // the address with the whole magic.
            let port = u16::from_be_bytes([value[2], value[3]]) ^ 0x2112;
            let ip = Ipv4Addr::new(
                value[4] ^ 0x21,
                value[5] ^ 0x12,
                value[6] ^ 0xA4,
                value[7] ^ 0x42,
            );
            return Some(format!("{ip}:{port}"));
        }
        if attr_type == 0x0001 && value.len() >= 8 {
            let port = u16::from_be_bytes([value[2], value[3]]);
            let ip = Ipv4Addr::new(value[4], value[5], value[6], value[7]);
            return Some(format!("{ip}:{port}"));
        }
        attributes = attributes.get(4 + attr_len + (4 - attr_len % 4) % 4..)?;
    }
    None
}

/// Parse an `ip:port` endpoint, `None` when it is not one.
pub fn endpoint_of(ip: &str, port: u16) -> Option<SocketAddr> {
    let ip: IpAddr = ip.parse().ok()?;
    Some(SocketAddr::new(ip, port))
}
