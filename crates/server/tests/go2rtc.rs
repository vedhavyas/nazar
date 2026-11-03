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
        "ffmpeg:rtsps://wowza2.example/hero-iot-live/x_CAMERA_VOD0?token=abc#input=nazar#video=nazar-h264#audio=opus#audio=aac#starttimeout=60",
        "the source names the config's ffmpeg template, selects the one-second-keyframe \
         preset, carries both audio codecs (opus for WebRTC, aac for HLS - a browser \
         cannot decode aac inside WebRTC, and the AAC-only answer SDP showed a dead \
         audio m-line), and keeps the churn-past start window"
    );
}

#[test]
fn low_source_chains_off_the_high_stream_by_name() {
    assert_eq!(
        low_source("cam-1-high"),
        "ffmpeg:cam-1-high#input=nazar#video=nazar-h264#width=640#audio=opus#audio=aac#starttimeout=60",
        "the tile rides the high stream inside go2rtc - no second mint, one cloud consumer - \
         with a one-second-keyframe preset, both audio codecs (the tile's WebRTC needs \
         opus too), and a start window past the churn"
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
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
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
        config.contains("nazar-h264:") && config.contains("-g 10"),
        "the one-second-keyframe preset exists for both sources: a served stream's first \
         frame waits at most one IDR interval"
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

/// A go2rtc child that exits must come back on its own: the server owns
/// the process, and one death must not take the house down until someone
/// restarts the container. The config is rewritten before each respawn,
/// so a truncated config is replaced before the next spawn.
#[tokio::test]
async fn a_child_that_exits_is_respawned_with_a_written_config() {
    let data_dir = temp_dir();
    let bin = data_dir.join("exiting-go2rtc.sh");
    std::fs::write(
        &bin,
        "#!/bin/sh\n[ -s \"$2\" ] || exit 1\nprintf 'run:%s\\n' \"$*\" >> \"$(dirname \"$0\")/runs.log\"\nexec sleep 1\n",
    )
    .expect("the stand-in writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in is executable");
    }
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: bin,
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
    };
    let mut go2rtc = Go2rtc::start(&cfg).expect("the fake binary spawns");
    let runs = data_dir.join("runs.log");
    let expected_argv = format!("run:--config {}", go2rtc.config_path().display());

    assert!(
        await_runs(&runs, 1, SPAWN_WINDOW).await,
        "the first run lands"
    );
    assert_eq!(
        run_lines(&runs).first(),
        Some(&expected_argv),
        "a run is counted only when the config is already non-empty, and it records its argv"
    );
    assert!(
        await_runs(&runs, 2, SPAWN_WINDOW).await,
        "the dead child is respawned: {:?}",
        run_lines(&runs)
    );
    assert!(
        run_lines(&runs).iter().all(|line| line == &expected_argv),
        "every run carries the same argv: {:?}",
        run_lines(&runs)
    );

    // Truncate the config after a run; the next respawn must rewrite it
    // before spawning. Wait until the truncation is the observed state,
    // then count: the next run must be one more than this point.
    std::fs::write(go2rtc.config_path(), "").expect("the config is truncated");
    assert!(
        await_text_where(go2rtc.config_path(), str::is_empty)
            .await
            .is_some(),
        "the truncation lands before the next respawn"
    );
    let before = run_lines(&runs).len();
    assert!(
        await_runs(&runs, before + 1, SPAWN_WINDOW).await,
        "the respawn comes back after the truncation: {:?}",
        run_lines(&runs)
    );
    let config = std::fs::read_to_string(go2rtc.config_path()).expect("the config is readable");
    assert_eq!(
        listen_of(&config, "api").as_deref(),
        Some("127.0.0.1:1984"),
        "the respawn wrote the API listener first"
    );
    assert!(
        config.contains("/usr/bin/ffmpeg") && config.contains("nazar-h264:"),
        "the respawn wrote the ffmpeg template and preset: {config:?}"
    );

    go2rtc.stop().await.expect("stop ends the supervisor");
}

/// A config that cannot be written must withhold the spawn: go2rtc falls
/// back to permissive defaults (api and rtsp on every interface) when the
/// file is missing, so spawning without it loses the loopback pinning.
#[cfg(unix)]
#[tokio::test]
async fn a_failed_config_write_withholds_the_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let uid = std::process::Command::new("id")
        .arg("-u")
        .output()
        .expect("id runs");
    if String::from_utf8_lossy(&uid.stdout).trim() == "0" {
        return; // root ignores the directory mode bits
    }
    let data_dir = temp_dir();
    let marker_dir = temp_dir();
    let runs = marker_dir.join("runs.log");
    let bin = data_dir.join("respawns-go2rtc.sh");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\nprintf 'run\\n' >> \"{}\"\nexec sleep 1\n",
            runs.display()
        ),
    )
    .expect("the stand-in writes");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .expect("the stand-in is executable");
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: bin,
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
    };
    let mut go2rtc = Go2rtc::start(&cfg).expect("the fake binary spawns");
    assert!(
        await_runs(&runs, 1, SPAWN_WINDOW).await,
        "the first run lands"
    );

    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o555))
        .expect("the data dir turns read-only");
    tokio::time::sleep(Duration::from_secs(3)).await;
    assert_eq!(
        run_lines(&runs).len(),
        1,
        "no spawn while the config cannot be written: {:?}",
        run_lines(&runs)
    );
    std::fs::set_permissions(&data_dir, std::fs::Permissions::from_mode(0o755))
        .expect("the data dir turns writable");
    assert!(
        await_runs(&runs, 2, SPAWN_WINDOW).await,
        "the spawn returns once the config can be written: {:?}",
        run_lines(&runs)
    );
    go2rtc.stop().await.expect("stop ends the supervisor");
}

/// Dropping the handle without stop() must not orphan the child: the
/// supervisor sees its channel close, kills the child and reaps it.
#[tokio::test]
async fn dropping_the_handle_kills_the_child() {
    let data_dir = temp_dir();
    let bin = data_dir.join("pid-go2rtc.sh");
    std::fs::write(
        &bin,
        "#!/bin/sh\nprintf '%s' \"$$\" > \"$(dirname \"$0\")/go2rtc.pid\"\nexec sleep 60\n",
    )
    .expect("the stand-in writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in is executable");
    }
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: bin,
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
    };
    let go2rtc = Go2rtc::start(&cfg).expect("the fake binary spawns");
    let pid_path = data_dir.join("go2rtc.pid");
    let pid = await_text(&pid_path)
        .await
        .expect("the pid lands")
        .trim()
        .to_owned();
    assert!(!pid.is_empty(), "the stand-in recorded its pid");

    drop(go2rtc);
    let mut gone = false;
    for _ in 0..100 {
        if !process_alive(&pid) {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        gone,
        "the child dies with the handle, no stop needed: pid {pid}"
    );
}

fn process_alive(pid: &str) -> bool {
    std::process::Command::new("kill")
        .arg("-0")
        .arg(pid)
        .status()
        .is_ok_and(|status| status.success())
}

/// A binary that cannot spawn is a boot failure: the `?` on start in main
/// must see it, not a server warning at a dead engine forever.
#[tokio::test]
async fn a_first_spawn_that_fails_is_an_error() {
    let data_dir = temp_dir();
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: data_dir.join("missing-go2rtc"),
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
    };
    match Go2rtc::start(&cfg) {
        Err(Go2rtcError::Io(_)) => {}
        Err(other) => panic!("a missing go2rtc must fail with its io error: {other:?}"),
        Ok(_) => panic!("a missing go2rtc must fail the boot"),
    }
}

/// A child that execs and exits at once (a wrapper rejecting argv, a
/// broken binary) must fail the boot too, not leave the supervisor
/// restarting a corpse while health stays green. An exit-0 child is just
/// as useless.
#[tokio::test]
async fn a_first_child_that_exits_at_once_is_a_boot_failure() {
    let config_with = |bin: PathBuf| Config {
        data_dir: temp_dir(),
        // A real binary that execs and exits at once; a shell wrapper adds
        // startup latency that is not the case under test.
        go2rtc_bin: bin,
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
    };
    let false_bin = PathBuf::from(if cfg!(target_os = "macos") {
        "/usr/bin/false"
    } else {
        "/bin/false"
    });
    match Go2rtc::start(&config_with(false_bin)) {
        Err(Go2rtcError::StartupExit(status)) => {
            assert!(
                !status.success(),
                "the boot failure carries the exit status"
            );
        }
        Err(other) => panic!("a child that exits at boot must fail with its status: {other:?}"),
        Ok(_) => panic!("a child that exits at boot must fail the boot"),
    }
    let true_bin = PathBuf::from(if cfg!(target_os = "macos") {
        "/usr/bin/true"
    } else {
        "/bin/true"
    });
    match Go2rtc::start(&config_with(true_bin)) {
        Err(Go2rtcError::StartupExit(status)) => {
            assert!(
                status.success(),
                "an exit-0 child inside the grace is still fatal"
            );
        }
        Err(other) => panic!("an exit-0 child at boot must fail with its status: {other:?}"),
        Ok(_) => panic!("an exit-0 child inside the grace must fail the boot"),
    }
}

/// A respawn that cannot spawn is retried, not abandoned: the stand-in
/// deletes its own script and exits, the first retry fails, and the run
/// returns once the script is restored.
#[tokio::test]
async fn a_spawn_failure_after_an_exit_is_retried() {
    let data_dir = temp_dir();
    let marker_dir = temp_dir();
    let runs = marker_dir.join("runs.log");
    let bin = data_dir.join("vanishing-go2rtc.sh");
    let script = format!(
        "#!/bin/sh\n[ -s \"$2\" ] || exit 1\nprintf 'run:%s\\n' \"$*\" >> \"{}\"\nrm -f \"$0\"\nexec sleep 0.5\n",
        runs.display()
    );
    std::fs::write(&bin, &script).expect("the stand-in writes");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in is executable");
    }
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: bin.clone(),
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
        service_provider: ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
    };
    let mut go2rtc = Go2rtc::start(&cfg).expect("the fake binary spawns");
    assert!(
        await_runs(&runs, 1, SPAWN_WINDOW).await,
        "the first run lands"
    );
    // The child exits at ~0.5s and deletes its script; wait past the first
    // 1s retry, then give the supervisor a binary again.
    tokio::time::sleep(Duration::from_secs(2)).await;
    std::fs::write(&bin, &script).expect("the stand-in is restored");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
            .expect("the stand-in is executable again");
    }
    assert!(
        await_runs(&runs, 2, SPAWN_WINDOW).await,
        "the retry lands once the binary is back: {:?}",
        run_lines(&runs)
    );
    go2rtc.stop().await.expect("stop ends the supervisor");
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
        p2p_login_device_name: "MobileApp_test".to_owned(),
        p2p_arm_timeout: std::time::Duration::from_secs(15),
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

/// How long the harness waits for a fake binary to record a file. A
/// spawned shell can be slow on a box under full load; this window is
/// patience, not a product latency claim.
const SPAWN_WINDOW: Duration = Duration::from_secs(30);

/// Poll `path` until `done` accepts its text, within `SPAWN_WINDOW`.
async fn await_text_where(path: &Path, done: impl Fn(&str) -> bool) -> Option<String> {
    let deadline = tokio::time::Instant::now() + SPAWN_WINDOW;
    while tokio::time::Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path)
            && done(&text)
        {
            return Some(text);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    None
}

async fn await_text(path: &Path) -> Option<String> {
    await_text_where(path, |text| !text.is_empty()).await
}

/// The recorded runs, one marker line per spawn that saw a non-empty config.
fn run_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path).map_or_default(|text| text.lines().map(str::to_owned).collect())
}

/// Whether `path` records at least `n` runs within `budget`.
async fn await_runs(path: &Path, n: usize, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if run_lines(path).len() >= n {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn await_args(path: &Path) -> Vec<String> {
    await_text(path)
        .await
        .expect("the fake binary never recorded its argv")
        .lines()
        .map(str::to_owned)
        .collect()
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
    let args_text = await_text(&args_path)
        .await
        .expect("the stand-in recorded its args");
    let args: Vec<&str> = args_text.lines().collect();
    let target = args.iter().position(|a| *a == "-i").map(|i| args[i + 1]);
    assert_eq!(
        target,
        Some("rtsp://127.0.0.1:8554/hall-high"),
        "the hold reads the stream over the loopback rtsp listener: {args:?}"
    );

    let pid_path = dir.join("ffmpeg.pid");
    let pid = await_text(&pid_path)
        .await
        .expect("the stand-in recorded its pid");

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
    // doubled once (5s -> 10s), so SPAWN_WINDOW clears it with wide slack
    // under load.
    let restarted = await_text_where(&runs_path, |text| text.len() >= 2)
        .await
        .is_some();
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
