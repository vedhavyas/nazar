//! go2rtc orchestration: the generated config, the spawned binary, and the
//! loopback API.
//!
//! Fixture provenance: the streams shape is measured against go2rtc 1.9.14
//! running locally (2026-10-15) - a stream object holds `producers` (each
//! `{url}`) and `consumers`. The empty-producers entry is shaped; the
//! measured case is producers-present.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use qubo::ServiceProvider;
use server::config::Config;
use server::go2rtc::{Go2rtc, Go2rtcError, StreamStatus, live_source, low_source};
use server::supervisor::StreamHold;

#[test]
fn live_source_references_the_config_template() {
    assert_eq!(
        live_source("rtsps://wowza2.example/hero-iot-live/x_CAMERA_VOD0?token=abc"),
        "ffmpeg:rtsps://wowza2.example/hero-iot-live/x_CAMERA_VOD0?token=abc#input=nazar#video=h264#audio=aac",
        "the source names the config's ffmpeg template and asks for H.264"
    );
}

#[test]
fn low_source_chains_off_the_high_stream_by_name() {
    assert_eq!(
        low_source("cam-1-high"),
        "ffmpeg:cam-1-high#input=nazar#video=h264#width=640#audio=copy",
        "the tile rides the high stream inside go2rtc: no second mint, one cloud consumer"
    );
}

#[tokio::test]
async fn put_stream_sends_the_name_and_source() {
    let api = common::FakeApi::start().await;
    let go2rtc = Go2rtc::connect(api.base.clone()).expect("a client");

    go2rtc
        .put_stream(
            "hall",
            "ffmpeg:rtsps://x/y#input=nazar#video=h264#audio=aac",
        )
        .await
        .expect("the fake accepts the stream");

    assert_eq!(
        api.puts(),
        vec![(
            "hall".to_owned(),
            "ffmpeg:rtsps://x/y#input=nazar#video=h264#audio=aac".to_owned()
        )],
        "the PUT carries the stream name and source"
    );
}

#[tokio::test]
async fn put_stream_reports_a_rejection() {
    let api = common::FakeApi::start().await;
    api.set_put_status(400);
    let go2rtc = Go2rtc::connect(api.base.clone()).expect("a client");

    let err = go2rtc
        .put_stream("hall", "exec:whatever")
        .await
        .expect_err("a rejected put is an error");

    match err {
        Go2rtcError::Rejected { status, body } => {
            assert_eq!(status, 400, "the status is kept");
            assert!(
                body.contains("insecure producer"),
                "go2rtc's answer is kept: {body}"
            );
        }
        other => panic!("expected Rejected, got {other:?}"),
    }
}

#[tokio::test]
async fn streams_reports_a_producer_as_live() {
    let api = common::FakeApi::start().await;
    let go2rtc = Go2rtc::connect(api.base.clone()).expect("a client");

    let streams = go2rtc.streams().await.expect("the fake answers");

    assert_eq!(
        streams,
        vec![
            StreamStatus {
                name: "gate".to_owned(),
                live: false,
                connected: false,
                consumers: 0,
            },
            StreamStatus {
                name: "hall".to_owned(),
                live: true,
                // The measured fixture's producer carries no `remote_addr`:
                // held, but not shown connected.
                connected: false,
                consumers: 0,
            },
        ],
        "one status per stream, sorted by name, live when a producer is held"
    );
}

/// A go2rtc that accepts the connection and then never answers must not
/// hold a caller: every call is bounded by the client's own timeout, so a
/// wedged API is an error inside a known window, not a hang until the
/// process dies - the supervisor's state lock rides on these calls.
#[tokio::test]
async fn a_go2rtc_that_never_answers_is_bounded() {
    use tokio::io::AsyncReadExt as _;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("a port");
    let addr = listener.local_addr().expect("the address");
    tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.expect("a connection");
        let mut buf = [0_u8; 4096];
        // Consume the request, then hold the socket open in silence.
        let _ = socket.read(&mut buf).await;
        std::future::pending::<()>().await;
    });

    let go2rtc =
        Go2rtc::connect(format!("http://{addr}").parse().expect("a url")).expect("a client");
    let started = std::time::Instant::now();
    let answer = tokio::time::timeout(Duration::from_secs(8), go2rtc.streams())
        .await
        .expect("the call is bounded by the client's own timeout, not by this test");
    assert!(answer.is_err(), "a wedged go2rtc is an error, not a hang");
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "and it returns inside the bound: {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn start_writes_the_config_and_runs_the_binary_with_it() {
    let data_dir = temp_dir();
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: fake_bin(),
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: Some("198.51.100.1:8555".to_owned()),
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
    };

    let mut go2rtc = Go2rtc::start(&cfg).expect("the fake binary spawns");
    assert_eq!(
        go2rtc.config_path(),
        data_dir.join("go2rtc.yaml"),
        "the generated config lives in the data dir"
    );

    let config = std::fs::read_to_string(go2rtc.config_path()).expect("the config is written");
    assert_eq!(
        listen_of(&config, "api").as_deref(),
        Some("127.0.0.1:1984"),
        "the API binds loopback only"
    );
    assert!(
        config.contains("/usr/bin/ffmpeg"),
        "the config carries the ffmpeg binary"
    );
    assert!(
        config.contains("-rtsp_transport tcp") && config.contains("-fps_mode cfr"),
        "the template keeps the spike's transport and ffmpeg-9 fps flags"
    );
    assert!(
        !config.contains("-vsync"),
        "-vsync stays out: ffmpeg 9 dropped it"
    );
    assert!(
        config.contains("nazar:"),
        "the template live_source names exists"
    );
    assert!(
        config.contains("candidates:") && config.contains("198.51.100.1:8555"),
        "the configured WebRTC candidate reaches the generated config"
    );
    assert_eq!(
        listen_of(&config, "rtsp").as_deref(),
        Some("127.0.0.1:8554"),
        "the chain's rtsp listener is pinned to loopback: go2rtc's default exposes :8554 on every interface"
    );

    let argv = await_args(&args_path(go2rtc.config_path())).await;
    assert_eq!(
        argv,
        vec![
            "--config".to_owned(),
            go2rtc.config_path().display().to_string()
        ],
        "the binary is launched with the generated config"
    );

    go2rtc.stop().await.expect("stop kills the fake binary");
}

#[tokio::test]
async fn no_webrtc_section_without_a_configured_candidate() {
    let data_dir = temp_dir();
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: fake_bin(),
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
    };

    let mut go2rtc = Go2rtc::start(&cfg).expect("the fake binary spawns");
    let config = std::fs::read_to_string(go2rtc.config_path()).expect("the config is written");
    assert!(
        !config.contains("webrtc:"),
        "go2rtc's own discovery stands when no candidate is configured: {config}"
    );

    go2rtc.stop().await.expect("stop kills the fake binary");
}

/// The `listen:` value under a top-level `section:`, as the generated
/// config carries it. go2rtc reads the section, not the text: a commented
/// or misplaced pair must not count as a listener.
fn listen_of(config: &str, section: &str) -> Option<String> {
    let mut inside = false;
    for line in config.lines() {
        if line == format!("{section}:") {
            inside = true;
            continue;
        }
        if !inside || line.starts_with('#') || line.is_empty() {
            continue;
        }
        if !line.starts_with(' ') {
            return None; // the section ended without a listener
        }
        if let Some(value) = line.trim().strip_prefix("listen:") {
            return Some(value.trim().trim_matches('"').to_owned());
        }
    }
    None
}

/// Where the fake binary records its argv: beside its own config, so
/// concurrent tests cannot read each other's.
fn args_path(config_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.args", config_path.display()))
}

async fn await_args(path: &Path) -> Vec<String> {
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(path) {
            return text.lines().map(str::to_owned).collect();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the fake binary never recorded its argv");
}

fn fake_bin() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/fake-go2rtc.sh")
}

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nazar-go2rtc-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

/// The ffmpeg hold's own lifecycle: it starts on the stream's loopback
/// target, and releasing the lease kills the child.
#[tokio::test]
async fn the_ffmpeg_hold_targets_the_loopback_stream_and_release_kills_it() {
    let dir = temp_dir();
    let bin = dir.join("fake-ffmpeg.sh");
    std::fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$(dirname \"$0\")/ffmpeg.args\"\nprintf '%s' \"$$\" > \"$(dirname \"$0\")/ffmpeg.pid\"\nexec sleep 60\n",
    )
    .expect("the stand-in writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in is executable");
    }

    let hold = server::go2rtc::FfmpegHold::new(bin);
    let lease = hold.hold("hall-high");

    let args_path = dir.join("ffmpeg.args");
    // The window clears the spawn with slack: a first spawn under load can
    // be slow, and the hold's retry (5s doubling per failure) is what then
    // writes the args.
    let mut args = String::new();
    for _ in 0..600 {
        if let Ok(text) = std::fs::read_to_string(&args_path)
            && !text.is_empty()
        {
            args = text;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let args: Vec<&str> = args.lines().collect();
    let target = args.iter().position(|a| *a == "-i").map(|i| args[i + 1]);
    assert_eq!(
        target,
        Some("rtsp://127.0.0.1:8554/hall-high"),
        "the hold reads the stream over the loopback rtsp listener: {args:?}"
    );

    let pid_path = dir.join("ffmpeg.pid");
    let mut pid = String::new();
    for _ in 0..100 {
        if let Ok(text) = std::fs::read_to_string(&pid_path)
            && !text.is_empty()
        {
            pid = text;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!pid.is_empty(), "the stand-in recorded its pid");

    // The attach flag: true once the child outlives its startup (the
    // subtraction counts registered consumers, not spawned processes),
    // and false again the moment the child is gone.
    let mut attached = false;
    for _ in 0..150 {
        if lease.attached() {
            attached = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(attached, "the hold reports attached once its child is up");
    let killed = std::process::Command::new("kill")
        .args([&pid])
        .status()
        .expect("kill runs");
    assert!(killed.success(), "the child was killed for the flag's fall");
    let mut fallen = false;
    for _ in 0..150 {
        if !lease.attached() {
            fallen = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(fallen, "and reads unattached once the child is gone");

    drop(lease);

    // The drop-kill property needs a child that is ALIVE when the lease
    // drops: the manual kill above already proved the flag falls, so this
    // runs on a second, fresh lease with its own child.
    let second = hold.hold("hall-high");
    let mut next = String::new();
    for _ in 0..300 {
        if let Ok(text) = std::fs::read_to_string(&pid_path)
            && !text.is_empty()
            && text != pid
        {
            next = text;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!next.is_empty(), "the fresh lease spawned its own child");
    drop(second);
    assert!(
        gone_within(&next, 300).await,
        "releasing the lease kills the hold's child"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

/// Whether `pid` is gone within `tries` polls of 20ms: dead, or a zombie
/// awaiting reaping.
async fn gone_within(pid: &str, tries: usize) -> bool {
    for _ in 0..tries {
        let status = std::process::Command::new("kill")
            .args(["-0", pid])
            .status()
            .expect("kill runs");
        let state = std::process::Command::new("ps")
            .args(["-o", "state=", "-p", pid])
            .output()
            .expect("ps runs");
        if !status.success() || String::from_utf8_lossy(&state.stdout).contains('Z') {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    false
}

/// A hold whose child dies comes back: the camera's blip must not leave
/// the stream cold for good.
#[tokio::test]
async fn the_ffmpeg_hold_restarts_after_the_child_dies() {
    let dir = temp_dir();
    let bin = dir.join("dead-ffmpeg.sh");
    // A young life, short of the attach grace but long against the poll:
    // a child that exits in milliseconds would hide a wrongly-set flag.
    std::fs::write(
        &bin,
        "#!/bin/sh\nprintf 'x' >> \"$(dirname \"$0\")/runs.log\"\nsleep 0.4\nexit 0\n",
    )
    .expect("the stand-in writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in is executable");
    }

    let hold = server::go2rtc::FfmpegHold::new(bin);
    let lease = hold.hold("hall-high");

    let runs_path = dir.join("runs.log");
    // Two runs means the retry landed: the first respawn waits the base
    // doubled once (5s -> 10s), so the window clears it with wide slack
    // under load.
    let mut restarted = false;
    for _ in 0..1200 {
        if let Ok(text) = std::fs::read_to_string(&runs_path)
            && text.len() >= 2
        {
            restarted = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(
        restarted,
        "the hold re-runs its ffmpeg after the child exits: {:?}",
        std::fs::read_to_string(&runs_path)
    );
    assert!(
        !lease.attached(),
        "a child that keeps dying young never reads attached"
    );
    drop(lease);
    let _ = std::fs::remove_dir_all(&dir);
}
