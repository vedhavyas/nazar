//! Device discovery wire tests.
//!
//! Fixture provenance (spec §4 E5/E6): `units.json` and `devices.json` are
//! synthesized from the documented key set (`unitUUID`; `deviceUUID`,
//! `deviceName`, `deviceType`, `aModelNo`, `extraData`), which the spec marks
//! inferrable - the live listing was verified 2026-10-13 but its raw JSON was
//! never recorded. `devices_alias.json` exercises the documented key aliases.
//!
//! `devices_live.json` is the 2026-10-16 live capture (7 devices: four
//! `ptzCamera3MP`/`HCP01` cameras, an `smartAirPurifierX4`/`HPH04A`, a
//! `smartAirPurifierQ200`/`HPH01A`, and the `mobile` app row). The wire's
//! own `deviceType`, `aModelNo` and `deviceName` values are kept verbatim;
//! the identities (uuids, MACs, serials) are synthesized.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use qubo::client::QuboClient;
use qubo::devices::{Device, list_devices};
use qubo::session::Session;

const SP: &str = "d10e4bfb0153496e8e8bb955f7ebe413";
const UNIT: &str = "8f14e45f-ceea-467a-9f99-3a1e0f6d9a11";
const SECOND_UNIT: &str = "2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d";
const UNITS_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units";
const DEVICES_PATH: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/8f14e45f-ceea-467a-9f99-3a1e0f6d9a11/devices";
const DEVICES_PATH_2: &str = "/unit-entity-management/api/v1/sp/d10e4bfb0153496e8e8bb955f7ebe413/units/2a3b4c5d-6e7f-4a8b-9c0d-1e2f3a4b5c6d/devices";

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

fn fixture(source: &str) -> Value {
    serde_json::from_str(source).expect("fixture is JSON")
}

async fn mount_get(server: &MockServer, route: &str, body: &str) {
    Mock::given(method("GET"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_string(body.to_owned()))
        .mount(server)
        .await;
}

#[tokio::test]
async fn list_devices_reads_the_real_device_schema() {
    let server = MockServer::start().await;
    mount_get(&server, UNITS_PATH, include_str!("fixtures/units.json")).await;
    mount_get(&server, DEVICES_PATH, include_str!("fixtures/devices.json")).await;

    let client = client_for(&server);
    let devices = list_devices(&client, &session())
        .await
        .expect("listing succeeds");

    let payload = fixture(include_str!("fixtures/devices.json"));
    assert_eq!(
        devices.len(),
        payload["data"].as_array().expect("fixture data").len(),
        "every device in the unit is listed"
    );
    for (device, raw) in devices.iter().zip(payload["data"].as_array().unwrap()) {
        assert_eq!(
            device.device_id, raw["deviceUUID"],
            "device id comes from deviceUUID"
        );
        assert_eq!(device.name, raw["deviceName"], "name comes from deviceName");
        assert_eq!(
            device.device_type, raw["deviceType"],
            "type comes from deviceType"
        );
        assert_eq!(
            device.unit_id, UNIT,
            "the unit the device was listed under is carried"
        );
        assert_eq!(
            device.entity_id, raw["entityUUID"],
            "the entity the device was listed under is carried"
        );
    }
    assert_eq!(
        devices[0].controller_id.as_deref(),
        Some("d0000000-0000-4000-8000-000000000001"),
        "the controllerUUID is read when the listing carries one"
    );
    assert_eq!(
        devices[1].controller_id, None,
        "a listing without a controllerUUID has none"
    );
}

#[tokio::test]
async fn list_devices_deduplicates_and_skips_records_without_an_id() {
    let server = MockServer::start().await;
    mount_get(&server, UNITS_PATH, &two_units()).await;
    mount_get(
        &server,
        DEVICES_PATH,
        include_str!("fixtures/devices_alias.json"),
    )
    .await;
    mount_get(
        &server,
        DEVICES_PATH_2,
        include_str!("fixtures/devices_alias.json"),
    )
    .await;

    let client = client_for(&server);
    let devices = list_devices(&client, &session())
        .await
        .expect("listing succeeds");

    let ids: Vec<&str> = devices.iter().map(|d| d.device_id.as_str()).collect();
    assert_eq!(
        ids,
        vec![
            "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01",
            "d0000000-0000-4000-8000-000000000015"
        ],
        "one entry per device: the id-less record is skipped and the shared device is listed once"
    );
    assert_eq!(
        devices[0].name, "Hall",
        "the alias key `name` is read with the `uuid` alias"
    );
    assert_eq!(
        devices[1].name, "Qubo d00000",
        "a nameless device gets a placeholder name"
    );
    assert_eq!(
        devices[1].entity_id, "default",
        "a device listed without an entity falls back to the documented placeholder"
    );
}

fn two_units() -> String {
    let units = fixture(include_str!("fixtures/units.json"));
    let mut extra = units[0].clone();
    extra["unitUUID"] = Value::String(SECOND_UNIT.to_owned());
    Value::Array(vec![units[0].clone(), extra]).to_string()
}

/// The live capture as the house sees it: the cameras of the family, and
/// nothing else. The wire's `deviceType` is the product family there, so a
/// filter that reads it finds no cameras at all and the house renders empty.
#[tokio::test]
async fn list_devices_sees_the_cameras_the_live_wire_carries() {
    let server = MockServer::start().await;
    mount_get(&server, UNITS_PATH, include_str!("fixtures/units.json")).await;
    mount_get(
        &server,
        DEVICES_PATH,
        include_str!("fixtures/devices_live.json"),
    )
    .await;

    let client = client_for(&server);
    let devices = list_devices(&client, &session())
        .await
        .expect("listing succeeds");
    assert_eq!(devices.len(), 7, "every device in the live unit is listed");

    let cameras: Vec<&str> = devices
        .iter()
        .filter(|device| device.is_camera())
        .map(|device| device.name.as_str())
        .collect();
    assert_eq!(
        cameras,
        vec!["Cam360 3MP", "First floor", "Second floor", "Front Door"],
        "the four ptzCamera3MP devices are the cameras; the purifiers and the app row are not"
    );
    for device in devices.iter().filter(|device| device.is_camera()) {
        assert_eq!(device.model, "HCP01", "each camera's model is its aModelNo");
    }
}

fn device(device_type: &str, model: &str) -> Device {
    Device {
        device_id: "3c9e1f22-5b7a-4d0e-8f21-6a2b7c8d9e01".into(),
        name: "Hall".into(),
        device_type: device_type.into(),
        model: model.into(),
        unit_id: UNIT.into(),
        entity_id: "default".into(),
        controller_id: None,
    }
}

/// The camera filter across every shape the wire has shown: the live family
/// strings, the decompile-era shape that carried the model in the type, and
/// the empty listing that must stay visible.
#[test]
fn is_camera_decides_on_the_model_the_wire_carries() {
    for (device_type, model, want) in [
        ("ptzCamera3MP", "HCP01", true),
        ("smartAirPurifierX4", "HPH04A", false),
        ("smartAirPurifierQ200", "HPH01A", false),
        ("mobile", "", false),
        ("HCP01", "", true),
        ("", "", true),
    ] {
        assert_eq!(
            device(device_type, model).is_camera(),
            want,
            "deviceType {device_type:?} / model {model:?}"
        );
    }
}
