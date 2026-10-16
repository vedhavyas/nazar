//! PTT wire shapes, ported from the decompile (`ghidra_stream2.txt`):
//! the talk-start session update (`updateP2PStream`'s ADD_PTT/REMOVE_PTT,
//! 21329-21460 - wire twin: the `setStreamState` command of protocol 4.4),
//! the heartbeat URL (`getP2pPttHeartbeatUrl`, 7049-7101) and the audio
//! frame's RTP shape (protocol 6.5's PT 97).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::{Value, json};

use qubo::p2p::ptt::{audio_frame, heartbeat_path, set_stream_state};
use qubo::p2p::signaling::StreamQuality;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const CAMERA: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const SELF: &str = "d0000000-0000-4000-8000-000000000002";
const SESSION: &str = "d0000000-0000-4000-8000-000000000004";

fn command(value: &Value) -> &Value {
    value
        .pointer("/command/devices/services/p2pStreaming/commands/setStreamState")
        .expect("the envelope carries the command")
}

#[test]
fn set_stream_state_rides_the_captured_data_channel_envelope() {
    let on = set_stream_state(CAMERA, SESSION, true, StreamQuality::High);

    let keys = |value: &Value| -> BTreeSet<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(
        keys(&on),
        BTreeSet::from([
            "command".to_owned(),
            "deviceUUID".to_owned(),
            "timestamp".to_owned()
        ]),
        "the data-channel envelope's top level is the capture's (6.7)"
    );
    let parameters = &command(&on)["parameters"];
    assert_eq!(
        keys(parameters),
        BTreeSet::from([
            "pttEnabled".to_owned(),
            "sessionId".to_owned(),
            "streamSourceQuality".to_owned()
        ]),
        "the parameters are the decompile's set (protocol 4.4)"
    );
    assert_eq!(
        parameters["sessionId"],
        json!(SESSION),
        "the update names the armed session"
    );
    assert_eq!(
        parameters["pttEnabled"],
        json!(true),
        "pttEnabled is a JSON boolean, true for the start"
    );
    assert_eq!(
        parameters["streamSourceQuality"],
        json!("high"),
        "the update carries the session's quality"
    );
    assert_eq!(
        command(&on)["instanceId"],
        json!(0),
        "instanceId is 0, as in the capture"
    );
    assert_eq!(
        on["deviceUUID"],
        json!(CAMERA),
        "the envelope names the camera"
    );

    let off = set_stream_state(CAMERA, SESSION, false, StreamQuality::Low);
    assert_eq!(
        command(&off)["parameters"]["pttEnabled"],
        json!(false),
        "pttEnabled is false for the stop"
    );
    assert_eq!(
        command(&off)["parameters"]["streamSourceQuality"],
        json!("low"),
        "the stop carries the same quality grammar"
    );
    assert!(
        off["timestamp"].as_i64().is_some_and(|ms| ms > 0),
        "the timestamp is epoch milliseconds"
    );
}

#[test]
fn the_heartbeat_path_matches_the_decompiled_url() {
    let path = heartbeat_path(SP, UNIT, CAMERA, SELF, SESSION);
    assert_eq!(
        path,
        format!(
            "/stream-manager/api/v1/sp/{SP}/unit/{UNIT}/device/{CAMERA}/ptt/heartbeat\
             ?userAppID={SELF}&appSessionId={SESSION}"
        ),
        "the URL is getP2pPttHeartbeatUrl's own grammar (ghidra_stream2 7049)"
    );
}

#[test]
fn the_audio_frame_is_the_pt_97_rtp_shape() {
    let samples = [0xAA, 0xBB, 0xCC, 0xDD];
    let frame = audio_frame(0x0102, 0x0000_0400, &samples);
    assert_eq!(
        frame.len(),
        12 + samples.len(),
        "the frame is a 12-byte RTP header plus the samples"
    );
    assert_eq!(frame[0], 0x80, "RTP v2, no padding, no extension, no CSRCs");
    assert_eq!(frame[1], 97, "audio rides PT 97, the captured audio type");
    assert_eq!(
        &frame[2..4],
        &[0x01, 0x02],
        "the sequence number is big-endian"
    );
    assert_eq!(
        &frame[4..8],
        &[0x00, 0x00, 0x04, 0x00],
        "the timestamp is big-endian"
    );
    assert_eq!(
        &frame[8..12],
        &[0, 0, 0, 0],
        "the SSRC is zero, as the SDK sends it"
    );
    assert_eq!(
        &frame[12..],
        &samples,
        "the samples follow the header verbatim"
    );

    let empty = audio_frame(0, 0, &[]);
    assert_eq!(empty.len(), 12, "a header-only frame is still a frame");
}
