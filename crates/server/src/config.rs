//! Runtime configuration.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;
use std::time::Duration;

use qubo::ServiceProvider;

use crate::notify::AQI_ALERT_AT;

/// How many days of motion episodes the timeline keeps by default.
pub const EPISODE_RETENTION_DAYS: u32 = 30;

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
    /// The PM2.5 reading worth a notification.
    pub aqi_alert_at: u32,
    /// How many days of motion episodes the recording timeline keeps.
    pub episodes_retention_days: u32,
}

impl Config {
    /// Read the environment: `DATA_DIR` (default `/data`), `GO2RTC_BIN`
    /// and `FFMPEG_BIN` (default to PATH lookups), `QUBO_SP`,
    /// `NAZAR_WEBRTC_CANDIDATES`, `NAZAR_AQI_ALERT_AT`,
    /// `NAZAR_EPISODE_RETENTION_DAYS`.
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
            aqi_alert_at: aqi_line(var("NAZAR_AQI_ALERT_AT").as_deref()),
            episodes_retention_days: retention_days(var("NAZAR_EPISODE_RETENTION_DAYS").as_deref()),
        }
    }

    /// The timeline's retention as a duration.
    pub fn episodes_retention(&self) -> Duration {
        Duration::from_secs(u64::from(self.episodes_retention_days) * 86_400)
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

/// The alert line from its environment value: anything that is not a number
/// keeps the default. Pure, so the branch is testable without the
/// environment (which the workspace's `unsafe_code = "forbid"` bars tests
/// from touching).
pub fn aqi_line(raw: Option<&str>) -> u32 {
    raw.map_or(AQI_ALERT_AT, |raw| {
        raw.parse().unwrap_or_else(|_| {
            tracing::warn!(
                value = %raw,
                "NAZAR_AQI_ALERT_AT is not a number; keeping the default line"
            );
            AQI_ALERT_AT
        })
    })
}

/// The timeline's retention from its environment value: anything that is
/// not a number keeps the default.
pub fn retention_days(raw: Option<&str>) -> u32 {
    raw.map_or(EPISODE_RETENTION_DAYS, |raw| {
        raw.parse().unwrap_or_else(|_| {
            tracing::warn!(
                value = %raw,
                "NAZAR_EPISODE_RETENTION_DAYS is not a number; keeping the default"
            );
            EPISODE_RETENTION_DAYS
        })
    })
}
