//! Runtime configuration.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use qubo::ServiceProvider;

/// Everything the server needs from its environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Durable state: the Qubo session and the generated go2rtc config.
    pub data_dir: PathBuf,
    /// The go2rtc binary to run.
    pub go2rtc_bin: PathBuf,
    /// The ffmpeg binary go2rtc's sources run.
    pub ffmpeg_bin: PathBuf,
    /// The Qubo service provider.
    pub service_provider: ServiceProvider,
    /// One WebRTC candidate viewers can reach, `ip:port`. Empty means
    /// go2rtc's own discovery, which finds container-internal addresses.
    pub webrtc_candidates: Option<String>,
    /// Cameras that try the P2P fast lane before the cloud mint
    /// (`NAZAR_PREFER_P2P`, comma-separated device ids).
    pub prefer_p2p: Vec<String>,
    /// The app-side identity the fast lane borrows: the unit's `mobile`
    /// device and the Android-ID-style `srcDeviceId`. The defaults are the
    /// captured phone's own (`NAZAR_P2P_SELF_DEVICE`, `NAZAR_P2P_SRC_DEVICE_ID`).
    pub p2p_self_device: String,
    pub p2p_src_device_id: String,
    /// The camera's controllerUUID for the device-access-token dest
    /// (`NAZAR_P2P_CAMERA_CONTROLLER`); `None` reads it from the device
    /// listing.
    pub p2p_camera_controller: Option<String>,
}

impl Config {
    /// Read the environment: `DATA_DIR` (default `/data`), `GO2RTC_BIN`
    /// and `FFMPEG_BIN` (default to PATH lookups), `QUBO_SP`,
    /// `NAZAR_WEBRTC_CANDIDATES`, and the fast-lane set
    /// (`NAZAR_PREFER_P2P`, `NAZAR_P2P_*`).
    pub fn from_env() -> Self {
        Self {
            data_dir: var("DATA_DIR").unwrap_or_else(|| "/data".to_owned()).into(),
            go2rtc_bin: var("GO2RTC_BIN")
                .unwrap_or_else(|| "go2rtc".to_owned())
                .into(),
            ffmpeg_bin: var("FFMPEG_BIN")
                .unwrap_or_else(|| "ffmpeg".to_owned())
                .into(),
            service_provider: var("QUBO_SP")
                .map_or_else(ServiceProvider::default, ServiceProvider::new),
            webrtc_candidates: var("NAZAR_WEBRTC_CANDIDATES"),
            prefer_p2p: var("NAZAR_PREFER_P2P").map_or_else(Vec::new, |list| {
                list.split(',')
                    .map(str::trim)
                    .filter(|item| !item.is_empty())
                    .map(str::to_owned)
                    .collect()
            }),
            p2p_self_device: var("NAZAR_P2P_SELF_DEVICE")
                .unwrap_or_else(|| "d0000000-0000-4000-8000-000000000002".to_owned()),
            p2p_src_device_id: var("NAZAR_P2P_SRC_DEVICE_ID")
                .unwrap_or_else(|| "<mobile-id>".to_owned()),
            p2p_camera_controller: var("NAZAR_P2P_CAMERA_CONTROLLER"),
        }
    }

    /// The account's `Source-Device-Id`: created once under the data dir
    /// (0600) and reused, so restarts do not add device rows to the real
    /// account. An unwritable data dir falls back to an ephemeral id.
    pub fn device_id(&self) -> String {
        let path = self.data_dir.join("device_id");
        if let Ok(stored) = std::fs::read_to_string(&path) {
            let stored = stored.trim();
            if !stored.is_empty() {
                return stored.to_owned();
            }
        }
        let fresh = uuid::Uuid::new_v4().to_string();
        let written = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(fresh.as_bytes()));
        if let Err(err) = written {
            tracing::warn!(%err, path = %path.display(), "could not persist the device id");
        }
        fresh
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
