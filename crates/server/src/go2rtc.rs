//! go2rtc: the spawned process and its loopback API.
//!
//! `exec:` sources are refused by go2rtc's API (the scheme is marked
//! insecure) and API sources may not contain spaces, so the ffmpeg recipe
//! lives in the generated config as a named template that camera sources
//! reference with `#input=`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;
use tokio::process::{Child, Command};

use crate::config::Config;
use crate::supervisor::{HoldLease, StreamHold};

/// Where the generated config puts go2rtc's API.
pub const API_BASE: &str = "http://127.0.0.1:1984";

/// How long a go2rtc call may take before it gives up. The API is loopback
/// and answers in milliseconds; the bound exists so an API that accepts and
/// then never answers cannot hold a caller - and, through it, whatever lock
/// the caller holds - for the process's lifetime.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// The client every go2rtc call goes through. Both limits are load-bearing:
/// `a_go2rtc_that_never_answers_is_bounded` pins what removing them costs.
/// A build failure is an error, not a fallback to a client without them.
fn api_client() -> Result<reqwest::Client, Go2rtcError> {
    Ok(reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .build()?)
}

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
    /// Whether go2rtc holds a producer for the stream. The entry appears as
    /// soon as the source is set, so this alone cannot tell a connected
    /// producer from one whose source is failing.
    pub live: bool,
    /// Whether a producer has actually connected: the entry carries its
    /// peer address once the source answers, and none while it is still
    /// failing (measured on the hub 2026-10-16: a producer whose ffmpeg
    /// the camera was refusing read no `remote_addr`).
    pub connected: bool,
    /// How many viewers are attached. A watched stream is not idle,
    /// however long it has been since anyone asked this server for it.
    pub consumers: usize,
}

#[derive(Debug, Deserialize)]
struct StreamEntry {
    #[serde(default)]
    producers: Vec<Value>,
    #[serde(default)]
    consumers: Option<Vec<Value>>,
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

impl Clone for Go2rtc {
    /// A second handle on the same API, without the spawned child: whoever
    /// holds the original still owns the process.
    fn clone(&self) -> Self {
        Self {
            api: self.api.clone(),
            http: self.http.clone(),
            child: None,
            config_path: PathBuf::new(),
        }
    }
}

impl Go2rtc {
    /// Spawn the configured binary with a generated config and talk to its
    /// loopback API. The process may take a moment to listen; callers retry
    /// their first request.
    pub fn start(cfg: &Config) -> Result<Self, Go2rtcError> {
        // The client first: a build failure must not leave a spawned child
        // behind.
        let http = api_client()?;
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
            http,
            child: Some(child),
            config_path,
        })
    }

    /// Attach to a go2rtc API already listening at `api_base`.
    pub fn connect(api_base: Url) -> Result<Self, Go2rtcError> {
        Ok(Self {
            api: api_base,
            http: api_client()?,
            child: None,
            config_path: PathBuf::new(),
        })
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

    /// One JPEG frame of the stream, as go2rtc's own API serves it. Free
    /// when a producer is already running; it spawns one when none is, so
    /// callers check for a live stream first.
    pub async fn frame(&self, name: &str) -> Result<Vec<u8>, Go2rtcError> {
        let response = self
            .http
            .get(self.api_for("/api/frame.jpeg")?)
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
        Ok(response.bytes().await?.to_vec())
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
                connected: entry.producers.iter().any(|producer| {
                    producer
                        .get("remote_addr")
                        .and_then(Value::as_str)
                        .is_some_and(|addr| !addr.is_empty())
                }),
                consumers: entry.consumers.map_or(0, |consumers| consumers.len()),
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

/// The tile rendition's width: the cloud's own low profile, so a tile costs
/// what the app's low stream did.
const LOW_WIDTH: u32 = 640;

/// The window the 640 chain gets to start before go2rtc kills it. go2rtc's
/// default is 30s (`starttimeout`), and a chain whose start an in-flight
/// re-mint slowed gets killed mid-connect at churn - the hub logged those
/// kills. 60s waits a churn-slow start out; a genuinely wedged process is
/// still killed, a minute in.
const LOW_START_TIMEOUT_SECS: u32 = 60;

/// The go2rtc source for a camera's tile rendition: the high stream's own
/// name, chained and scaled inside go2rtc. The cloud stream is minted once
/// per camera (mints are last-mint-wins), so the tile re-reads it locally
/// rather than taking a second cloud consumer. Measured on go2rtc 1.9.14:
/// the chain resolves to the loopback rtsp server and the tile's scaler
/// reads the high stream from there.
pub fn low_source(high_name: &str) -> String {
    format!(
        "ffmpeg:{high_name}#input={TEMPLATE}#video=h264#width={LOW_WIDTH}#audio=copy#starttimeout={LOW_START_TIMEOUT_SECS}"
    )
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
# The tile chain re-reads streams through this listener; pinned to
# loopback because go2rtc's default binds every interface.
rtsp:
  listen: "127.0.0.1:8554"
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

/// Where the generated config's rtsp listener answers.
pub const RTSP_BASE: &str = "rtsp://127.0.0.1:8554";

/// The base delay before a hold reconnects: long enough that a camera
/// that is away is not dialled every second, short enough that a viewer
/// arriving after a failure waits seconds, not minutes. Consecutive young
/// deaths double it up to `HOLD_BACKOFF_MAX`.
const HOLD_RETRY: Duration = Duration::from_secs(5);

/// The cap on the hold's respawn backoff: an offline camera settles at
/// one dial a minute.
const HOLD_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// A hold run this long counts as healthy and resets the failure streak.
const HOLD_HEALTHY_RUN: Duration = Duration::from_secs(30);

/// How long a hold's child must outlive its startup before the consumer
/// counts as attached: a dial that dies fast (a missing stream, a broken
/// binary) never registered at go2rtc.
const HOLD_ATTACH_GRACE: Duration = Duration::from_secs(1);

/// After this many consecutive young deaths, say the diagnosis once: a
/// hold that never attaches is otherwise invisible in the retry noise.
const HOLD_ESCALATE_AT: u32 = 5;

fn hold_backoff(failures: u32) -> Duration {
    (HOLD_RETRY * 2u32.pow(failures.min(4))).min(HOLD_BACKOFF_MAX)
}

/// Drain a child's stderr to EOF on its own task, keeping the TAIL: the
/// last words name the failure, and a closed pipe would kill the child
/// with SIGPIPE on its next write.
fn drain_tail(mut stderr: tokio::process::ChildStderr) -> tokio::task::JoinHandle<Vec<u8>> {
    tokio::spawn(async move {
        use tokio::io::AsyncReadExt as _;
        let mut tail: Vec<u8> = Vec::new();
        let mut buf = [0u8; 1024];
        loop {
            match stderr.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    tail.extend_from_slice(&buf[..n]);
                    if tail.len() > 2048 {
                        let over = tail.len() - 2048;
                        tail.drain(..over);
                    }
                }
            }
        }
        tail
    })
}

/// narar's own consumer on a stream: an ffmpeg reading it over the
/// loopback rtsp listener the generated config already runs. A consumer
/// on that listener is a viewer as far as go2rtc is concerned, so the
/// producer stays up between real viewers and a client connects to a
/// flowing picture.
pub struct FfmpegHold {
    ffmpeg: PathBuf,
}

impl FfmpegHold {
    pub fn new(ffmpeg: PathBuf) -> Self {
        Self { ffmpeg }
    }
}

impl StreamHold for FfmpegHold {
    fn hold(&self, name: &str) -> Arc<dyn HoldLease> {
        let ffmpeg = self.ffmpeg.clone();
        let target = format!("{RTSP_BASE}/{name}");
        let stream = name.to_owned();
        let attached = Arc::new(AtomicBool::new(false));
        let worker = {
            let attached = Arc::clone(&attached);
            tokio::spawn(async move {
                let mut failures: u32 = 0;
                loop {
                    let started = Instant::now();
                    match Command::new(&ffmpeg)
                        .args([
                            "-nostdin",
                            "-v",
                            "error",
                            "-rtsp_transport",
                            "tcp",
                            "-i",
                            &target,
                            "-c",
                            "copy",
                            "-f",
                            "null",
                            "-",
                        ])
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true)
                        .spawn()
                    {
                        Ok(mut child) => {
                            let drain = child.stderr.take().map(drain_tail);
                            // Attached means the consumer REGISTERED, not
                            // that a process was spawned: a child that
                            // dies inside the startup grace never counted
                            // at go2rtc and must not be subtracted.
                            let status = if let Ok(status) =
                                tokio::time::timeout(HOLD_ATTACH_GRACE, child.wait()).await
                            {
                                status
                            } else {
                                attached.store(true, Ordering::SeqCst);
                                child.wait().await
                            };
                            attached.store(false, Ordering::SeqCst);
                            let reason = match drain {
                                Some(handle) => {
                                    String::from_utf8_lossy(&handle.await.unwrap_or_default())
                                        .into_owned()
                                }
                                None => String::new(),
                            };
                            if started.elapsed() >= HOLD_HEALTHY_RUN {
                                failures = 0;
                            } else {
                                failures += 1;
                            }
                            tracing::warn!(
                                stream = %stream,
                                ran_for = ?started.elapsed(),
                                ?status,
                                failures,
                                reason = %snippet(&reason),
                                "the hold consumer exited"
                            );
                        }
                        Err(err) => {
                            attached.store(false, Ordering::SeqCst);
                            failures += 1;
                            tracing::warn!(
                                stream = %stream,
                                %err,
                                failures,
                                "the hold consumer could not start"
                            );
                        }
                    }
                    if failures == HOLD_ESCALATE_AT {
                        tracing::warn!(
                            stream = %stream,
                            failures,
                            "the hold keeps dying young; check the ffmpeg path and the rtsp listener"
                        );
                    }
                    tokio::time::sleep(hold_backoff(failures)).await;
                }
            })
        };
        Arc::new(FfmpegLease { worker, attached })
    }
}

/// The running hold: aborting the worker kills its ffmpeg through
/// `kill_on_drop`, and the attach flag clears with it.
struct FfmpegLease {
    worker: tokio::task::JoinHandle<()>,
    attached: Arc<AtomicBool>,
}

impl HoldLease for FfmpegLease {
    fn attached(&self) -> bool {
        self.attached.load(Ordering::SeqCst)
    }
}

impl Drop for FfmpegLease {
    fn drop(&mut self) {
        self.worker.abort();
        self.attached.store(false, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_hold_backoff_doubles_from_five_and_caps_at_a_minute() {
        let values: Vec<u64> = (1..=7)
            .map(|failures| hold_backoff(failures).as_secs())
            .collect();
        assert_eq!(
            values,
            vec![10, 20, 40, 60, 60, 60, 60],
            "the base is 5s, doubling per consecutive failure to the 60s cap"
        );
        assert_eq!(
            hold_backoff(0),
            HOLD_RETRY,
            "with no failures yet, the base itself"
        );
    }
}
