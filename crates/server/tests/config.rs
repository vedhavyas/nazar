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
        prefer_p2p: Vec::new(),
        p2p_self_device: "self-device".to_owned(),
        p2p_src_device_id: "src-device".to_owned(),
        p2p_camera_controller: None,
    }
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
