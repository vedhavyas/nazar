//! P2P wire vectors: every frame builder pinned against the captures and the
//! reference implementation.
//!
//! Expected bytes live in `fixtures/p2p_byte_vectors.json`; its per-entry
//! `source` fields name the capture frame or reference case each vector came
//! from, and `provenance` records how it was produced (tshark extraction plus
//! the reference client's own builders and decryptor, never hand-typed). The
//! frames themselves are cited in `docs/qubo-protocol.md` §6.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::BTreeSet;

use serde_json::{Value, json};

use qubo::p2p::media::{AesMode, H264Assembler, MediaDecryptor, decode_key16, unwrap_rtp};
use qubo::p2p::relay::{self, NameGen, Packet, Txid};
use qubo::p2p::signaling::{
    MediaStreams, Protocols, StartCall, StreamQuality, build_candidate_list, build_end_call,
    build_start_call, candidate_endpoints, parse_p2p_callback,
};

fn vectors() -> Value {
    serde_json::from_str(include_str!("fixtures/p2p_byte_vectors.json"))
        .expect("the vector fixture is JSON")
}

fn hex_bytes(text: &str) -> Vec<u8> {
    text.as_bytes()
        .chunks(2)
        .map(|pair| {
            u8::from_str_radix(std::str::from_utf8(pair).expect("hex is ASCII"), 16)
                .expect("hex bytes")
        })
        .collect()
}

fn case_bytes(case: &Value) -> Vec<u8> {
    hex_bytes(case["bytes"].as_str().expect("a bytes field"))
}

fn case_nonce(case: &Value) -> [u8; 20] {
    hex_bytes(case["nonce"].as_str().expect("a nonce field"))
        .try_into()
        .expect("a 20-byte nonce")
}

fn case_txid(case: &Value) -> Txid {
    let name = case["name"].as_str().expect("a name field");
    Txid::parse(&name[..12]).expect("a 12-byte txid")
}

#[test]
fn simple_syn_matches_the_reference_layout_and_the_captured_train() {
    let v = vectors();

    let reference = &v["simple_syn_reference"];
    let syn = relay::simple_syn(&case_txid(reference), &case_nonce(reference));
    assert_eq!(syn.len(), 52, "the simple SYN is 52 bytes");
    assert_eq!(
        &syn[..],
        case_bytes(reference).as_slice(),
        "the simple SYN reproduces the reference selftest's byte layout"
    );

    let captured = &v["simple_syn_capture"];
    let syn = relay::simple_syn(&case_txid(captured), &case_nonce(captured));
    assert_eq!(
        &syn[..],
        case_bytes(captured).as_slice(),
        "the simple SYN matches capture frame 6293 byte for byte"
    );
}

#[test]
fn extended_syn_matches_the_captured_session_packet() {
    let v = vectors();
    let case = &v["extended_syn_reference"];
    let syn = relay::extended_syn(&case_txid(case), &case_nonce(case));
    assert_eq!(syn.len(), 60, "the extended SYN is 60 bytes");
    assert_eq!(
        &syn[..],
        case_bytes(case).as_slice(),
        "the extended SYN matches capture frame 6301 byte for byte"
    );
}

#[test]
fn register_matches_the_captured_turn_connect() {
    let v = vectors();
    let case = &v["register_capture"];
    let register = relay::register(&case_txid(case));
    assert_eq!(register.len(), 24, "the register is 24 bytes");
    assert_eq!(
        &register[..],
        case_bytes(case).as_slice(),
        "the register matches capture frame 893 byte for byte"
    );
}

#[test]
fn peer_ack_matches_the_captured_probe_reply() {
    let v = vectors();
    let case = &v["peer_ack_capture"];
    let name = case["name"].as_str().expect("a name field");
    let ack = relay::peer_ack(name.as_bytes(), &case_nonce(case)).expect("a 12-byte peer txid");
    assert_eq!(ack.len(), 44, "the probe reply is 44 bytes");
    assert_eq!(
        &ack[..],
        case_bytes(case).as_slice(),
        "the probe reply matches capture frame 6312 byte for byte"
    );
}

#[test]
fn classify_reads_the_captured_handshake_frames() {
    let v = vectors();
    let name = |case: &Value| case["name"].as_str().expect("a name field").to_owned();

    match relay::classify(&case_bytes(&v["probe_capture"])) {
        Packet::Probe { txid } => {
            assert_eq!(
                txid,
                name(&v["probe_capture"]).as_bytes(),
                "the camera's own query classifies as a probe"
            );
        }
        other => panic!("expected Probe, got {other:?}"),
    }
    match relay::classify(&case_bytes(&v["echo_capture"])) {
        Packet::Echo { txid } => {
            assert_eq!(
                txid,
                name(&v["echo_capture"]).as_bytes(),
                "the camera's reply to our SYN classifies as an echo"
            );
        }
        other => panic!("expected Echo, got {other:?}"),
    }
    match relay::classify(&case_bytes(&v["extended_ack_capture"])) {
        Packet::ExtendedAck { txid } => {
            assert_eq!(
                txid,
                name(&v["extended_ack_capture"]).as_bytes(),
                "the ack of an extended SYN carries its txid"
            );
        }
        other => panic!("expected ExtendedAck, got {other:?}"),
    }
    assert!(
        matches!(
            relay::classify(&case_bytes(&v["rtp_frames"][0])),
            Packet::Rtp
        ),
        "an RTP datagram classifies as media"
    );

    // The relay's session teardown (grammar from the capture, 6.1; no
    // teardown frame was in a captured window).
    let mut bye = vec![0x20, 0x03, 0x00, 0x04];
    bye.extend_from_slice(&[0x24, 0x4A, 0x21, 0x12]);
    bye.extend_from_slice(b"qWruqY1E1055");
    assert!(
        matches!(relay::classify(&bye), Packet::Bye),
        "the relay's 2003 teardown classifies as a bye"
    );
}

#[test]
fn control_frame_matches_the_captured_pan_command() {
    let v = vectors();
    let case = &v["control_capture"];
    let txid = Txid::parse(case["txid"].as_str().expect("a txid field")).expect("a 12-byte txid");
    let json = case["json"].as_str().expect("a json field").as_bytes();
    assert_eq!(
        json.len(),
        usize::try_from(case["json_len"].as_u64().expect("a json_len field"))
            .expect("the json length fits a usize"),
        "the fixture's json length is the capture's"
    );

    let frame = relay::control_frame(&txid, json, &case_nonce(case))
        .expect("the captured envelope fits the frame");
    assert_eq!(frame.len(), 524, "the 2001 frame is 524 bytes");
    assert_eq!(
        frame,
        case_bytes(case),
        "the 2001 frame matches capture frame 1886 byte for byte"
    );

    // A json length that is not a multiple of four is zero-padded, as the
    // capture's frame 2218 shows (its 477-byte JSON pads by 3).
    let frame = relay::control_frame(&txid, b"{}abc", &[0u8; 20]).expect("fits a u16 length");
    assert_eq!(
        frame.len(),
        4 + 4 + 12 + 4 + 5 + 3 + 24,
        "odd json lengths pad to the 4-byte attribute boundary"
    );
    assert_eq!(
        &frame[24 + 5..24 + 5 + 3],
        &[0, 0, 0],
        "the padding is zero bytes"
    );
}

#[test]
fn txid_is_eight_alnum_plus_a_monotonic_counter() {
    let mut names = NameGen::new();
    let first = names.generate();
    let second = names.generate();
    for name in [&first, &second] {
        let text = name.as_str();
        assert_eq!(text.len(), 12, "a txid is 12 bytes");
        assert!(
            text.as_bytes()[..8].iter().all(u8::is_ascii_alphanumeric),
            "the first 8 bytes are alphanumerics"
        );
        assert!(
            text.as_bytes()[8..].iter().all(u8::is_ascii_digit),
            "the last 4 bytes are the counter"
        );
    }
    let counter = |txid: &Txid| -> u32 { txid.as_str()[8..].parse().expect("a 4-digit counter") };
    assert_eq!(
        counter(&second),
        counter(&first) + 1,
        "the counter is monotonic across generations"
    );

    // A fresh random prefix per generation: a constant prefix must not
    // survive (an 8-character collision across two draws is ~1 in 2e14).
    let mut prefixes = std::collections::BTreeSet::new();
    let mut names = NameGen::new();
    for _ in 0..2 {
        prefixes.insert(names.generate().as_str()[..8].to_owned());
    }
    assert_eq!(
        prefixes.len(),
        2,
        "the prefix is freshly generated per txid"
    );
}

#[test]
fn rtp_unwrap_extracts_the_captured_payload() {
    let v = vectors();
    let packet = case_bytes(&v["rtp_frames"][0]);
    let rtp = unwrap_rtp(&packet).expect("the captured RTP parses");
    assert_eq!(rtp.payload_type, 96, "the captured stream is PT 96");
    assert!(
        rtp.marker,
        "the first packet of the burst carries the marker"
    );
    assert_eq!(rtp.sequence, 0x16e9, "the sequence number is as captured");
    assert_eq!(
        rtp.timestamp, 0,
        "the timestamp is zero, as the SDK sends it"
    );
    assert_eq!(rtp.ssrc, 0, "the SSRC is zero, as the SDK sends it");
    assert_eq!(rtp.header_len, 12, "no CSRCs and no extension on the wire");
    assert_eq!(
        packet.len() - rtp.header_len,
        23,
        "the first captured packet carries 23 payload bytes"
    );

    // The header length also skips a CSRC list and an extension header.
    let mut padded = vec![
        0x92, 0x60, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
    ];
    padded.extend([0xAA; 8]); // two CSRCs
    padded.extend([0xBE, 0xDE, 0x00, 0x02]); // extension profile + two words
    padded.extend([0x11; 8]); // the extension words
    padded.extend([0x22, 0x33]); // the RTP payload
    let rtp = unwrap_rtp(&padded).expect("parses with CSRCs and an extension");
    assert_eq!(
        rtp.header_len,
        12 + 8 + 4 + 8,
        "CSRCs and extension words are skipped"
    );
    assert_eq!(
        &padded[rtp.header_len..],
        &[0x22, 0x33],
        "the payload after the extension is the RTP payload"
    );
}

#[test]
fn aes_ctr_matches_the_reference_on_captured_packets() {
    let v = vectors();
    let key = v["aes"]["key"].as_str().expect("a key field");
    let iv = v["aes"]["iv"].as_str().expect("an iv field");

    assert_eq!(
        decode_key16(key).expect("the captured key decodes"),
        hex_bytes(key).as_slice(),
        "the hex key decodes to its 16 raw bytes"
    );
    assert_eq!(
        decode_key16(v["aes"]["key_b64"].as_str().expect("a key_b64 field"))
            .expect("the base64 spelling decodes"),
        hex_bytes(key).as_slice(),
        "the base64 spelling of the same key decodes identically"
    );

    let frames: Vec<Value> = v["rtp_frames"]
        .as_array()
        .expect("a rtp_frames array")
        .clone();

    // ctr-cont: one keystream continued across packets (the reference's default).
    let mut decryptor = MediaDecryptor::new(AesMode::CtrCont, key, iv)
        .expect("the captured keys build a decryptor");
    for (index, frame) in frames.iter().enumerate() {
        let packet = case_bytes(frame);
        let rtp = unwrap_rtp(&packet).expect("the captured RTP parses");
        let mut payload = packet[rtp.header_len..].to_vec();
        decryptor.decrypt(&mut payload);
        assert_eq!(
            payload,
            hex_bytes(
                frame["plaintext_ctr_cont"]
                    .as_str()
                    .expect("a plaintext field")
            ),
            "packet {} matches the reference's ctr-cont plaintext",
            index + 1
        );
    }

    // ctr-reset: the keystream restarts on every packet.
    let mut decryptor = MediaDecryptor::new(AesMode::CtrReset, key, iv)
        .expect("the captured keys build a decryptor");
    for (index, frame) in frames.iter().enumerate() {
        let packet = case_bytes(frame);
        let mut payload = packet[12..].to_vec();
        decryptor.decrypt(&mut payload);
        assert_eq!(
            payload,
            hex_bytes(
                frame["plaintext_ctr_reset"]
                    .as_str()
                    .expect("a plaintext field")
            ),
            "packet {} matches the reference's ctr-reset plaintext",
            index + 1
        );
    }
}

#[test]
fn aes_cfb_ofb_and_plain_match_the_reference_on_captured_packets() {
    let v = vectors();
    let key = v["aes"]["key"].as_str().expect("a key field");
    let iv = v["aes"]["iv"].as_str().expect("an iv field");
    let frames: Vec<Value> = v["rtp_frames"]
        .as_array()
        .expect("a rtp_frames array")
        .clone();

    for (mode, field) in [
        (AesMode::Cfb, "plaintext_cfb"),
        (AesMode::Ofb, "plaintext_ofb"),
    ] {
        let mut decryptor =
            MediaDecryptor::new(mode, key, iv).expect("the captured keys build a decryptor");
        for (index, frame) in frames.iter().enumerate() {
            let packet = case_bytes(frame);
            let mut payload = packet[12..].to_vec();
            decryptor.decrypt(&mut payload);
            assert_eq!(
                payload,
                hex_bytes(frame[field].as_str().expect("a plaintext field")),
                "{mode:?} packet {} matches the reference's plaintext",
                index + 1
            );
        }
    }

    // Plain is the diagnostic passthrough: the ciphertext is the payload.
    let packet = case_bytes(&frames[0]);
    let mut payload = packet[12..].to_vec();
    MediaDecryptor::new(AesMode::Plain, key, iv)
        .expect("the captured keys build a decryptor")
        .decrypt(&mut payload);
    assert_eq!(
        payload,
        packet[12..],
        "the plain mode passes the payload through untouched"
    );
}

#[test]
fn h264_depacketizes_single_stap_a_and_fu_a() {
    let mut assembler = H264Assembler::new();
    assert_eq!(
        assembler.feed(&[0x65, 0x01, 0x02, 0x03]),
        vec![vec![0x65, 0x01, 0x02, 0x03]],
        "a single-NAL packet passes through"
    );
    assert_eq!(
        assembler.feed(&[0x18, 0x00, 0x03, 0x67, 0x01, 0xAA, 0x00, 0x02, 0x68, 0xBB]),
        vec![vec![0x67, 0x01, 0xAA], vec![0x68, 0xBB]],
        "a STAP-A unpacks into its size-prefixed NALs"
    );

    // The reference selftest's FU-A case: the fragment payload carries the NAL
    // without its header byte; the header is rebuilt from the FU indicator.
    let mut assembler = H264Assembler::new();
    let nal = [0x65_u8, 0x01, 0x02, 0x03];
    let mut out = assembler.feed(&[0x7C, 0x85, nal[1], nal[2]]);
    out.extend(assembler.feed(&[0x7C, 0x45, nal[3]]));
    assert_eq!(
        out,
        vec![nal.to_vec()],
        "FU-A fragments reassemble in order"
    );
    assert!(assembler.saw_idr(), "the reassembled IDR is reported");
}

/// The start call the builder produces for the captured identity.
fn built_start_call() -> Value {
    let offer = build_candidate_list("10.0.0.5", 56902, &["1.2.3.4:100".to_owned()]);
    build_start_call(&StartCall {
        camera: "c0000000-0000-4000-8000-000000000001",
        self_device: "d0000000-0000-4000-8000-000000000002",
        src_device_id: "<mobile-id>",
        session_id: "d0000000-0000-4000-8000-000000000004",
        offer_data: &offer,
        aes_key: "00000000000000000000000000000000",
        aes_iv: "11111111111111111111111111111111",
        protocols: Protocols::Udp,
        quality: StreamQuality::High,
        media: MediaStreams::Both,
        ptt: false,
    })
}

/// The `parameters` object inside a start-call envelope.
fn start_call_parameters(envelope: &Value) -> &Value {
    envelope
        .pointer("/command/devices/services/p2pStreaming/commands/startP2pCall/parameters")
        .expect("the envelope carries parameters")
}

#[test]
fn start_call_envelope_matches_the_captured_keys() {
    let captured: Value =
        serde_json::from_str(include_str!("fixtures/p2p_start_call_capture.json"))
            .expect("the capture fixture is JSON");
    let mine = built_start_call();

    let reference_params = &captured["command"]["devices"]["services"]["p2pStreaming"]["commands"]
        ["startP2pCall"]["parameters"];
    let mine_params = start_call_parameters(&mine);
    let keys = |value: &Value| -> BTreeSet<String> {
        value
            .as_object()
            .expect("an object")
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(
        keys(&mine),
        keys(&captured),
        "the envelope's top-level keys are the capture's"
    );
    assert_eq!(
        keys(mine_params),
        keys(reference_params),
        "the parameter keys are the capture's"
    );

    assert_eq!(
        mine_params["pttEnabled"],
        json!(false),
        "pttEnabled is a JSON boolean, as captured"
    );
    assert_eq!(
        mine_params["useTls"],
        Value::Null,
        "useTls is null, as captured"
    );
    assert_eq!(
        mine_params["playTimestamp"],
        json!(""),
        "playTimestamp is the empty string, as captured"
    );
    assert_eq!(
        mine_params["sourceType"],
        json!("live"),
        "sourceType is live"
    );
}

#[test]
fn start_call_envelope_carries_the_requested_settings() {
    let mine = built_start_call();
    let mine_params = start_call_parameters(&mine);

    assert_eq!(
        mine_params["mediaStreams"],
        json!("both"),
        "mediaStreams carries the requested media"
    );
    assert_eq!(
        mine_params["protocols"],
        json!("udp"),
        "protocols carries the requested protocol"
    );
    assert_eq!(
        mine_params["streamSourceQuality"],
        json!("high"),
        "streamSourceQuality carries the requested quality"
    );
    assert_eq!(
        mine_params["mobileUUID"],
        json!("d0000000-0000-4000-8000-000000000002"),
        "mobileUUID is the app-side device"
    );
    assert_eq!(
        mine_params["sessionId"],
        json!("d0000000-0000-4000-8000-000000000004"),
        "sessionId is the caller's allocation id, never another identity"
    );
    assert_eq!(
        mine_params["aesKey"],
        json!("00000000000000000000000000000000"),
        "aesKey is the caller's key verbatim"
    );
    assert_eq!(
        mine_params["aesIV"],
        json!("11111111111111111111111111111111"),
        "aesIV is the caller's iv verbatim"
    );
    assert_eq!(
        mine["deviceUUID"],
        json!("c0000000-0000-4000-8000-000000000001"),
        "deviceUUID is the camera"
    );
    assert_eq!(
        mine["srcDeviceId"],
        json!("<mobile-id>"),
        "srcDeviceId is the app device id"
    );
    assert_eq!(
        mine["timestamp"], mine["msgSequenceId"],
        "timestamp and msgSequenceId are one epoch-millisecond value"
    );
    let client_id = mine["clientId"].as_str().expect("clientId is a string");
    assert_eq!(client_id.len(), 36, "clientId is a fresh uuid");
    assert_eq!(
        client_id.matches('-').count(),
        4,
        "clientId is a fresh uuid"
    );

    // offerData is a string carrying the candidate array.
    let offer = mine_params["offerData"]
        .as_str()
        .expect("offerData is a string");
    let parsed: Value = serde_json::from_str(offer).expect("offerData parses as JSON");
    assert_eq!(
        parsed[0]["host"],
        json!("10.0.0.5:56902"),
        "the host candidate is ip:port"
    );
    assert_eq!(
        parsed[0]["protocol"],
        json!("UDP"),
        "the candidate protocol is UDP"
    );
    assert_eq!(
        parsed[0]["srflx_0"],
        json!("1.2.3.4:100"),
        "srflx entries carry their index"
    );
}

#[test]
fn end_call_carries_the_session_to_tear_down() {
    let end = build_end_call(
        "c0000000-0000-4000-8000-000000000001",
        "<mobile-id>",
        "d0000000-0000-4000-8000-000000000014",
    );
    let call = &end["command"]["devices"]["services"]["p2pStreaming"]["commands"]["endP2pCall"];
    assert_eq!(
        call["parameters"]["sessionId"],
        json!("d0000000-0000-4000-8000-000000000014"),
        "endP2pCall carries the session to tear down"
    );
    assert_eq!(call["instanceId"], json!(0), "instanceId is 0, as captured");
}

#[test]
fn callback_parser_reads_the_captured_answer() {
    let payload = include_str!("fixtures/p2p_call_answer_capture.json");
    let topic = "/monitor/20000000-0000-4000-8000-000000000001/\
                 c0000000-0000-4000-8000-000000000001/p2pStreaming";
    let session = "d0000000-0000-4000-8000-000000000004";

    let parsed = parse_p2p_callback(topic, payload, session).expect("the captured answer parses");
    assert_eq!(parsed.event, "p2pCallStarted", "the event name is read");
    assert_eq!(
        parsed.session_id, session,
        "the session id correlates the answer"
    );
    assert_eq!(
        parsed.handle_name.as_deref(),
        Some("d0000000-0000-4000-8000-000000000001"),
        "handleName is the camera's controllerUUID"
    );
    assert_eq!(
        candidate_endpoints(&parsed.candidates),
        vec![
            ("192.0.2.21".to_owned(), 56839),
            ("203.0.113.219".to_owned(), 56839)
        ],
        "endpoints flatten host first, then srflx, as the reference reads them"
    );

    assert!(
        parse_p2p_callback(topic, payload, "00000000-0000-0000-0000-000000000000").is_none(),
        "another session's answer is not ours"
    );
    assert!(
        parse_p2p_callback("/monitor/u/d/heartbeat", payload, session).is_none(),
        "a non-p2pStreaming topic is not ours"
    );
    assert!(
        parse_p2p_callback(topic, "not json", session).is_none(),
        "a non-JSON payload parses to nothing"
    );
}
