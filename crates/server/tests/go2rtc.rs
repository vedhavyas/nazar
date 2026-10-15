//! go2rtc orchestration: the generated config, the spawned binary, and the
//! loopback API.
//!
//! Fixture provenance: the streams shape is measured against go2rtc 1.9.14
//! running locally (2026-10-15) - a stream object holds `producers` (each
//! `{url}`) and `consumers`. The empty-producers entry is shaped; the
//! measured case is producers-present.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::extract::Query;
use axum::http::StatusCode;
use axum::routing::put;
use tokio::net::TcpListener;

use server::config::Config;
use server::go2rtc::{Go2rtc, Go2rtcError, StreamStatus, live_source};

const ARGS_PATH: &str = "/tmp/nazar-fake-go2rtc-args.txt";

/// A fake go2rtc API recording the stream PUTs it receives.
struct FakeApi {
    base: reqwest::Url,
    puts: Arc<Mutex<Vec<(String, String)>>>,
}

async fn fake_api() -> FakeApi {
    fake_api_with_put(StatusCode::OK).await
}

async fn fake_api_with_put(status: StatusCode) -> FakeApi {
    let puts = Arc::new(Mutex::new(Vec::new()));
    let recorded = Arc::clone(&puts);
    let app = Router::new().route(
        "/api/streams",
        put(move |Query(params): Query<HashMap<String, String>>| {
            let recorded = Arc::clone(&recorded);
            async move {
                recorded.lock().unwrap().push((
                    params.get("name").cloned().unwrap_or_default(),
                    params.get("src").cloned().unwrap_or_default(),
                ));
                let body = if status.is_success() {
                    String::new()
                } else {
                    "streams: source from insecure producer".to_owned()
                };
                (status, body)
            }
        })
        .get(|| async { include_str!("fixtures/streams.json") }),
    );
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("an ephemeral port");
    let addr = listener.local_addr().expect("bound address");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("the fake serves");
    });
    FakeApi {
        base: format!("http://{addr}").parse().expect("a url"),
        puts,
    }
}

#[test]
fn live_source_references_the_config_template() {
    assert_eq!(
        live_source("rtsps://wowza2.example/hero-iot-live/x_CAMERA_VOD0?token=abc"),
        "ffmpeg:rtsps://wowza2.example/hero-iot-live/x_CAMERA_VOD0?token=abc#input=nazar#video=h264#audio=aac",
        "the source names the config's ffmpeg template and asks for H.264"
    );
}

#[tokio::test]
async fn put_stream_sends_the_name_and_source() {
    let api = fake_api().await;
    let go2rtc = Go2rtc::connect(api.base.clone());

    go2rtc
        .put_stream(
            "hall",
            "ffmpeg:rtsps://x/y#input=nazar#video=h264#audio=aac",
        )
        .await
        .expect("the fake accepts the stream");

    let puts = api.puts.lock().unwrap().clone();
    assert_eq!(
        puts,
        vec![(
            "hall".to_owned(),
            "ffmpeg:rtsps://x/y#input=nazar#video=h264#audio=aac".to_owned()
        )],
        "the PUT carries the stream name and source"
    );
}

#[tokio::test]
async fn put_stream_reports_a_rejection() {
    let api = fake_api_with_put(StatusCode::BAD_REQUEST).await;
    let go2rtc = Go2rtc::connect(api.base.clone());

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
    let api = fake_api().await;
    let go2rtc = Go2rtc::connect(api.base.clone());

    let streams = go2rtc.streams().await.expect("the fake answers");

    assert_eq!(
        streams,
        vec![
            StreamStatus {
                name: "gate".to_owned(),
                live: false,
            },
            StreamStatus {
                name: "hall".to_owned(),
                live: true,
            },
        ],
        "one status per stream, sorted by name, live when a producer is held"
    );
}

#[tokio::test]
async fn start_writes_the_config_and_runs_the_binary_with_it() {
    let _ = std::fs::remove_file(ARGS_PATH);
    let data_dir = temp_dir();
    let cfg = Config {
        data_dir: data_dir.clone(),
        go2rtc_bin: fake_bin(),
        ffmpeg_bin: PathBuf::from("/usr/bin/ffmpeg"),
    };

    let mut go2rtc = Go2rtc::start(&cfg).expect("the fake binary spawns");
    assert_eq!(
        go2rtc.config_path(),
        data_dir.join("go2rtc.yaml"),
        "the generated config lives in the data dir"
    );

    let config = std::fs::read_to_string(go2rtc.config_path()).expect("the config is written");
    assert!(
        config.contains("listen: \"127.0.0.1:1984\""),
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

    let argv = await_args(Path::new(ARGS_PATH)).await;
    assert_eq!(
        argv,
        vec![
            "--config".to_owned(),
            go2rtc.config_path().display().to_string()
        ],
        "the binary is launched with the generated config"
    );

    go2rtc.stop().await.expect("stop kills the fake binary");
    let _ = std::fs::remove_file(ARGS_PATH);
}

async fn await_args(path: &Path) -> Vec<String> {
    for _ in 0..40 {
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

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("nazar-go2rtc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}
