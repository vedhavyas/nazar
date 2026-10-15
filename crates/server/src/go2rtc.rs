//! go2rtc: the spawned process and its loopback API.
//!
//! `exec:` sources are refused by go2rtc's API (the scheme is marked
//! insecure) and API sources may not contain spaces, so the ffmpeg recipe
//! lives in the generated config as a named template that camera sources
//! reference with `#input=`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::{Child, Command};

use crate::config::Config;

/// Where the generated config puts go2rtc's API.
pub const API_BASE: &str = "http://127.0.0.1:1984";

/// The generated config's ffmpeg template name.
const TEMPLATE: &str = "nazar";

/// The spike's proven input flags. Codec and output flags are not repeated
/// here: go2rtc appends its own after the input segment and the later flag
/// wins in ffmpeg. `-fps_mode cfr` replaces `-vsync`, dropped in ffmpeg 9.
const TEMPLATE_ARGS: &str = "-rtsp_transport tcp -fflags +genpts -i {input} -fps_mode cfr -r 10";

/// A stream's presence in go2rtc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamStatus {
    pub name: String,
    /// Whether go2rtc holds a producer for the stream. Per-producer
    /// connection state is not exposed, so this is the strongest liveness
    /// signal the API gives.
    pub live: bool,
}

#[derive(Debug, Deserialize)]
struct StreamEntry {
    #[serde(default)]
    producers: Vec<Value>,
}

/// Everything that can go wrong running or calling go2rtc.
#[derive(Debug, thiserror::Error)]
pub enum Go2rtcError {
    #[error("go2rtc io: {0}")]
    Io(#[from] std::io::Error),
    #[error("go2rtc api: {0}")]
    Api(#[from] reqwest::Error),
    #[error("go2rtc url: {0}")]
    Url(String),
    #[error("go2rtc rejected the request (HTTP {status}): {body}")]
    Rejected { status: u16, body: String },
    #[error("go2rtc response: {0}")]
    Parse(#[from] serde_json::Error),
}

/// A go2rtc instance: the spawned process, when this server owns one, and
/// the API it answers on.
pub struct Go2rtc {
    api: Url,
    http: reqwest::Client,
    child: Option<Child>,
    config_path: PathBuf,
}

impl Go2rtc {
    /// Spawn the configured binary with a generated config and talk to its
    /// loopback API. The process may take a moment to listen; callers retry
    /// their first request.
    pub fn start(cfg: &Config) -> Result<Self, Go2rtcError> {
        let config_path = cfg.data_dir.join("go2rtc.yaml");
        std::fs::write(
            &config_path,
            config_yaml(&cfg.ffmpeg_bin, cfg.webrtc_candidates.as_deref()),
        )?;
        let child = Command::new(&cfg.go2rtc_bin)
            .arg("--config")
            .arg(&config_path)
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .spawn()?;
        tracing::info!(
            bin = %cfg.go2rtc_bin.display(),
            config = %config_path.display(),
            "started go2rtc"
        );
        Ok(Self {
            api: parse_url(API_BASE)?,
            http: reqwest::Client::new(),
            child: Some(child),
            config_path,
        })
    }

    /// Attach to a go2rtc API already listening at `api_base`.
    pub fn connect(api_base: Url) -> Self {
        Self {
            api: api_base,
            http: reqwest::Client::new(),
            child: None,
            config_path: PathBuf::new(),
        }
    }

    /// The generated config's path, when this instance spawned go2rtc.
    pub fn config_path(&self) -> &Path {
        &self.config_path
    }

    /// Add or replace the stream `name`'s source.
    pub async fn put_stream(&self, name: &str, source: &str) -> Result<(), Go2rtcError> {
        let response = self
            .http
            .put(self.api_for("/api/streams")?)
            .query(&[("name", name), ("src", source)])
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Go2rtcError::Rejected {
                status: status.as_u16(),
                body: snippet(&body),
            });
        }
        tracing::debug!(stream = name, "go2rtc stream set");
        Ok(())
    }

    /// Drop the stream `name` and the producer behind it. go2rtc's own UI
    /// deletes with `?src=`, so that is the shape its API takes.
    pub async fn delete_stream(&self, name: &str) -> Result<(), Go2rtcError> {
        let response = self
            .http
            .delete(self.api_for("/api/streams")?)
            .query(&[("src", name)])
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(Go2rtcError::Rejected {
                status: status.as_u16(),
                body: snippet(&body),
            });
        }
        tracing::debug!(stream = name, "go2rtc stream dropped");
        Ok(())
    }

    /// Every stream go2rtc currently holds, by name.
    pub async fn streams(&self) -> Result<Vec<StreamStatus>, Go2rtcError> {
        let response = self.http.get(self.api_for("/api/streams")?).send().await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(Go2rtcError::Rejected {
                status: status.as_u16(),
                body: snippet(&text),
            });
        }
        let entries: BTreeMap<String, StreamEntry> = serde_json::from_str(&text)?;
        Ok(entries
            .into_iter()
            .map(|(name, entry)| StreamStatus {
                name,
                live: !entry.producers.is_empty(),
            })
            .collect())
    }

    /// Kill the spawned process, if this instance owns one.
    pub async fn stop(&mut self) -> Result<(), Go2rtcError> {
        let Some(child) = &mut self.child else {
            return Ok(());
        };
        child.kill().await?;
        Ok(())
    }

    fn api_for(&self, path: &str) -> Result<Url, Go2rtcError> {
        self.api
            .join(path)
            .map_err(|err| Go2rtcError::Url(err.to_string()))
    }
}

/// The go2rtc source for a minted camera URL: the config's ffmpeg template
/// carries it, transcoding the camera's HEVC to H.264 and keeping audio.
pub fn live_source(mint_url: &str) -> String {
    format!("ffmpeg:{mint_url}#input={TEMPLATE}#video=h264#audio=aac")
}

/// The generated config text.
///
/// WebRTC media flows browser-to-go2rtc directly, so the candidate an env
/// names is what mesh viewers connect to; without one go2rtc advertises
/// container-internal addresses. When direct candidates do not reach a
/// viewer, the fallback is go2rtc's `webrtc.ice_servers` pointed at the
/// hub's coturn (design §9 risk 3).
fn config_yaml(ffmpeg_bin: &Path, webrtc_candidates: Option<&str>) -> String {
    let webrtc = webrtc_candidates.map_or_else(String::new, |candidates| {
        format!("webrtc:\n  candidates:\n    - {candidates}\n")
    });
    format!(
        r#"api:
  listen: "127.0.0.1:1984"
{webrtc}ffmpeg:
  bin: "{ffmpeg}"
  {TEMPLATE}: "{TEMPLATE_ARGS}"
"#,
        ffmpeg = ffmpeg_bin.display(),
    )
}

fn parse_url(base: &str) -> Result<Url, Go2rtcError> {
    base.parse::<Url>()
        .map_err(|err| Go2rtcError::Url(err.to_string()))
}

fn snippet(body: &str) -> String {
    body.trim().chars().take(200).collect()
}
