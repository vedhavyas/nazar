//! Talkback: the PTT session update, its heartbeat, and the audio frames -
//! ported from the decompile (`ghidra_stream2.txt`).
//!
//! The native flow the port mirrors:
//! - `startAudio` is `updateP2PStream(session, ..., ADD_PTT)` (0x170e10),
//!   `stopAudio` its REMOVE_PTT twin (0x1712a0); the wire twin is the
//!   `setStreamState` command with `pttEnabled` (protocol 4.4).
//! - `sendP2pPttHeartbeat` (0x15b380) sends the heartbeat URL;
//!   `getP2pPttHeartbeatUrl` (0x15bad4) builds it, and the timer callback
//!   (0x172144) re-arms itself every 15000 ms under the id
//!   `p2p_ptt_session_<session>`.
//!
//! [UNCERTAIN httpMethod] the heartbeat's verb: the decompile passes enum
//! value 2, and only 1 = POST is pinned on disk (protocol 2.1) - the empty
//! body with everything in the query reads as GET, which is what the port
//! sends. The live run settles it against the response.
//!
//! [UNCERTAIN audio] the mic uplink's payload encoding: the dumps pin the
//! RTP shape of the downlink audio (PT 97, protocol 6.5) but not what codec
//! the camera expects on the uplink. `push_audio` frames the caller's
//! samples as PT 97 verbatim; the live wiring test against the camera's
//! response is what settles the encoding.

use std::time::Duration;

use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::client::QuboClient;
use crate::p2p::signaling::{StreamQuality, epoch_millis};
use crate::session::Session;

/// The heartbeat cadence: the SDK's own timer (ghi 0x172144 re-arms at
/// 15000 ms).
pub const PTT_HEARTBEAT: Duration = Duration::from_secs(15);

/// The audio payload type; the capture's audio rides PT 97.
pub const AUDIO_PAYLOAD_TYPE: u8 = 97;

/// What a talk burst needs from the arm: the account session, the
/// signaling client, and the identities the heartbeat names.
#[derive(Debug, Clone)]
pub struct TalkContext {
    pub client: QuboClient,
    pub session: Session,
    pub unit_id: String,
    pub user_app_id: String,
    pub quality: StreamQuality,
}

impl TalkContext {
    /// The heartbeat's path, exactly `getP2pPttHeartbeatUrl`'s grammar.
    pub fn heartbeat_path(&self, camera: &str, session_id: &str) -> String {
        heartbeat_path(
            self.client.service_provider().as_str(),
            &self.unit_id,
            camera,
            &self.user_app_id,
            session_id,
        )
    }
}

/// The heartbeat path: `/stream-manager/api/v1/sp/<sp>/unit/<unit>
/// /device/<camera>/ptt/heartbeat?userAppID=<app>&appSessionId=<session>`.
pub fn heartbeat_path(
    sp: &str,
    unit: &str,
    camera: &str,
    user_app_id: &str,
    session_id: &str,
) -> String {
    format!(
        "/stream-manager/api/v1/sp/{sp}/unit/{unit}/device/{camera}/ptt/heartbeat\
         ?userAppID={user_app_id}&appSessionId={session_id}"
    )
}

/// The `setStreamState` update that starts or ends a talk burst - the
/// `updateP2PStream` ADD_PTT/REMOVE_PTT actions on the wire (protocol
/// 4.4) - in the data-channel envelope shape of protocol 6.7.
pub fn set_stream_state(
    camera: &str,
    session_id: &str,
    ptt: bool,
    quality: StreamQuality,
) -> Value {
    let mut services = serde_json::Map::new();
    services.insert(
        "p2pStreaming".to_owned(),
        json!({
            "commands": {
                "setStreamState": {
                    "parameters": {
                        "sessionId": session_id,
                        "pttEnabled": ptt,
                        "streamSourceQuality": quality.as_str(),
                    },
                    "instanceId": 0,
                }
            }
        }),
    );
    json!({
        "command": {
            "devices": { "deviceUUID": camera, "services": services }
        },
        "deviceUUID": camera,
        "timestamp": epoch_millis(),
    })
}

/// One audio frame: a 12-byte RTP header (PT 97, SSRC zero) followed by the
/// samples verbatim.
pub fn audio_frame(sequence: u16, timestamp: u32, samples: &[u8]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(12 + samples.len());
    frame.push(0x80);
    frame.push(AUDIO_PAYLOAD_TYPE);
    frame.extend_from_slice(&sequence.to_be_bytes());
    frame.extend_from_slice(&timestamp.to_be_bytes());
    frame.extend_from_slice(&[0, 0, 0, 0]);
    frame.extend_from_slice(samples);
    frame
}

/// A running talk burst: the heartbeat task and the audio clock's counters.
pub struct Talk {
    heartbeat: JoinHandle<()>,
    sequence: u16,
    timestamp: u32,
}

impl Talk {
    /// Start the heartbeat: one beat now, then one per `cadence` (the
    /// caller passes [`PTT_HEARTBEAT`]; tests compress it), against the
    /// cloud with the session's own auth. The verb is the module's
    /// [UNCERTAIN httpMethod] GET reading.
    pub fn start(context: &TalkContext, path: String, cadence: Duration) -> Self {
        let client = context.client.clone();
        let session = context.session.clone();
        let heartbeat = tokio::spawn(async move {
            let mut ticker = tokio::time::interval(cadence);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match client.get_json(&path, Some(&session)).await {
                    Ok(_) => tracing::debug!("ptt heartbeat sent"),
                    Err(err) => tracing::debug!(%err, "ptt heartbeat failed"),
                }
            }
        });
        Self {
            heartbeat,
            sequence: 0,
            timestamp: 0,
        }
    }

    /// The next frame's sequence and timestamp, advancing the clock by the
    /// sample count (16-bit mono, so two bytes per sample - [UNCERTAIN:
    /// the uplink clock rate is unpinned]).
    pub fn next_frame(&mut self, samples: &[u8]) -> Vec<u8> {
        let frame = audio_frame(self.sequence, self.timestamp, samples);
        self.sequence = self.sequence.wrapping_add(1);
        let step = u32::try_from(samples.len() / 2).unwrap_or(u32::MAX);
        self.timestamp = self.timestamp.wrapping_add(step);
        frame
    }

    /// End the burst's heartbeat.
    pub fn stop(self) {
        self.heartbeat.abort();
    }
}

#[cfg(test)]
mod tests {
    //! The heartbeat's cadence and its query, driven against a local double.

    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;
    use crate::client::ServiceProvider;

    fn context(server: &wiremock::MockServer) -> TalkContext {
        TalkContext {
            client: QuboClient::new(
                server.uri().parse().expect("a url"),
                ServiceProvider::new("sp"),
            ),
            session: Session {
                access_token: "token".to_owned(),
                uuid: "user".to_owned(),
                email: None,
            },
            unit_id: "unit".to_owned(),
            user_app_id: "self".to_owned(),
            quality: StreamQuality::High,
        }
    }

    #[tokio::test]
    async fn the_heartbeat_keeps_its_cadence_and_names_the_session() {
        let server = wiremock::MockServer::start().await;
        wiremock::Mock::given(wiremock::matchers::method("GET"))
            .and(wiremock::matchers::path(
                "/stream-manager/api/v1/sp/sp/unit/unit/device/cam/ptt/heartbeat",
            ))
            .and(wiremock::matchers::header("User-UUID", "user"))
            .respond_with(wiremock::ResponseTemplate::new(200).set_body_string("{}"))
            .mount(&server)
            .await;

        let context = context(&server);
        let talk = Talk::start(
            &context,
            context.heartbeat_path("cam", "sid"),
            Duration::from_millis(30),
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
        let requests = server.received_requests().await.expect("recorded requests");
        assert!(
            requests.len() >= 3,
            "the heartbeat runs on its cadence (saw {})",
            requests.len()
        );
        let query: Vec<(String, String)> = requests[0]
            .url
            .query_pairs()
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        assert!(
            query.contains(&("userAppID".to_owned(), "self".to_owned()))
                && query.contains(&("appSessionId".to_owned(), "sid".to_owned())),
            "the query names the app device and the session"
        );
        assert_eq!(
            requests[0].url.path(),
            "/stream-manager/api/v1/sp/sp/unit/unit/device/cam/ptt/heartbeat",
            "the heartbeat hits the decompiled path"
        );

        talk.stop();
        let settled = server.received_requests().await.expect("requests").len();
        tokio::time::sleep(Duration::from_millis(120)).await;
        assert_eq!(
            server.received_requests().await.expect("requests").len(),
            settled,
            "the stop ends the heartbeat"
        );
    }
}
