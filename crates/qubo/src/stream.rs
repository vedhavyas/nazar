//! Live stream minting.

use serde::Deserialize;
use serde_json::json;

use crate::client::QuboClient;
use crate::devices::Device;
use crate::error::QuboError;
use crate::session::Session;

/// The two qualities the gateway accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quality {
    High,
    Low,
}

impl Quality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::High => "high",
            Self::Low => "low",
        }
    }
}

/// A minted live stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mint {
    pub url: String,
    /// The gateway's stream session id. The response schema beyond
    /// `streamURL` is unverified (spec §6), so this key is assumed.
    pub session_id: String,
}

#[derive(Debug, Deserialize)]
struct MintResponse {
    #[serde(rename = "streamURL")]
    stream_url: String,
    #[serde(rename = "sessionId", default)]
    session_id: String,
}

/// Mint a fresh live stream URL for `device`. Sessions last about five
/// minutes; mint again rather than reusing one.
pub async fn mint_live(
    client: &QuboClient,
    session: &Session,
    device: &Device,
    quality: Quality,
) -> Result<Mint, QuboError> {
    let path = format!(
        "/stream-manager/api/v1/sp/{}/stream/unit/{}/device/{}",
        client.service_provider(),
        device.unit_id,
        device.device_id
    );
    let device_type = if device.device_type.is_empty() {
        "ptzeCamera"
    } else {
        device.device_type.as_str()
    };
    let query = [
        ("timestamp", "NOW"),
        ("type", quality.as_str()),
        ("streamSourceCamera", "primary"),
        ("deviceType", device_type),
    ];
    let body = json!({ "userAppID": client.device_id() });

    let response = client
        .post_json_query(&path, &query, &body, Some(session))
        .await?;
    let mint: MintResponse = serde_json::from_value(response)?;
    Ok(Mint {
        url: mint.stream_url,
        session_id: mint.session_id,
    })
}
