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
    /// One control envelope for the camera.
    Control(Value),
    /// End the session (the pump disarms on its way out).
    Shutdown,
}

/// The server-side half of one armed session: ports for go2rtc to read,
/// and a control path back to the camera.
struct PumpLane {
    commands: mpsc::UnboundedSender<LaneCommand>,
    sources: Mutex<HashMap<String, String>>,
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
        if let Some(source) = sources.get(name) {
            return Ok(source.clone());
        }
        let port = free_loopback_port()?;
        let sdp = write_sdp(&self.data_dir, name, port)?;
        self.commands
            .send(LaneCommand::Subscribe { port })
            .map_err(|_| "the fast-lane session has ended".to_owned())?;
        let source = format!("ffmpeg:{}#input=nazar#video=h264", sdp.display());
        sources.insert(name.to_owned(), source.clone());
        Ok(source)
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
                Some(LaneCommand::Subscribe { port }) => {
                    let sink = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
                    if !sinks.contains(&sink) {
                        sinks.push(sink);
                    }
                }
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
                    if packet.header().payload_type == 96 {
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
