//! Runtime configuration.

use std::path::PathBuf;

use qubo::ServiceProvider;

/// Everything the server needs from its environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Durable state: the Qubo session, the PIN, the generated go2rtc
    /// config.
    pub data_dir: PathBuf,
    /// The go2rtc binary to run.
    pub go2rtc_bin: PathBuf,
    /// The ffmpeg binary go2rtc's sources run.
    pub ffmpeg_bin: PathBuf,
    /// The Qubo service provider.
    pub service_provider: ServiceProvider,
}

impl Config {
    /// Read the environment: `DATA_DIR` (default `/data`), `GO2RTC_BIN`
    /// and `FFMPEG_BIN` (default to PATH lookups), `QUBO_SP`.
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
        }
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}
