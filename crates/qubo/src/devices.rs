//! Device discovery: the account's units, then the devices under each.

use std::collections::HashSet;

use serde_json::Value;

use crate::client::QuboClient;
use crate::error::QuboError;
use crate::session::Session;

/// A device registered under a unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    pub device_id: String,
    pub name: String,
    pub device_type: String,
    pub model: String,
    pub unit_id: String,
    pub entity_id: String,
    /// The device's controllerUUID, when the listing carries one. The P2P
    /// device-access-token dest needs exactly this, never the deviceUUID
    /// (the capture's tokens all carry `dest` = the target's controller).
    pub controller_id: Option<String>,
}

impl Device {
    /// Whether this is one of the camera families. The live wire's
    /// `deviceType` is the product family (`ptzCamera3MP`,
    /// `smartAirPurifierX4`, `mobile`), so the model - `aModelNo`, e.g.
    /// `HCP01` - is what decides; the type only decides for older shapes
    /// that carried the model in it. The model prefixes are the reference
    /// client's (HCP, HCO, HCD: `qubopy/devices/camera.py`); a listing that
    /// says nothing about the device keeps it, because hiding a camera is
    /// far worse than showing a purifier.
    pub fn is_camera(&self) -> bool {
        if !self.model.is_empty() {
            return self.model.starts_with("HC");
        }
        self.device_type.is_empty() || self.device_type.starts_with("HC")
    }
}

/// Every device under every unit of the account, one entry per device.
pub async fn list_devices(
    client: &QuboClient,
    session: &Session,
) -> Result<Vec<Device>, QuboError> {
    let mut devices = Vec::new();
    let mut seen = HashSet::new();

    for unit in list_units(client, session).await? {
        let Some(unit_id) = string_of(&unit, &["unitUUID", "uuid", "unitId"]) else {
            tracing::warn!(
                shape = %shape_of(&unit),
                "unit record without an id was skipped"
            );
            continue;
        };
        let path = format!(
            "/unit-entity-management/api/v1/sp/{}/units/{unit_id}/devices",
            client.service_provider()
        );
        let response = client.get_json(&path, Some(session)).await?;
        for value in items_of(&response, &["data", "devices"]) {
            let Some(device) = device_of(&value, &unit_id) else {
                tracing::warn!(
                    unit = %unit_id,
                    shape = %shape_of(&value),
                    "device record without an id was skipped"
                );
                continue;
            };
            if seen.insert(device.device_id.clone()) {
                devices.push(device);
            }
        }
    }
    Ok(devices)
}

async fn list_units(client: &QuboClient, session: &Session) -> Result<Vec<Value>, QuboError> {
    let path = format!(
        "/unit-entity-management/api/v1/sp/{}/units",
        client.service_provider()
    );
    let response = client.get_json(&path, Some(session)).await?;
    Ok(items_of(&response, &["data", "units"]))
}

/// The list may arrive bare, or wrapped under one of `keys`; a single
/// object is a one-element list.
fn items_of(response: &Value, keys: &[&str]) -> Vec<Value> {
    match response {
        Value::Array(items) => items.clone(),
        Value::Object(_) => keys
            .iter()
            .find_map(|key| response.get(*key))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_else(|| {
                // The wrapped shapes are unchecked (spec §4: the aliases are
                // inferred), so an unknown envelope must not look like data.
                tracing::warn!(
                    expected = keys.join(","),
                    shape = %shape_of(response),
                    "unexpected list envelope; treating the object as one record"
                );
                vec![response.clone()]
            }),
        other => {
            tracing::warn!(shape = %shape_of(other), "unexpected list body; no records read");
            Vec::new()
        }
    }
}

/// A compact description of a payload for a warning line.
fn shape_of(value: &Value) -> String {
    match value {
        Value::Object(map) => {
            let keys: Vec<&str> = map.keys().map(String::as_str).collect();
            format!("object with keys [{}]", keys.join(", "))
        }
        Value::Array(items) => format!("array of {}", items.len()),
        Value::String(_) => "string".to_owned(),
        Value::Number(_) => "number".to_owned(),
        Value::Bool(_) => "bool".to_owned(),
        Value::Null => "null".to_owned(),
    }
}

/// The device's key set is unverified (spec §4 E6 lists aliases); the
/// documented ones are read, and a record without an id is not a device.
fn device_of(value: &Value, unit_id: &str) -> Option<Device> {
    let device_id = string_of(value, &["deviceUUID", "deviceId", "uuid"])?;
    let name = string_of(value, &["deviceName", "name"])
        .unwrap_or_else(|| format!("Qubo {}", device_id.get(..6).unwrap_or(&device_id)));
    let device_type = string_of(value, &["deviceType", "type"]).unwrap_or_default();
    // The live listing carries the model here; the older decompile-era
    // shapes carried it in `deviceType`. Aliases match the reference
    // client's (`qubopy`).
    let model =
        string_of(value, &["aModelNo", "modelNo", "model", "deviceModel"]).unwrap_or_default();
    // Shadow and command paths carry the entity; the placeholder is the
    // reference client's fallback for a listing that omits it.
    let entity_id =
        string_of(value, &["entityUUID", "entityId"]).unwrap_or_else(|| "default".to_owned());
    let controller_id = string_of(value, &["controllerUUID", "controllerId"]);
    Some(Device {
        device_id,
        name,
        device_type,
        model,
        unit_id: unit_id.to_owned(),
        entity_id,
        controller_id,
    })
}

/// The first non-empty string among `keys`.
fn string_of(value: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        value
            .get(key)?
            .as_str()
            .filter(|text| !text.is_empty())
            .map(str::to_owned)
    })
}
