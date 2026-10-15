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
    pub unit_id: String,
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
            continue;
        };
        let path = format!(
            "/unit-entity-management/api/v1/sp/{}/units/{unit_id}/devices",
            client.service_provider()
        );
        let response = client.get_json(&path, Some(session)).await?;
        for value in items_of(&response, &["data", "devices"]) {
            let Some(device) = device_of(&value, &unit_id) else {
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
            .unwrap_or_else(|| vec![response.clone()]),
        _ => Vec::new(),
    }
}

/// The device's key set is unverified (spec §4 E6 lists aliases); the
/// documented ones are read, and a record without an id is not a device.
fn device_of(value: &Value, unit_id: &str) -> Option<Device> {
    let device_id = string_of(value, &["deviceUUID", "deviceId", "uuid"])?;
    let name = string_of(value, &["deviceName", "name"])
        .unwrap_or_else(|| format!("Qubo {}", device_id.get(..6).unwrap_or(&device_id)));
    let device_type = string_of(value, &["deviceType", "type"]).unwrap_or_default();
    Some(Device {
        device_id,
        name,
        device_type,
        unit_id: unit_id.to_owned(),
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
