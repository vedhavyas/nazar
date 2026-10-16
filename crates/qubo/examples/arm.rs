//! Arm the ported fast-lane engine against the live camera - the M6 Task 2
//! run. Loads a stored session, arms one P2P session, prints the acceptance
//! evidence (relay engagement comes through the debug log; the first
//! decrypted datagrams are printed here), and disarms.
//!
//! Run with the debug log on, from the repo root:
//!
//!   RUST_LOG=qubo=debug cargo run -p qubo --release --example arm -- \
//!     --session /data/session.json \
//!     --unit 20000000-0000-4000-8000-000000000001 \
//!     --camera c0000000-0000-4000-8000-000000000001 \
//!     --self-device d0000000-0000-4000-8000-000000000002 \
//!     --camera-controller d0000000-0000-4000-8000-000000000001 \
//!     --seconds 60
//!
//! The identity defaults are the capture's own (the phone's unit device),
//! exactly what the Python reference ran with. Acceptance is the
//! `relay engaged` debug line within ~1s of the register, then `extended
//! SYN acked - session up`, then `[rtp]` lines appearing below.

use std::error::Error;
use std::time::Duration;

use qubo::p2p::{AesMode, P2pConfig, P2pEngine, P2pIdentity};
use qubo::{BASE_URL, Device, SessionStore};

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse()?;
    let session = SessionStore::new(&args.session)
        .load()?
        .ok_or("no stored session at the given --session path")?;
    println!("[arm] session for user {}", session.uuid);

    let client = qubo::QuboClient::new(BASE_URL.parse()?, qubo::ServiceProvider::default());
    let engine = P2pEngine::new(
        client,
        P2pConfig {
            device_name: "nazar-arm".to_owned(),
            aes_mode: args.aes_mode,
            ..P2pConfig::default()
        },
    );
    let device = Device {
        device_id: args.camera.clone(),
        name: "camera".to_owned(),
        device_type: String::new(),
        unit_id: args.unit.clone(),
        entity_id: String::new(),
    };
    let identity = P2pIdentity {
        self_device: args.self_device.clone(),
        src_device_id: args.src_device_id.clone(),
        camera_controller: Some(args.camera_controller.clone()),
    };

    println!("[arm] arming {} (watch for relay engagement)", args.camera);
    let mut armed = engine.arm(&session, &device, &identity).await?;
    println!(
        "[arm] armed session={} peer={} \n[arm] reading media for {}s",
        armed.session_id(),
        armed.peer(),
        args.seconds
    );

    let deadline = std::time::Instant::now() + Duration::from_secs(args.seconds);
    let mut datagrams = 0_u32;
    let mut payload_types = std::collections::BTreeSet::new();
    let mut saw_start_code = false;
    while std::time::Instant::now() < deadline {
        match armed.recv_media().await? {
            Some(packet) => {
                datagrams += 1;
                payload_types.insert(packet.header().payload_type);
                let payload = packet.payload();
                saw_start_code |= payload.windows(4).any(|w| w == [0, 0, 0, 1]);
                if datagrams <= 10 {
                    println!(
                        "[rtp] #{datagrams} pt={} seq={} len={} head={}",
                        packet.header().payload_type,
                        packet.header().sequence,
                        payload.len(),
                        hex(&payload[..payload.len().min(16)]),
                    );
                }
            }
            None => println!("[arm] idle so far: {datagrams} datagrams"),
        }
    }
    println!(
        "[arm] received {datagrams} datagrams; payload types {payload_types:?}; H.264 start code seen: {saw_start_code}"
    );
    println!("[arm] disarming (endP2pCall)");
    armed.disarm().await;
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut out, byte| {
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// The flags; every default is the capture's own identity, so the run needs
/// only a session path.
struct Args {
    session: std::path::PathBuf,
    unit: String,
    camera: String,
    self_device: String,
    src_device_id: String,
    camera_controller: String,
    seconds: u64,
    aes_mode: AesMode,
}

impl Args {
    fn parse() -> Result<Self, Box<dyn Error>> {
        let mut args = Self {
            session: "/data/session.json".into(),
            unit: "20000000-0000-4000-8000-000000000001".to_owned(),
            camera: "c0000000-0000-4000-8000-000000000001".to_owned(),
            self_device: "d0000000-0000-4000-8000-000000000002".to_owned(),
            src_device_id: "<mobile-id>".to_owned(),
            camera_controller: "d0000000-0000-4000-8000-000000000001".to_owned(),
            seconds: 60,
            aes_mode: AesMode::CtrCont,
        };
        let mut items = std::env::args().skip(1);
        while let Some(flag) = items.next() {
            let value = items
                .next()
                .ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--session" => args.session = value.into(),
                "--unit" => args.unit = value,
                "--camera" => args.camera = value,
                "--self-device" => args.self_device = value,
                "--src-device-id" => args.src_device_id = value,
                "--camera-controller" => args.camera_controller = value,
                "--seconds" => args.seconds = value.parse()?,
                // The captures never proved the mode (protocol 6.5); switch
                // it here when a run shows garbage plaintext.
                "--aes-mode" => {
                    args.aes_mode = match value.as_str() {
                        "ctr-cont" => AesMode::CtrCont,
                        "ctr-reset" => AesMode::CtrReset,
                        "cfb" => AesMode::Cfb,
                        "ofb" => AesMode::Ofb,
                        "plain" => AesMode::Plain,
                        other => return Err(format!("unknown aes mode {other}").into()),
                    };
                }
                other => return Err(format!("unknown flag {other}").into()),
            }
        }
        Ok(args)
    }
}
