//! Control wire tests: the command envelope and the shadow read.
//!
//! Fixture provenance (spec §4 E9): `shadow_camera.json` is modeled on the
//! documented services-array shape and qubopy's parser, which the live
//! sessions proved end to end on 2026-10-13/14 (a PTZ move and a privacy
//! write, both confirmed by shadow read-back). The raw shadow response was
//! never recorded - the 2026-10-14 captures are TLS-encrypted without a
//! keylog - so the fixture carries the documented shape, both attribute
//! encodings (`{"value": ...}` and scalar), not captured bytes.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::client::QuboClient;
use qubo::control::{read_shadow, write_service};
use qubo::devices::Device;
use qubo::session::Session;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const ENTITY: &str = "30000000-0000-4000-8000-000000000001";
const DEVICE_ID: &str = "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";
const DEVICE_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/entities/30000000-0000-4000-8000-000000000001/devices/3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01";

fn client_for(server: &MockServer) -> QuboClient {
    let base = server
        .uri()
        .parse()
        .expect("mock server uri parses as a URL");
    QuboClient::new(base, SP.into())
}

fn session() -> Session {
    Session {
        access_token: "token-abc".into(),
        uuid: "user-uuid-1".into(),
        email: None,
        refresh_token: None,
    }
}

fn camera() -> Device {
    Device {
        device_id: DEVICE_ID.into(),
        name: "Hall".into(),
        device_type: "HCP01".into(),
        model: "HCP01".into(),
        unit_id: UNIT.into(),
        entity_id: ENTITY.into(),
        controller_id: None,
    }
}

#[tokio::test]
async fn write_service_sends_the_verified_envelope() {
    let server = MockServer::start().await;
    Mock::given(method("PUT"))
        .and(path(format!("{DEVICE_PATH}/command")))
        .respond_with(ResponseTemplate::new(200).set_body_string("SUCCESS_UM_006"))
        .mount(&server)
        .await;

    write_service(
        &client_for(&server),
        &session(),
        &camera(),
        "panTiltControl",
        &[("horizontalVerticalPostion", "10,20")],
    )
    .await
    .expect("the ack is not the truth, but a 200 is an accepted write");

    let sent = server.received_requests().await.expect("the requests");
    assert_eq!(sent.len(), 1, "one command, one request");
    let body: Value = serde_json::from_slice(&sent[0].body).expect("the body is json");
    assert_eq!(
        body,
        json!({
            "topic": format!("/control/{UNIT}/{DEVICE_ID}/panTiltControl"),
            "payload": {
                "devices": {
                    "deviceUUID": DEVICE_ID,
                    "services": {
                        "panTiltControl": {
                            "attributes": { "horizontalVerticalPostion": "10,20" },
                            "instanceId": 0
                        }
                    }
                }
            }
        }),
        "the envelope is the one the spike proved on hardware"
    );
}

#[tokio::test]
async fn read_shadow_reads_the_services_array() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{DEVICE_PATH}/shadow")))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(include_str!("fixtures/shadow_camera.json")),
        )
        .mount(&server)
        .await;

    let shadow = read_shadow(&client_for(&server), &session(), &camera())
        .await
        .expect("the shadow reads");

    assert_eq!(
        shadow.position(),
        Some((10, -20)),
        "the pan/tilt position comes from panTiltControl.horizontalVerticalPostion"
    );
    assert_eq!(
        shadow.camera_enabled(),
        Some(false),
        "cameraFunction.state = disable means the lens is closed"
    );
    assert_eq!(
        shadow.attribute("wifiSettings", "SSIDName"),
        Some(&json!("HomeWifi")),
        "any other service stays readable, whatever the camera reports"
    );
}

#[tokio::test]
async fn a_shadow_without_a_service_reports_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{DEVICE_PATH}/shadow")))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string(r#"{"services":[{"service":"wifiSettings","attributes":{}}]}"#),
        )
        .mount(&server)
        .await;

    let shadow = read_shadow(&client_for(&server), &session(), &camera())
        .await
        .expect("the shadow reads");
    assert_eq!(
        shadow.position(),
        None,
        "no panTiltControl is no position, not a default one"
    );
    assert_eq!(
        shadow.camera_enabled(),
        None,
        "no cameraFunction is unknown, not off"
    );
}

#[tokio::test]
async fn a_position_that_is_not_a_pair_reports_unknown() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path(format!("{DEVICE_PATH}/shadow")))
        .respond_with(ResponseTemplate::new(200).set_body_string(
            r#"{"services":[{"service":"panTiltControl","attributes":{"horizontalVerticalPostion":{"value":"unknown"}}}]}"#,
        ))
        .mount(&server)
        .await;

    let shadow = read_shadow(&client_for(&server), &session(), &camera())
        .await
        .expect("the shadow reads");
    assert_eq!(shadow.position(), None, "an unparsable position is unknown");
}
