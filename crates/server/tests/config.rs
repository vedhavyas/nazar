//! Configuration helpers: the persisted device id.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::os::unix::fs::PermissionsExt;

use server::config::Config;

fn config_in(dir: &std::path::Path) -> Config {
    Config {
        data_dir: dir.to_owned(),
        go2rtc_bin: "go2rtc".into(),
        ffmpeg_bin: "ffmpeg".into(),
        service_provider: qubo::ServiceProvider::default(),
        webrtc_candidates: None,
        aqi_alert_at: server::notify::AQI_ALERT_AT,
        episodes_retention_days: 30,
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
    }
}

/// The environment lines' parse-and-warn branches, through the pure seams -
/// the workspace forbids `unsafe`, so tests never touch the process
/// environment.
#[test]
fn the_config_lines_refuse_bad_values_and_keep_their_defaults() {
    use server::config::{EPISODE_RETENTION_DAYS, aqi_line, retention_days};

    assert_eq!(
        aqi_line(Some("garbage")),
        server::notify::AQI_ALERT_AT,
        "a line that is not a number keeps the default line"
    );
    assert_eq!(aqi_line(Some("150")), 150, "a number is honored");
    assert_eq!(
        aqi_line(None),
        server::notify::AQI_ALERT_AT,
        "absence keeps the default"
    );

    assert_eq!(
        retention_days(Some("garbage")),
        EPISODE_RETENTION_DAYS,
        "a line that is not a number keeps the default window"
    );
    assert_eq!(
        retention_days(Some("0")),
        EPISODE_RETENTION_DAYS,
        "zero days would silently disable the timeline, so it keeps the default"
    );
    assert_eq!(retention_days(Some("7")), 7, "a window is honored");
    assert_eq!(retention_days(None), EPISODE_RETENTION_DAYS);
}

#[test]
fn the_device_id_persists_across_calls_with_owner_only_permissions() {
    let dir = std::env::temp_dir().join(format!("nazar-device-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("data dir");
    let _ = std::fs::remove_file(dir.join("device_id"));

    let first = config_in(&dir).device_id();
    let again = config_in(&dir).device_id();
    assert_eq!(first, again, "the id is stable across restarts");

    let mode = std::fs::metadata(dir.join("device_id"))
        .expect("the id file exists")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "the id file is owner-only");

    let _ = std::fs::remove_dir_all(&dir);
}
