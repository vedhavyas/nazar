//! The broker leg of an arm: the start call is published, and the
//! subscription is re-issued after a reconnect - the reference's paho
//! re-subscribes in on_connect, and a blip inside the answer window must
//! not lose the answer.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use serde_json::json;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::p2p::{ArmSwitches, P2pConfig, P2pEngine, P2pIdentity};
use qubo::{Device, QuboClient};

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const CAMERA: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const V3_PATH: &str = "/stream-manager/api/v3/sp/d10e4bfb0153496e8e8bb955f7ebe413/unit/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/device/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01/p2p";

/// A minimal MQTT 3.1.1 double: CONNACK, SUBACK, counters, and a single
/// blip - the first connection drops right after its first SUBACK, so the
/// client has to reconnect and (as the reference does) re-subscribe.
struct Broker {
    port: u16,
    subscribes: Arc<AtomicUsize>,
    publishes: Arc<AtomicUsize>,
}

impl Broker {
    async fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("a port");
        let port = listener.local_addr().expect("the address").port();
        let subscribes = Arc::new(AtomicUsize::new(0));
        let publishes = Arc::new(AtomicUsize::new(0));
        let blipped = Arc::new(AtomicBool::new(false));
        let accept_subscribes = Arc::clone(&subscribes);
        let accept_publishes = Arc::clone(&publishes);
        tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                let subscribes = Arc::clone(&accept_subscribes);
                let publishes = Arc::clone(&accept_publishes);
                let blipped = Arc::clone(&blipped);
                tokio::spawn(async move {
                    serve(socket, &subscribes, &publishes, &blipped).await;
                });
            }
        });
        Self {
            port,
            subscribes,
            publishes,
        }
    }
}

/// One MQTT connection, varint-aware: publishes arrive at QoS 2 and must be
/// completed (PUBREC/PUBCOMP), or the client's request pipeline blocks and
/// the re-subscribe never goes out.
async fn serve(
    mut socket: TcpStream,
    subscribes: &AtomicUsize,
    publishes: &AtomicUsize,
    blipped: &AtomicBool,
) {
    while let Some((kind, body)) = read_packet(&mut socket).await {
        match kind {
            1 => {
                // CONNECT: session present 0, accepted.
                if socket.write_all(&[0x20, 0x02, 0x00, 0x00]).await.is_err() {
                    return;
                }
            }
            8 => {
                // SUBSCRIBE: grant QoS 1 for whatever was asked.
                subscribes.fetch_add(1, Ordering::SeqCst);
                let id = [body[0], body[1]];
                if socket
                    .write_all(&[0x90, 0x03, id[0], id[1], 0x01])
                    .await
                    .is_err()
                {
                    return;
                }
                if !blipped.swap(true, Ordering::SeqCst) {
                    // The blip: drop the first connection once it is
                    // subscribed. The client must come back and re-subscribe.
                    return;
                }
            }
            3 => {
                // PUBLISH (QoS 2): count it and complete the handshake.
                publishes.fetch_add(1, Ordering::SeqCst);
                let topic_len = usize::from(u16::from_be_bytes([body[0], body[1]]));
                let at = 2 + topic_len;
                let id = [body[at], body[at + 1]];
                if socket.write_all(&[0x50, 0x02, id[0], id[1]]).await.is_err() {
                    return;
                }
            }
            6 => {
                // PUBREL: PUBCOMP.
                let id = [body[0], body[1]];
                if socket.write_all(&[0x70, 0x02, id[0], id[1]]).await.is_err() {
                    return;
                }
            }
            12 => {
                // PINGREQ.
                let _ = socket.write_all(&[0xD0, 0x00]).await;
            }
            14 => return,
            _ => {}
        }
    }
}

/// One whole MQTT packet: the type nibble and the body, the varint length
/// and the fixed header consumed.
async fn read_packet(socket: &mut TcpStream) -> Option<(u8, Vec<u8>)> {
    let mut first = [0_u8; 1];
    socket.read_exact(&mut first).await.ok()?;
    let mut length = 0_usize;
    let mut multiplier = 1_usize;
    for _ in 0..4 {
        let mut byte = [0_u8; 1];
        socket.read_exact(&mut byte).await.ok()?;
        length += usize::from(byte[0] & 0x7F) * multiplier;
        if byte[0] & 0x80 == 0 {
            break;
        }
        multiplier *= 128;
    }
    let mut body = vec![0_u8; length];
    socket.read_exact(&mut body).await.ok()?;
    Some((first[0] >> 4, body))
}

#[tokio::test]
async fn an_arm_publishes_the_start_call_and_resubscribes_after_a_blip() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter("qubo=debug")
        .try_init();
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path(V3_PATH))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                json!({
                    "appSessionId": "d0000000-0000-4000-8000-000000000004",
                    "aesKey": "82458EA37F4740361F47B4E68946546F",
                    "iv": "A3B6C1B75EEB65ACF59802CE5A6995B3",
                })
                .to_string(),
            ),
        )
        .mount(&server)
        .await;
    let broker = Broker::start().await;

    let engine = P2pEngine::new(
        QuboClient::new(server.uri().parse().expect("a url"), SP.into()),
        P2pConfig {
            stun_server: None,
            broker_host: "127.0.0.1".to_owned(),
            broker_port: broker.port,
            broker_tls: false,
            answer_timeout: Duration::from_secs(5),
            handshake_timeout: Duration::from_millis(100),
            switches: ArmSwitches {
                device_token: false,
                ..ArmSwitches::default()
            },
            ..P2pConfig::default()
        },
    );
    let session = qubo::Session {
        access_token: "token-abc".to_owned(),
        uuid: "user-uuid-1".to_owned(),
        email: None,
        refresh_token: None,
    };
    let device = Device {
        device_id: CAMERA.to_owned(),
        name: "Hall".to_owned(),
        device_type: "HCP01".to_owned(),
        model: "HCP01".to_owned(),
        unit_id: UNIT.to_owned(),
        entity_id: "default".to_owned(),
        controller_id: None,
    };
    let identity = P2pIdentity {
        self_device: "d0000000-0000-4000-8000-000000000002".to_owned(),
        src_device_id: "<mobile-id>".to_owned(),
        camera_controller: None,
    };

    let arm = tokio::spawn(async move { engine.arm(&session, &device, &identity).await });

    // With no camera answer and no relay endpoint, the arm reports that and
    // tears down; the point here was the broker leg, whose counters outlive
    // the arm.
    let outcome = arm.await.expect("the arm task joins");
    assert!(
        outcome.is_err(),
        "no answer and no relay endpoint: the arm reports an error"
    );
    assert!(
        broker.publishes.load(Ordering::SeqCst) >= 1,
        "the start call reached the broker"
    );
    // The blip forced a reconnect inside the answer window; the
    // subscription landed again across it, as the reference's on_connect
    // re-issues it.
    assert!(
        broker.subscribes.load(Ordering::SeqCst) >= 2,
        "the subscription landed again after the blip (saw {})",
        broker.subscribes.load(Ordering::SeqCst)
    );
}
