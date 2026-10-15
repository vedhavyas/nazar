//! Runtime configuration.

use std::path::PathBuf;

/// Everything the server needs from its environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Durable state: the Qubo session, the generated go2rtc config.
    pub data_dir: PathBuf,
    /// The go2rtc binary to run.
    pub go2rtc_bin: PathBuf,
    /// The ffmpeg binary go2rtc's sources run.
    pub ffmpeg_bin: PathBuf,
}
