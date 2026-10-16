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
//! `--talk <seconds>` chains a talk burst right after the arm: start_talk
//! (the setStreamState update over the data channel), a 440 Hz tone pushed
//! as 20 ms PT 97 frames for that long, then stop_talk. The objective
//! capture is on the wire - the heartbeat's HTTP responses, the control
//! datagrams the camera sends back (logged with RUST_LOG=qubo=debug), its
//! own reporting; hearing the tone needs an ear in the house.
//!
//! The identity defaults are the capture's own (the phone's unit device),
//! exactly what the Python reference ran with. Acceptance is the
//! `relay engaged` debug line within ~1s of the register, then `extended
//! SYN acked - session up`, then `[rtp]` lines appearing below.

use std::error::Error;
use std::time::Duration;

use qubo::p2p::{AesMode, H264Assembler, P2pConfig, P2pEngine, P2pIdentity};
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
            switches: qubo::p2p::ArmSwitches {
                hold_beat: args.hold_beat,
                ..qubo::p2p::ArmSwitches::default()
            },
            ..P2pConfig::default()
        },
    );
    let device = Device {
        device_id: args.camera.clone(),
        name: "camera".to_owned(),
        device_type: String::new(),
        unit_id: args.unit.clone(),
        entity_id: String::new(),
        controller_id: Some(args.camera_controller.clone()),
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

    if args.talk > 0 {
        hold_talk(&mut armed, args.talk).await?;
    }

    let deadline = std::time::Instant::now() + Duration::from_secs(args.seconds);
    let mut datagrams = 0_u32;
    let mut payload_types = std::collections::BTreeSet::new();
    let mut first_bytes: std::collections::BTreeMap<u8, u32> = std::collections::BTreeMap::new();
    let mut assembler = H264Assembler::new();
    let mut nals = 0_u32;
    while std::time::Instant::now() < deadline {
        match armed.recv_media().await? {
            Some(packet) => {
                datagrams += 1;
                payload_types.insert(packet.header().payload_type);
                let payload = packet.payload();
                if packet.header().payload_type == 96 {
                    if let Some(&first) = payload.first() {
                        *first_bytes.entry(first).or_default() += 1;
                    }
                    nals += u32::try_from(assembler.feed(payload).len()).unwrap_or(0);
                }
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
    println!("[arm] received {datagrams} datagrams; payload types {payload_types:?}");
    // RFC 6184 payloads carry bare NALs (and FU-A fragments), not Annex-B
    // start codes, so the first byte is the reliable shape signal.
    println!(
        "[arm] PT 96 first bytes {first_bytes:02x?} \
         (0x7c = FU-A indicator; 0x65 IDR / 0x41 non-IDR / 0x67 SPS / 0x68 PPS = bare NALs)"
    );
    println!(
        "[arm] NAL units {nals}; SPS={} PPS={} IDR={}",
        assembler.saw_sps(),
        assembler.saw_pps(),
        assembler.saw_idr()
    );
    println!("[arm] disarming (endP2pCall)");
    armed.disarm().await;
    Ok(())
}

/// Hold a talk burst: start_talk, `seconds` of 440 Hz tone pushed as 20 ms
/// PT 97 frames, stop_talk.
async fn hold_talk(armed: &mut qubo::ArmedSession, seconds: u64) -> Result<(), Box<dyn Error>> {
    println!("[talk] holding a 440 Hz tone for {seconds}s (chained after the arm)");
    armed.start_talk().await?;
    let frames = seconds * 50;
    let mut start_sample = 0_u32;
    for index in 0..frames {
        armed.push_audio(&tone_frame(start_sample)).await?;
        start_sample = start_sample.saturating_add(320);
        if index % 50 == 49 {
            println!("[talk] {}s of tone pushed", (index + 1) / 50);
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    armed.stop_talk().await?;
    println!("[talk] burst done; the setStreamState off went out");
    Ok(())
}

/// A 440 Hz square tone as 16-bit mono PCM: one 20 ms frame (320 samples at
/// 16 kHz) starting at `start_sample` - integer-only, so the example holds
/// no float casts.
fn tone_frame(start_sample: u32) -> Vec<u8> {
    const RATE: u32 = 16_000;
    const TONE_HZ: u32 = 440;
    const SAMPLES: u32 = 320;
    const HALF_PERIOD: u32 = RATE / (TONE_HZ * 2);
    let mut frame = Vec::new();
    for sample in 0..SAMPLES {
        let phase = (start_sample + sample) / HALF_PERIOD;
        let amplitude: i16 = if phase.is_multiple_of(2) {
            9_000
        } else {
            -9_000
        };
        frame.extend_from_slice(&amplitude.to_le_bytes());
    }
    frame
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
    /// Seconds of 440 Hz tone to push after the arm; 0 = no talk burst.
    talk: u64,
    aes_mode: AesMode,
    hold_beat: bool,
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
            talk: 0,
            aes_mode: AesMode::CtrCont,
            hold_beat: false,
        };
        let mut items = std::env::args().skip(1);
        while let Some(flag) = items.next() {
            if flag == "--hold-beat" {
                args.hold_beat = true;
                continue;
            }
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
                "--talk" => args.talk = value.parse()?,
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
