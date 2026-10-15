//! Device control: the command envelope the cloud accepts, and the shadow
//! read that tells the truth about what the device did.

use std::collections::HashMap;

use serde_json::{Value, json};

use crate::client::QuboClient;
use crate::devices::Device;
use crate::error::QuboError;
use crate::session::Session;

/// The attributes a command writes, each value stringified as the cloud
/// expects them.
pub type Attributes<'a> = &'a [(&'a str, &'a str)];

/// A device's shadow: what the device itself reports, keyed by service and
/// attribute.
#[derive(Debug, Clone, PartialEq)]
pub struct Shadow {
    services: HashMap<String, HashMap<String, Value>>,
}

impl Shadow {
    /// Parse the services-array shape (spec §4 E9), whose attribute values
    /// arrive either as `{"value": x}` or as a bare scalar.
    pub fn from_response(value: &Value) -> Self {
        let mut services = HashMap::new();
        let Some(entries) = value.get("services").and_then(Value::as_array) else {
            return Self { services };
        };
        for entry in entries {
            let Some(service) = entry.get("service").and_then(Value::as_str) else {
                continue;
            };
            let mut attributes = HashMap::new();
            if let Some(raw) = entry.get("attributes").and_then(Value::as_object) {
                for (name, value) in raw {
                    let value = value.get("value").unwrap_or(value);
                    if !value.is_null() {
                        attributes.insert(name.clone(), value.clone());
                    }
                }
            }
            services.insert(service.to_owned(), attributes);
        }
        Self { services }
    }

    pub fn attribute(&self, service: &str, attribute: &str) -> Option<&Value> {
        self.services.get(service)?.get(attribute)
    }

    /// The pan/tilt position as reported, `horizontalVerticalPostion`
    /// spelling and all.
    pub fn position(&self) -> Option<(i64, i64)> {
        let raw = self
            .attribute("panTiltControl", "horizontalVerticalPostion")?
            .as_str()?;
        let (x, y) = raw.split_once(',')?;
        Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
    }

    /// `cameraFunction.state`: whether the lens is open. `None` when the
    /// camera says nothing about it - unknown, not off.
    pub fn camera_enabled(&self) -> Option<bool> {
        match self.attribute("cameraFunction", "state")?.as_str()? {
            "enable" => Some(true),
            "disable" => Some(false),
            _ => None,
        }
    }
}

/// Write one service's attributes to a device. The cloud acks every shape
/// with a 200, so an `Ok` here means the write was accepted, never that the
/// device obeyed - read the shadow back.
pub async fn write_service(
    client: &QuboClient,
    session: &Session,
    device: &Device,
    service: &str,
    attributes: Attributes<'_>,
) -> Result<(), QuboError> {
    let attributes: serde_json::Map<String, Value> = attributes
        .iter()
        .map(|(name, value)| ((*name).to_owned(), json!(value)))
        .collect();
    let body = json!({
        "topic": format!("/control/{}/{}/{service}", device.unit_id, device.device_id),
        "payload": {
            "devices": {
                "deviceUUID": device.device_id,
                "services": {
                    service: { "attributes": attributes, "instanceId": 0 }
                }
            }
        }
    });
    client
        .put_ack(
            &format!(
                "/unit-entity-management/api/v1/sp/{}/units/{}/entities/{}/devices/{}/command",
                client.service_provider(),
                device.unit_id,
                device.entity_id,
                device.device_id
            ),
            &body,
            Some(session),
        )
        .await?;
    Ok(())
}

/// Read a device's shadow.
pub async fn read_shadow(
    client: &QuboClient,
    session: &Session,
    device: &Device,
) -> Result<Shadow, QuboError> {
    let response = client
        .get_json(
            &format!(
                "/unit-entity-management/api/v1/sp/{}/units/{}/entities/{}/devices/{}/shadow",
                client.service_provider(),
                device.unit_id,
                device.entity_id,
                device.device_id
            ),
            Some(session),
        )
        .await?;
    Ok(Shadow::from_response(&response))
}
