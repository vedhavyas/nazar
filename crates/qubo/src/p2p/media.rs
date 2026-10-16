//! The media leg: RTP unwrap, AES decrypt, H.264 depacketization.
//!
//! Ported from `reference/p2p_client.py`; the wire shapes are from
//! `docs/qubo-protocol.md` 6.5.

use aes::Aes128;
use aes::cipher::{KeyIvInit as _, StreamCipher as _};
use base64::Engine as _;
use ctr::Ctr128BE;

use crate::p2p::P2pError;

/// The RTP header fields the fast lane reads (RFC 3550).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RtpHeader {
    pub marker: bool,
    pub payload_type: u8,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    /// The header length, CSRC list and extension header included.
    pub header_len: usize,
}

/// Parse an RTP datagram's fixed header, skipping a CSRC list and the
/// extension header when present. `None` for anything that is not a v2
/// datagram with a non-empty payload.
pub fn unwrap_rtp(packet: &[u8]) -> Option<RtpHeader> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let csrcs = usize::from(packet[0] & 0x0F);
    let mut header_len = 12 + 4 * csrcs;
    if packet[0] & 0x10 != 0 {
        // The extension header's own 4 bytes state how many words follow.
        if packet.len() < header_len + 4 {
            return None;
        }
        let words = usize::from(u16::from_be_bytes([
            packet[header_len + 2],
            packet[header_len + 3],
        ]));
        header_len += 4 + 4 * words;
    }
    if header_len >= packet.len() {
        return None;
    }
    Some(RtpHeader {
        marker: packet[1] & 0x80 != 0,
        payload_type: packet[1] & 0x7F,
        sequence: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        header_len,
    })
}

/// The AES modes the reference client offers for the media payloads. The
/// captures never proved which one the camera uses (protocol 6.5), so the
/// mode is a switch the live verification settles.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AesMode {
    /// One keystream continued across packets - the reference's default.
    #[default]
    CtrCont,
    /// The keystream restarts at the session iv on every packet.
    CtrReset,
}

/// A session's media decryptor: the stream `aesKey`/`aesIV` as hex or
/// base64, the mode choosing whether the keystream runs on.
pub struct MediaDecryptor {
    mode: AesMode,
    key: [u8; 16],
    iv: [u8; 16],
    stream: Ctr128BE<Aes128>,
}

impl MediaDecryptor {
    pub fn new(mode: AesMode, key: &str, iv: &str) -> Result<Self, P2pError> {
        let key = decode_key16(key)?;
        let iv = decode_key16(iv)?;
        Ok(Self {
            mode,
            key,
            iv,
            stream: Ctr128BE::new(&key.into(), &iv.into()),
        })
    }

    /// Decrypt a payload in place. `CtrCont` keeps one keystream running
    /// across calls; `CtrReset` restarts it at the iv on every call.
    pub fn decrypt(&mut self, payload: &mut [u8]) {
        match self.mode {
            AesMode::CtrCont => self.stream.apply_keystream(payload),
            AesMode::CtrReset => {
                let mut stream = Ctr128BE::<Aes128>::new(&self.key.into(), &self.iv.into());
                stream.apply_keystream(payload);
            }
        }
    }
}

/// Decode a 16-byte session key: 32 hex characters as captured, or the
/// equivalent base64. The reference pads malformed input into a key of the
/// right length; failing instead is deliberate - a wrong key cannot decrypt,
/// so a silent fallback only moves the failure downstream.
pub fn decode_key16(text: &str) -> Result<[u8; 16], P2pError> {
    if let Some(raw) = decode_hex16(text) {
        return Ok(raw);
    }
    if let Ok(raw) = base64::engine::general_purpose::STANDARD.decode(text)
        && let Ok(raw) = <[u8; 16]>::try_from(raw)
    {
        return Ok(raw);
    }
    Err(P2pError::KeyFormat)
}

fn decode_hex16(text: &str) -> Option<[u8; 16]> {
    let bytes = text.as_bytes();
    if bytes.len() != 32 {
        return None;
    }
    let mut out = [0_u8; 16];
    for (slot, chunk) in out.iter_mut().zip(bytes.as_chunks::<2>().0) {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        *slot = u8::try_from(hi * 16 + lo).ok()?;
    }
    Some(out)
}

/// The NAL kinds the reference client watches for, as a bitmask so the
/// assembler stays a small struct.
#[derive(Debug, Default, Clone, Copy)]
struct NalFlags(u8);

impl NalFlags {
    fn record(&mut self, nal_type: u8) {
        match nal_type {
            7 => self.0 |= 0b001,
            8 => self.0 |= 0b010,
            5 => self.0 |= 0b100,
            _ => {}
        }
    }

    fn sps(self) -> bool {
        self.0 & 0b001 != 0
    }

    fn pps(self) -> bool {
        self.0 & 0b010 != 0
    }

    fn idr(self) -> bool {
        self.0 & 0b100 != 0
    }
}

/// RFC 6184 depacketizer: single NAL units (types 1-23), STAP-A (24) and
/// FU-A (28).
#[derive(Debug, Default)]
pub struct H264Assembler {
    buffer: Vec<u8>,
    fragment: bool,
    seen: NalFlags,
}

impl H264Assembler {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one RTP payload; returns the NAL units that completed with it.
    pub fn feed(&mut self, payload: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let Some(&first) = payload.first() else {
            return out;
        };
        match first & 0x1F {
            1..=23 => out.push(payload.to_vec()),
            24 => {
                // STAP-A: 2-byte sizes, each NAL without its own header byte.
                let mut offset = 1;
                while offset + 2 <= payload.len() {
                    let size =
                        usize::from(u16::from_be_bytes([payload[offset], payload[offset + 1]]));
                    if let Some(nal) = payload.get(offset + 2..offset + 2 + size)
                        && !nal.is_empty()
                    {
                        out.push(nal.to_vec());
                    }
                    offset += 2 + size;
                }
            }
            28 if payload.len() >= 3 => {
                // FU-A: the fragments carry the NAL without its header byte;
                // the header is rebuilt from the FU indicator and header.
                let start = payload[1] & 0x80 != 0;
                let end = payload[1] & 0x40 != 0;
                if start {
                    self.buffer.clear();
                    self.buffer.push((first & 0xE0) | (payload[1] & 0x1F));
                    self.fragment = true;
                }
                if self.fragment {
                    self.buffer.extend_from_slice(&payload[2..]);
                    if end {
                        out.push(std::mem::take(&mut self.buffer));
                        self.fragment = false;
                    }
                }
            }
            _ => {}
        }
        for nal in &out {
            self.seen.record(nal[0] & 0x1F);
        }
        out
    }

    pub fn saw_sps(&self) -> bool {
        self.seen.sps()
    }

    pub fn saw_pps(&self) -> bool {
        self.seen.pps()
    }

    pub fn saw_idr(&self) -> bool {
        self.seen.idr()
    }
}
