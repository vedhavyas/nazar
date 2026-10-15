//! MQTT monitor-topic parsing, against payloads captured live.
//!
//! Fixtures are verbatim payloads from `qubo-spike/mqtt_probe.log`
//! (2026-10-14, spec §5).

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use server::events::EventBus;
use server::mqtt::{UnitEvent, parse_monitor_message};

const UNIT: &str = "20000000-0000-4000-8000-000000000001";
const CAMERA: &str = "c0000000-0000-4000-8000-000000000002";
const OTHER_UNIT_DEVICE: &str = "d0000000-0000-4000-8000-000000000002";
const OFFLINE_DEVICE: &str = "d0000000-0000-4000-8000-000000000003";

fn topic(device: &str, service: &str) -> String {
    format!("/monitor/{UNIT}/{device}/{service}")
}

#[test]
fn a_recording_event_reads_as_motion() {
    for fixture in [
        include_str!("fixtures/mqtt_recording_motion.json"),
        include_str!("fixtures/mqtt_recording_person.json"),
    ] {
        assert_eq!(
            parse_monitor_message(&topic(CAMERA, "recordingControl"), fixture),
            Some(UnitEvent::Motion {
                device_id: CAMERA.to_owned()
            }),
            "a sighting is a motion for the hero"
        );
    }
}

#[test]
fn an_unrecognized_cause_is_not_motion() {
    let payload = r#"{"devices":{"services":{"recordingControl":{"events":{"stateChanged":{"cause":"SOUND_DETECTED"}}}}}}"#;
    assert_eq!(
        parse_monitor_message(&topic(CAMERA, "recordingControl"), payload),
        None,
        "only a seen motion moves the hero"
    );
}

#[test]
fn a_heartbeat_reads_as_liveness() {
    let cases = [
        (
            OTHER_UNIT_DEVICE,
            include_str!("fixtures/mqtt_heartbeat_online.json"),
            true,
        ),
        (
            OFFLINE_DEVICE,
            include_str!("fixtures/mqtt_heartbeat_offline.json"),
            false,
        ),
    ];
    for (device, payload, online) in cases {
        assert_eq!(
            parse_monitor_message(&topic(device, "heartbeat"), payload),
            Some(UnitEvent::Liveness {
                device_id: device.to_owned(),
                online
            }),
            "a heartbeat says whether the device is there"
        );
    }
}

#[test]
fn other_services_are_not_events_yet() {
    let payload = include_str!("fixtures/mqtt_heartbeat_online.json");
    assert_eq!(
        parse_monitor_message(&topic(CAMERA, "aqiStatus"), payload),
        None,
        "the house listens for motion and presence, nothing else"
    );
}

#[test]
fn junk_is_not_an_event() {
    assert_eq!(
        parse_monitor_message(&topic(CAMERA, "heartbeat"), "{not json"),
        None,
        "a malformed payload is dropped, not fatal"
    );
    assert_eq!(
        parse_monitor_message("not/a/monitor/topic", "{}"),
        None,
        "topics outside the monitor family are ignored"
    );
    assert_eq!(
        parse_monitor_message(&topic(CAMERA, "recordingControl"), "{}"),
        None,
        "a payload without the event path is dropped"
    );
}

#[tokio::test]
async fn a_published_event_reaches_a_subscriber() {
    let bus = EventBus::new();
    let mut watcher = bus.subscribe();
    bus.publish(UnitEvent::Liveness {
        device_id: CAMERA.to_owned(),
        online: true,
    });
    // A timeout so a broken publish fails by name instead of hanging the
    // suite.
    let delivered = tokio::time::timeout(std::time::Duration::from_secs(1), watcher.recv())
        .await
        .expect("the published event arrives");
    assert_eq!(
        delivered.expect("the subscription is live"),
        UnitEvent::Liveness {
            device_id: CAMERA.to_owned(),
            online: true
        },
        "the bus carries the event unchanged"
    );
}
