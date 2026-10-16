//! The fast lane, wired for the supervisor: arms the ported P2P engine and
//! feeds its decrypted RTP to go2rtc through a local UDP pump described by
//! a generated SDP file.
//!
//! go2rtc reads the pump as `ffmpeg:{sdp}#input=nazar` - the same template
//! the minted cloud sources ride - so a fast-lane stream is a normal go2rtc
//! stream downstream of the source swap. The camera's P2P media is H.264
//! already (protocol 6.5), so the template's transcode step passes it
//! through.
//!
//! The SDP describes video only: the audio payload type's codec is
//! unverified, and listen-audio keeps riding the cloud path until the live
//! round pins it down.
//!
//! The camera sends one stream, so an armed camera's streams all read the
//! same full-size media: on the fast lane the low tile's label keeps its
//! name but not a downscale (the shared template carries no scale step -
//! the cloud path's smaller rendition is ffmpeg work the mint URL asks
//! for, which the P2P source has no equivalent of).

use std::collections::HashMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use qubo::p2p::{P2pConfig, P2pEngine, P2pIdentity};
use qubo::{ArmedSession, Device, QuboClient, SessionStore};
use serde_json::Value;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use crate::supervisor::{BoxFuture, Lane, LaneSession};

/// The engine behind the supervisor's fast-lane seam.
pub struct EngineLane {
    client: QuboClient,
    sessions: SessionStore,
    identity: P2pIdentity,
    config: P2pConfig,
    data_dir: PathBuf,
}

impl EngineLane {
    pub fn new(
        client: QuboClient,
        sessions: SessionStore,
        identity: P2pIdentity,
        config: P2pConfig,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            client,
            sessions,
            identity,
            config,
            data_dir,
        }
    }
}

impl Lane for EngineLane {
    fn arm(&self, device: Device) -> BoxFuture<Result<Arc<dyn LaneSession>, String>> {
        // Everything the future needs is cloned up front: the returned
        // future is 'static, so a caller's timeout can hold or drop it
        // without borrowing the lane.
        let client = self.client.clone();
        let sessions = self.sessions.clone();
        let identity = self.identity.clone();
        let config = self.config.clone();
        let data_dir = self.data_dir.clone();
        Box::pin(async move {
            let session = sessions
                .load()
                .map_err(|err| err.to_string())?
                .ok_or_else(|| "no Qubo session; log in".to_owned())?;
            let engine = P2pEngine::new(client, config);
            let armed = engine
                .arm(&session, &device, &identity)
                .await
                .map_err(|err| err.to_string())?;
            let lane: Arc<dyn LaneSession> = Arc::new(PumpLane::start(armed, data_dir));
            Ok(lane)
        })
    }
}

/// What the supervisor asks of a running pump.
enum LaneCommand {
    /// Start fanning the session's media out to this loopback port.
    Subscribe { port: u16 },
    /// Stop fanning to this loopback port.
    Unsubscribe { port: u16 },
    /// One control envelope for the camera.
    Control(Value),
    /// End the session (the pump disarms on its way out).
    Shutdown,
}

/// The server-side half of one armed session: ports for go2rtc to read,
/// and a control path back to the camera.
struct PumpLane {
    commands: mpsc::UnboundedSender<LaneCommand>,
    /// Per stream: its sink's loopback port and the go2rtc source string.
    sources: Mutex<HashMap<String, (u16, String)>>,
    alive: Arc<AtomicBool>,
    /// Where the per-stream SDP files are written for go2rtc's ffmpeg.
    data_dir: PathBuf,
}

impl PumpLane {
    fn start(armed: ArmedSession, data_dir: PathBuf) -> Self {
        let (commands, mut receiver) = mpsc::unbounded_channel();
        let alive = Arc::new(AtomicBool::new(true));
        let pump_alive = Arc::clone(&alive);
        tokio::spawn(async move {
            pump(armed, &mut receiver, &pump_alive).await;
        });
        Self {
            commands,
            sources: Mutex::new(HashMap::new()),
            alive,
            data_dir,
        }
    }
}

impl LaneSession for PumpLane {
    fn source_for(&self, name: &str) -> Result<String, String> {
        if !self.alive() {
            return Err("the fast-lane session has ended".to_owned());
        }
        let mut sources = self
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((_, source)) = sources.get(name) {
            return Ok(source.clone());
        }
        let port = free_loopback_port()?;
        let sdp = write_sdp(&self.data_dir, name, port)?;
        self.commands
            .send(LaneCommand::Subscribe { port })
            .map_err(|_| "the fast-lane session has ended".to_owned())?;
        let source = sdp_source(&sdp);
        sources.insert(name.to_owned(), (port, source.clone()));
        Ok(source)
    }

    fn release(&self, name: &str) {
        let entry = self
            .sources
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(name);
        let Some((port, _)) = entry else {
            return;
        };
        let _ = self.commands.send(LaneCommand::Unsubscribe { port });
        remove_sdp(&self.data_dir, name);
    }

    fn control(&self, envelope: Value) -> Result<(), String> {
        self.commands
            .send(LaneCommand::Control(envelope))
            .map_err(|_| "the fast-lane session has ended".to_owned())
    }

    fn alive(&self) -> bool {
        self.alive.load(Ordering::SeqCst)
    }

    fn shutdown(&self) {
        let _ = self.commands.send(LaneCommand::Shutdown);
    }
}

/// The pump loop: media from the session fans out to every subscribed
/// port; control envelopes go back over the session's data channel.
async fn pump(
    mut armed: ArmedSession,
    commands: &mut mpsc::UnboundedReceiver<LaneCommand>,
    alive: &AtomicBool,
) {
    // The outbound socket the fan-out sends from; the go2rtc side binds the
    // subscribed ports, this one only writes.
    let sender = match UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).await {
        Ok(sender) => sender,
        Err(err) => {
            tracing::warn!(%err, "the fast-lane pump socket failed");
            alive.store(false, Ordering::SeqCst);
            armed.disarm().await;
            return;
        }
    };
    let mut sinks: Vec<SocketAddr> = Vec::new();
    loop {
        tokio::select! {
            command = commands.recv() => match command {
                None | Some(LaneCommand::Shutdown) => break,
                Some(LaneCommand::Subscribe { port }) => add_sink(&mut sinks, port),
                Some(LaneCommand::Unsubscribe { port }) => drop_sink(&mut sinks, port),
                Some(LaneCommand::Control(envelope)) => {
                    if let Err(err) = armed.send_control(&envelope).await {
                        tracing::warn!(%err, "the fast-lane control send failed");
                        break;
                    }
                }
            },
            media = armed.recv_media() => match media {
                Ok(Some(packet)) => {
                    // Video only for now; the audio payload type's codec is
                    // unverified, and listen-audio rides the cloud path.
                    if is_video(packet.header().payload_type) {
                        for sink in &sinks {
                            let _ = sender.send_to(packet.to_rtp_bytes(), sink).await;
                        }
                    }
                }
                Ok(None) => {}
                Err(err) => {
                    tracing::warn!(%err, "the fast-lane session ended");
                    break;
                }
            },
        }
    }
    alive.store(false, Ordering::SeqCst);
    armed.disarm().await;
}

/// The video payload type the pump forwards; PT 97 audio stays on the
/// cloud path until the live round pins its uplink codec.
const VIDEO_PAYLOAD_TYPE: u8 = 96;

/// Whether a packet rides the media leg the pump forwards.
fn is_video(payload_type: u8) -> bool {
    payload_type == VIDEO_PAYLOAD_TYPE
}

/// Add a stream's loopback sink to the fan-out, once.
fn add_sink(sinks: &mut Vec<SocketAddr>, port: u16) {
    let sink = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    if !sinks.contains(&sink) {
        sinks.push(sink);
    }
}

/// Drop a stream's sink from the fan-out.
fn drop_sink(sinks: &mut Vec<SocketAddr>, port: u16) {
    sinks.retain(|sink| sink.port() != port);
}

/// Remove a stream's SDP file; a missing one is already gone.
fn remove_sdp(data_dir: &Path, name: &str) {
    let _ = std::fs::remove_file(data_dir.join(format!("p2p-{name}.sdp")));
}

/// The go2rtc source for a stream's SDP file: the same template the cloud
/// sources ride, with the video passthrough (the camera's P2P media is
/// H.264 already).
fn sdp_source(sdp: &Path) -> String {
    format!("ffmpeg:{}#input=nazar#video=h264", sdp.display())
}

/// A port nothing is listening on yet; go2rtc's ffmpeg binds it when the
/// stream is first watched. The bind-then-drop window is a risk only if
/// something else claims the port first, which the loopback range makes
/// remote.
fn free_loopback_port() -> Result<u16, String> {
    let socket =
        std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|err| err.to_string())?;
    let port = socket.local_addr().map_err(|err| err.to_string())?.port();
    drop(socket);
    Ok(port)
}

/// The SDP ffmpeg reads for the pump: our RTP stream, video (PT 96,
/// H.264/90000), on the loopback port.
fn write_sdp(data_dir: &Path, name: &str, port: u16) -> Result<PathBuf, String> {
    let path = data_dir.join(format!("p2p-{name}.sdp"));
    let content = format!(
        "v=0\no=- 0 0 IN IP4 127.0.0.1\ns=nazar {name}\nc=IN IP4 127.0.0.1\nt=0 0\n\
         m=video {port} RTP/AVP 96\na=rtpmap:96 H264/90000\n"
    );
    std::fs::write(&path, content).map_err(|err| err.to_string())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    //! The pump's pure pieces: the SDP write, the port pick, the fan-out
    //! bookkeeping and the video filter - the rest is the live round's.

    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn the_sdp_names_the_port_and_the_codec() {
        let dir = std::env::temp_dir().join(format!("nazar-sdp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = write_sdp(&dir, "cam-high", 43210).expect("the sdp writes");
        assert_eq!(
            path,
            dir.join("p2p-cam-high.sdp"),
            "the file is named for the stream"
        );
        let content = std::fs::read_to_string(&path).expect("the file exists");
        assert!(
            content.contains("m=video 43210 RTP/AVP 96"),
            "the sdp names the pump's port and PT 96: {content}"
        );
        assert!(
            content.contains("a=rtpmap:96 H264/90000"),
            "and the H.264 mapping"
        );
        assert_eq!(
            sdp_source(&path),
            format!("ffmpeg:{}#input=nazar#video=h264", path.display()),
            "the go2rtc source rides the shared template"
        );
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_port_pick_is_bindable() {
        let port = free_loopback_port().expect("a free port");
        assert!(port > 0, "a real port comes back");
        let socket = std::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, port))
            .expect("the picked port was free to bind");
        drop(socket);
    }

    #[test]
    fn the_fan_out_adds_once() {
        let mut sinks = Vec::new();
        add_sink(&mut sinks, 41001);
        add_sink(&mut sinks, 41002);
        add_sink(&mut sinks, 41001);
        assert_eq!(
            sinks.len(),
            2,
            "a repeated subscribe does not double the fan-out"
        );
        assert_eq!(sinks[0].port(), 41001, "order is preserved");
    }

    #[test]
    fn the_fan_out_drops_by_port() {
        let mut sinks = Vec::new();
        add_sink(&mut sinks, 41001);
        add_sink(&mut sinks, 41002);
        drop_sink(&mut sinks, 41001);
        assert_eq!(sinks.len(), 1, "the dropped stream's sink leaves");
        assert_eq!(sinks[0].port(), 41002, "the other stream's sink stays");
    }

    #[test]
    fn releasing_a_stream_removes_its_sdp() {
        let dir = std::env::temp_dir().join(format!("nazar-sdp-rel-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("a temp dir");
        let path = write_sdp(&dir, "cam-high", 41003).expect("the sdp writes");
        assert!(path.exists(), "the file is there to begin with");
        remove_sdp(&dir, "cam-high");
        assert!(!path.exists(), "release takes the sdp with it");
        // A second release for an already-gone file is a no-op.
        remove_sdp(&dir, "cam-high");
    }

    #[test]
    fn only_video_is_forwarded() {
        assert!(is_video(96), "PT 96 is the camera's video");
        assert!(!is_video(97), "PT 97 audio stays on the cloud path for now");
        assert!(!is_video(0), "everything else is not the media leg");
    }
}
