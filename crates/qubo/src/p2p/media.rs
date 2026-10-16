//! The media leg: RTP unwrap, AES decrypt, H.264 depacketization.
//!
//! Ported from `reference/p2p_client.py`; the wire shapes are from
//! `docs/qubo-protocol.md` 6.5.

use aes::Aes128;
use aes::cipher::{BlockEncrypt as _, KeyInit as _, KeyIvInit as _, StreamCipher as _};
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

/// The header length, CSRC list and extension header included; 0 when the
/// datagram is too short to be RTP. Version-unchecked: the reference's
/// receive loop gates and decrypts by exactly this arithmetic, and under a
/// continuing keystream, skipping a datagram the reference processed would
/// skew every later packet's plaintext.
pub fn rtp_header_len(packet: &[u8]) -> usize {
    if packet.len() < 12 {
        return 0;
    }
    let csrcs = usize::from(packet[0] & 0x0F);
    let mut header_len = 12 + 4 * csrcs;
    if packet[0] & 0x10 != 0 {
        // The extension header's own 4 bytes state how many words follow;
        // a tail too short to carry them counts as zero words, which lands
        // in the same skip the reference's clamped read produces.
        let words = if packet.len() >= header_len + 4 {
            u16::from_be_bytes([packet[header_len + 2], packet[header_len + 3]])
        } else {
            0
        };
        header_len += 4 + 4 * usize::from(words);
    }
    header_len
}

/// The fixed header fields at `header_len`; the caller guarantees at least
/// 12 bytes.
pub fn parse_header(packet: &[u8], header_len: usize) -> RtpHeader {
    RtpHeader {
        marker: packet[1] & 0x80 != 0,
        payload_type: packet[1] & 0x7F,
        sequence: u16::from_be_bytes([packet[2], packet[3]]),
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        header_len,
    }
}

/// Parse an RTP datagram's fixed header (v2 only, with a non-empty
/// payload), skipping a CSRC list and the extension header when present.
pub fn unwrap_rtp(packet: &[u8]) -> Option<RtpHeader> {
    if packet.len() < 12 || packet[0] >> 6 != 2 {
        return None;
    }
    let header_len = rtp_header_len(packet);
    if header_len == 0 || header_len >= packet.len() {
        return None;
    }
    Some(parse_header(packet, header_len))
}

/// The AES modes the reference client offers for the media payloads. The
/// captures never proved which one the camera uses (protocol 6.5), so the
/// mode is a switch the live verification settles - all five exist so a
/// live run can tell "wrong mode" from "not implemented".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AesMode {
    /// One keystream continued across packets - the reference's default.
    #[default]
    CtrCont,
    /// The keystream restarts at the session iv on every packet.
    CtrReset,
    /// CFB128: the feedback is the ciphertext; the keystream is continuous
    /// across packets (OpenSSL's EVP streaming, which the reference uses).
    Cfb,
    /// OFB: the feedback is the keystream itself; continuous across packets.
    Ofb,
    /// No encryption: the payload passes through (a diagnostic mode).
    Plain,
}

/// A session's media decryptor: the stream `aesKey`/`aesIV` as hex or
/// base64, the mode choosing how the keystream runs.
pub struct MediaDecryptor {
    mode: AesMode,
    key: [u8; 16],
    iv: [u8; 16],
    aes: Aes128,
    ctr: Ctr128BE<Aes128>,
    /// CFB/OFB block state: the working keystream and the offset within it.
    keystream: [u8; 16],
    /// CFB's feedback register (the ciphertext); the keystream starts at
    /// the iv.
    feedback: [u8; 16],
    position: usize,
}

impl MediaDecryptor {
    pub fn new(mode: AesMode, key: &str, iv: &str) -> Result<Self, P2pError> {
        let key = decode_key16(key)?;
        let iv = decode_key16(iv)?;
        Ok(Self {
            mode,
            key,
            iv,
            aes: Aes128::new(&key.into()),
            ctr: Ctr128BE::new(&key.into(), &iv.into()),
            keystream: iv,
            feedback: iv,
            position: 0,
        })
    }

    /// Decrypt a payload in place, byte-streamed across calls the way the
    /// reference's one decryptor per session streams it.
    pub fn decrypt(&mut self, payload: &mut [u8]) {
        match self.mode {
            AesMode::CtrCont => self.ctr.apply_keystream(payload),
            AesMode::CtrReset => {
                let mut stream = Ctr128BE::<Aes128>::new(&self.key.into(), &self.iv.into());
                stream.apply_keystream(payload);
            }
            AesMode::Cfb => {
                // CFB128, byte-wise across calls as OpenSSL EVP streams it:
                // the keystream refreshes every 16 bytes and the feedback is
                // the ciphertext bytes.
                for byte in payload {
                    if self.position == 0 {
                        self.keystream = self.encrypt_block(&self.feedback);
                    }
                    let cipher = *byte;
                    *byte ^= self.keystream[self.position];
                    self.feedback[self.position] = cipher;
                    self.position = (self.position + 1) % 16;
                }
            }
            AesMode::Ofb => {
                // OFB: the feedback is the keystream itself, so the register
                // starts at the iv and refreshes every 16 bytes.
                for byte in payload {
                    if self.position == 0 {
                        self.keystream = self.encrypt_block(&self.keystream);
                    }
                    *byte ^= self.keystream[self.position];
                    self.position = (self.position + 1) % 16;
                }
            }
            AesMode::Plain => {}
        }
    }

    fn encrypt_block(&self, block: &[u8; 16]) -> [u8; 16] {
        let mut out: aes::Block = (*block).into();
        self.aes.encrypt_block(&mut out);
        out.into()
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
                // STAP-A: 2-byte sizes, each NAL without its own header
                // byte. A truncated tail emits the bytes that are there,
                // as the reference's clamped slice read does.
                let mut offset = 1;
                while offset + 2 <= payload.len() {
                    let size =
                        usize::from(u16::from_be_bytes([payload[offset], payload[offset + 1]]));
                    let end = (offset + 2 + size).min(payload.len());
                    let nal = &payload[offset + 2..end];
                    if !nal.is_empty() {
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
