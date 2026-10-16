//! A minimal MQTT 3.1.1 broker for the feed's tests: it answers a CONNECT,
//! a SUBSCRIBE and a PING, counts the sessions it has taken, and can
//! publish a heartbeat on a cadence so a feed has something to hear.
//!
//! The count is of SUBSCRIBEs, not of TCP connections: the client library
//! opens a socket per attempt that it may abandon before sending anything,
//! so an accept is not a session.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};

/// A broker on a loopback port.
pub struct FakeBroker {
    port: u16,
    sessions: Arc<AtomicUsize>,
    /// Every password a CONNECT carried, in arrival order: what the feed
    /// actually authenticated with.
    connects: Arc<Mutex<Vec<String>>>,
}

/// What a broker does with a connection once it is subscribed.
#[derive(Clone, Copy)]
pub enum Mode {
    /// Nothing at all, so a feed goes quiet.
    Silent,
    /// A heartbeat on a cadence, so a feed hears something but no motion.
    Chatty(Duration),
    /// A motion event on a cadence.
    Moving(Duration),
    /// A subscription refusal (0x80), as a broker with a dead session
    /// answers.
    Refusing,
    /// A subscription accepted, then the connection dropped.
    Dropping,
    /// One heartbeat, a pause to let it land, then the connection dropped:
    /// a feed that was healthy and then lost the broker.
    Shaky(Duration),
}

impl FakeBroker {
    /// Start a broker that says nothing after the handshake.
    pub async fn silent() -> Self {
        Self::start(Mode::Silent).await
    }

    /// Start a broker that publishes a heartbeat every `every`, so a feed
    /// connected to it never goes quiet.
    pub async fn chatty(every: Duration) -> Self {
        Self::start(Mode::Chatty(every)).await
    }

    /// Start a broker that publishes motion every `every`.
    pub async fn moving(every: Duration) -> Self {
        Self::start(Mode::Moving(every)).await
    }

    /// Start a broker that refuses every subscription.
    pub async fn refusing() -> Self {
        Self::start(Mode::Refusing).await
    }

    /// Start a broker that takes a subscription and then drops the
    /// connection, as a broker restarting under a healthy feed does.
    pub async fn dropping() -> Self {
        Self::start(Mode::Dropping).await
    }

    /// Start a broker that delivers one heartbeat and then drops the
    /// connection: every cycle was briefly healthy.
    pub async fn shaky(pause: Duration) -> Self {
        Self::start(Mode::Shaky(pause)).await
    }

    async fn start(mode: Mode) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let port = listener.local_addr().expect("the address").port();
        let sessions = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&sessions);
        let connects = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&connects);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let counter = Arc::clone(&counter);
                let recorder = Arc::clone(&recorder);
                tokio::spawn(async move { serve(socket, mode, counter, recorder).await });
            }
        });
        Self {
            port,
            sessions,
            connects,
        }
    }

    /// The passwords every CONNECT carried, in arrival order.
    pub fn connects(&self) -> Vec<String> {
        self.connects.lock().unwrap().clone()
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// How many subscriptions the broker has taken: one per feed cycle.
    pub fn sessions(&self) -> usize {
        self.sessions.load(Ordering::SeqCst)
    }
}

/// The heartbeat a chatty broker sends; the topic is a monitor topic the
/// feed parses, so an event lands on the bus too.
fn heartbeat() -> (String, Vec<u8>) {
    let topic = "/monitor/20000000-0000-4000-8000-000000000001/d0000000-0000-4000-8000-000000000002/heartbeat".to_owned();
    let payload = br#"{"devices" : {"action" : 140,"deviceUUID" : "d0000000-0000-4000-8000-000000000002","operationState" : "online","timestamp" : 1791974159745}}"#.to_vec();
    (topic, payload)
}

/// A motion event, in the captured payload's shape.
fn motion() -> (String, Vec<u8>) {
    let topic = "/monitor/20000000-0000-4000-8000-000000000001/c0000000-0000-4000-8000-000000000002/recordingControl".to_owned();
    let payload = br#"{"devices" : {"deviceUUID" : "c0000000-0000-4000-8000-000000000002","services" : {"recordingControl" : {"events" : {"stateChanged" : {"cause" : "MOTION_DETECTED","record" : "start"}}}},"timestamp" : 1791974501206}}"#.to_vec();
    (topic, payload)
}

async fn serve(
    mut socket: TcpStream,
    mode: Mode,
    sessions: Arc<AtomicUsize>,
    connects: Arc<Mutex<Vec<String>>>,
) {
    loop {
        let Some(packet) = read_packet(&mut socket).await else {
            return;
        };
        match packet[0] >> 4 {
            // CONNECT: accepted, no session carried over.
            1 => {
                if let Some(token) = connect_password(&packet) {
                    connects.lock().unwrap().push(token);
                }
                if socket.write_all(&[0x20, 0x02, 0x00, 0x00]).await.is_err() {
                    return;
                }
            }
            // SUBSCRIBE: ack every filter, or refuse them all; a chatty or
            // moving broker then keeps the socket fed.
            8 => {
                sessions.fetch_add(1, Ordering::SeqCst);
                let id = u16::from_be_bytes([packet[2], packet[3]]);
                let mut at = 4;
                let mut codes = Vec::new();
                while at + 3 <= packet.len() {
                    let len = u16::from_be_bytes([packet[at], packet[at + 1]]) as usize;
                    at += 2 + len + 1;
                    codes.push(match mode {
                        Mode::Refusing => 0x80,
                        _ => 0x01,
                    });
                }
                let mut ack = vec![0x90];
                push_length(&mut ack, 2 + codes.len());
                ack.extend_from_slice(&id.to_be_bytes());
                ack.extend_from_slice(&codes);
                if socket.write_all(&ack).await.is_err() {
                    return;
                }
                if matches!(mode, Mode::Dropping) {
                    return;
                }
                if let Mode::Shaky(pause) = mode {
                    let (topic, payload) = heartbeat();
                    if socket
                        .write_all(&publish_frame(&topic, &payload))
                        .await
                        .is_err()
                    {
                        return;
                    }
                    tokio::time::sleep(pause).await;
                    return;
                }
                if let Some(every) = match mode {
                    Mode::Chatty(every) | Mode::Moving(every) => Some(every),
                    _ => None,
                } {
                    let (topic, payload) = match mode {
                        Mode::Moving(_) => motion(),
                        _ => heartbeat(),
                    };
                    let frame = publish_frame(&topic, &payload);
                    loop {
                        tokio::time::sleep(every).await;
                        if socket.write_all(&frame).await.is_err() {
                            return;
                        }
                    }
                }
            }
            // PINGREQ
            12 => {
                if socket.write_all(&[0xD0, 0x00]).await.is_err() {
                    return;
                }
            }
            // DISCONNECT
            14 => return,
            _ => {}
        }
    }
}

fn publish_frame(topic: &str, payload: &[u8]) -> Vec<u8> {
    let mut body = Vec::new();
    let len = u16::try_from(topic.len()).expect("a topic that fits a length prefix");
    body.extend_from_slice(&len.to_be_bytes());
    body.extend_from_slice(topic.as_bytes());
    body.extend_from_slice(payload);
    let mut frame = vec![0x30];
    push_length(&mut frame, body.len());
    frame.extend_from_slice(&body);
    frame
}

/// The password a CONNECT packet carries, when its flags include one: the
/// feed sends the session token there, so it is what a connect
/// authenticated with.
fn connect_password(packet: &[u8]) -> Option<String> {
    // read_packet keeps only the fixed header's first byte: the body -
    // protocol name first - starts right after it.
    let mut at = 1;
    let read_str = |at: &mut usize| -> Option<&[u8]> {
        let len = u16::from_be_bytes([*packet.get(*at)?, *packet.get(*at + 1)?]) as usize;
        *at += 2;
        let out = packet.get(*at..*at + len)?;
        *at += len;
        Some(out)
    };
    read_str(&mut at)?; // protocol name
    at += 1; // protocol level
    let flags = *packet.get(at)?;
    at += 1;
    at += 2; // keepalive
    read_str(&mut at)?; // client id
    if flags & 0x80 != 0 {
        read_str(&mut at)?; // username
    }
    if flags & 0x40 != 0 {
        return String::from_utf8(read_str(&mut at)?.to_vec()).ok();
    }
    None
}

/// Read one MQTT packet: the header, the length, and the body.
async fn read_packet(socket: &mut TcpStream) -> Option<Vec<u8>> {
    let mut header = [0u8; 1];
    socket.read_exact(&mut header).await.ok()?;
    let mut length = 0usize;
    let mut shift = 0;
    loop {
        let mut byte = [0u8; 1];
        socket.read_exact(&mut byte).await.ok()?;
        length |= ((byte[0] & 0x7F) as usize) << shift;
        shift += 7;
        if byte[0] & 0x80 == 0 {
            break;
        }
        if shift > 21 {
            return None;
        }
    }
    let mut body = vec![0u8; length];
    socket.read_exact(&mut body).await.ok()?;
    let mut packet = vec![header[0]];
    packet.extend_from_slice(&body);
    Some(packet)
}

fn push_length(frame: &mut Vec<u8>, mut length: usize) {
    loop {
        let mut byte = u8::try_from(length % 128).expect("a byte by construction");
        length /= 128;
        if length > 0 {
            byte |= 0x80;
        }
        frame.push(byte);
        if length == 0 {
            return;
        }
    }
}
