//! The cloud leg: the P2P allocation, the start/end call envelopes and the
//! camera's answer.
//!
//! Shapes from `docs/qubo-protocol.md` 2, 4.3, 4.5, 5.2 and 5.4; ported
//! from `reference/p2p_client.py`.

use serde::Deserialize;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::client::QuboClient;
use crate::error::QuboError;
use crate::session::Session;

/// The P2P allocation: the session identity and keys the camera is told
/// about, plus the relay endpoint when the allocation names one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Allocation {
    pub app_session_id: String,
    pub aes_key: String,
    pub aes_iv: String,
    pub server_ip: Option<String>,
    pub server_port: Option<u16>,
}

/// The allocation response's field set; see `docs/qubo-protocol.md` 5.2.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AllocationResponse {
    #[serde(default)]
    app_session_id: String,
    #[serde(default)]
    aes_key: String,
    #[serde(default, alias = "iv", alias = "aesIV")]
    aes_iv: String,
    #[serde(default, alias = "serverIp", alias = "serverIP")]
    server_ip: Option<String>,
    #[serde(default)]
    server_port: Option<u16>,
}

impl From<AllocationResponse> for Allocation {
    fn from(response: AllocationResponse) -> Self {
        Self {
            app_session_id: response.app_session_id,
            aes_key: response.aes_key,
            aes_iv: response.aes_iv,
            server_ip: response.server_ip,
            server_port: response.server_port,
        }
    }
}

/// Allocate over the native v3 route (the streaming path): the request
/// carries an empty `appSessionId`; the real id comes back in the response
/// (protocol 5.2).
pub async fn allocate_p2p(
    client: &QuboClient,
    session: &Session,
    unit: &str,
    camera: &str,
    user_app_id: &str,
    is_ptt: bool,
) -> Result<Option<Allocation>, QuboError> {
    let path = format!(
        "/stream-manager/api/v3/sp/{}/unit/{unit}/device/{camera}/p2p",
        client.service_provider()
    );
    let query = [("isPTT", if is_ptt { "true" } else { "false" })];
    let body = json!({ "userAppID": user_app_id, "appSessionId": "", "isPTT": is_ptt });
    let response = client
        .post_json_query(&path, &query, &body, Some(session))
        .await?;
    Ok(serde_json::from_value::<AllocationResponse>(response)
        .ok()
        .map(Allocation::from))
}

/// Allocate over the legacy v2 route (the local fast path). The response is
/// one record, or the record wrapped in a `data` array.
pub async fn allocate_credentials(
    client: &QuboClient,
    session: &Session,
    unit: &str,
    camera: &str,
    user_app_id: &str,
) -> Result<Option<Allocation>, QuboError> {
    let path = format!(
        "/stream-manager/api/v2/sp/{}/unit/{unit}/devices/pp2p/credentials",
        client.service_provider()
    );
    let body = json!({ "userAppID": user_app_id, "devices": [camera] });
    let response = client.post_json(&path, &body, Some(session)).await?;
    let record = match response {
        Value::Array(items) => items.into_iter().next(),
        Value::Object(ref map) => map
            .get("data")
            .and_then(Value::as_array)
            .and_then(|items| items.first().cloned())
            .or_else(|| Some(response.clone())),
        _ => None,
    };
    Ok(record
        .and_then(|record| serde_json::from_value::<AllocationResponse>(record).ok())
        .map(Allocation::from))
}

/// Fetch a device-access token for `[self -> dest]` - the preflight the app
/// performs right before every start (protocol 2). The response key is
/// [OPEN] in the reference, so the plausible spellings are tried.
pub async fn fetch_device_access_token(
    client: &QuboClient,
    session: &Session,
    unit: &str,
    self_device: &str,
    dest: &str,
) -> Result<Option<String>, QuboError> {
    let path = format!(
        "/unit-entity-management/api/v1/sp/{}/units/{unit}/device/{self_device}/device-access-token",
        client.service_provider()
    );
    let body = json!({ "src": self_device, "dest": dest });
    let response = client.post_json(&path, &body, Some(session)).await?;
    Ok([
        "deviceAccessToken",
        "device_access_token",
        "accessToken",
        "token",
    ]
    .iter()
    .find_map(|key| {
        response
            .get(*key)
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .map(str::to_owned)
    }))
}

/// `streamSourceQuality` values (protocol 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamQuality {
    Low,
    Medium,
    High,
    Auto,
}

impl StreamQuality {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Auto => "auto",
        }
    }
}

/// The transport the session prefers (protocol 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocols {
    Udp,
    Tcp,
    Both,
}

impl Protocols {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Udp => "udp",
            Self::Tcp => "tcp",
            Self::Both => "both",
        }
    }
}

/// Which media streams to request (protocol 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaStreams {
    VideoOnly,
    AudioOnly,
    Both,
    None,
}

impl MediaStreams {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::VideoOnly => "videoOnly",
            Self::AudioOnly => "audioOnly",
            Self::Both => "both",
            Self::None => "none",
        }
    }
}

/// `offerData` as the app sends it: a string carrying a JSON array of
/// candidate objects - the LAN host first, then the srflx entries in order.
pub fn build_candidate_list(host_ip: &str, port: u16, srflx: &[String]) -> String {
    let mut candidate = serde_json::Map::new();
    candidate.insert("host".to_owned(), json!(format!("{host_ip}:{port}")));
    candidate.insert("protocol".to_owned(), json!("UDP"));
    for (index, value) in srflx.iter().enumerate() {
        candidate.insert(format!("srflx_{index}"), json!(value));
    }
    Value::Array(vec![Value::Object(candidate)]).to_string()
}

/// The arguments of one start call (protocol 4.3's captured payload).
pub struct StartCall<'a> {
    pub camera: &'a str,
    pub self_device: &'a str,
    pub src_device_id: &'a str,
    pub session_id: &'a str,
    pub offer_data: &'a str,
    pub aes_key: &'a str,
    pub aes_iv: &'a str,
    pub protocols: Protocols,
    pub quality: StreamQuality,
    pub media: MediaStreams,
    pub ptt: bool,
}

/// The native `sendCommand` envelope around one `p2pStreaming` action.
fn command_envelope(
    camera: &str,
    src_device_id: &str,
    action: &str,
    parameters: &Value,
    timestamp: i64,
) -> Value {
    let mut commands = serde_json::Map::new();
    commands.insert(
        action.to_owned(),
        json!({ "parameters": parameters, "instanceId": 0 }),
    );
    json!({
        "command": {
            "devices": {
                "services": {
                    "p2pStreaming": { "commands": Value::Object(commands) }
                },
                "deviceUUID": camera,
            }
        },
        "timestamp": timestamp,
        "deviceUUID": camera,
        "srcDeviceId": src_device_id,
        "msgSequenceId": timestamp,
        "clientId": Uuid::new_v4().to_string(),
    })
}

/// The `startP2pCall` payload, byte-shape per the capture: parameters carry
/// `useTls` (null) and `playTimestamp` (""), `pttEnabled` is a JSON boolean,
/// and the top level has timestamp/deviceUUID/srcDeviceId/msgSequenceId/
/// clientId.
pub fn build_start_call(call: &StartCall<'_>) -> Value {
    let timestamp = epoch_millis();
    let parameters = json!({
        "aesIV": call.aes_iv,
        "aesKey": call.aes_key,
        "mediaStreams": call.media.as_str(),
        "offerData": call.offer_data,
        "protocols": call.protocols.as_str(),
        "pttEnabled": call.ptt,
        "sessionId": call.session_id,
        "useTls": Value::Null,
        "streamSourceQuality": call.quality.as_str(),
        "sourceType": "live",
        "playTimestamp": "",
        "mobileUUID": call.self_device,
    });
    command_envelope(
        call.camera,
        call.src_device_id,
        "startP2pCall",
        &parameters,
        timestamp,
    )
}

/// The `endP2pCall` payload - the app always tears its session down;
/// abandoning one leaves it live on the camera until it expires.
pub fn build_end_call(camera: &str, src_device_id: &str, session_id: &str) -> Value {
    let timestamp = epoch_millis();
    let parameters = json!({ "sessionId": session_id });
    command_envelope(camera, src_device_id, "endP2pCall", &parameters, timestamp)
}

fn epoch_millis() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|elapsed| i64::try_from(elapsed.as_millis()).ok())
        .unwrap_or_default()
}

/// `CLIENT_<name>_<role>_<unit>` - the app's own clientId pattern (protocol
/// 1.2/1.6). The role keeps this connection distinct from the app's own and
/// from the server's main feed.
pub fn client_id(device_name: &str, unit: &str, role: &str) -> String {
    format!("CLIENT_{device_name}_{role}_{unit}")
}

/// A candidate from the camera's `offerResponse`: the LAN host and any
/// srflx entries, in index order.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Candidate {
    pub host: Option<String>,
    pub srflx: Vec<String>,
}

impl Candidate {
    fn parse_list(text: &str) -> Vec<Self> {
        let Ok(Value::Array(items)) = serde_json::from_str::<Value>(text) else {
            return Vec::new();
        };
        items
            .iter()
            .filter_map(|item| {
                let object = item.as_object()?;
                let host = object
                    .get("host")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let mut srflx: Vec<(u32, String)> = object
                    .iter()
                    .filter_map(|(key, value)| {
                        let index = key.strip_prefix("srflx_")?.parse().ok()?;
                        Some((index, value.as_str()?.to_owned()))
                    })
                    .collect();
                srflx.sort_by_key(|(index, _)| *index);
                Some(Self {
                    host,
                    srflx: srflx.into_iter().map(|(_, value)| value).collect(),
                })
            })
            .collect()
    }
}

/// One parsed `p2pStreaming` event from the monitor feed.
#[derive(Debug, Clone, PartialEq)]
pub struct Callback {
    pub event: String,
    pub session_id: String,
    /// The camera's controllerUUID, as the app reads it.
    pub handle_name: Option<String>,
    pub candidates: Vec<Candidate>,
}

/// Parse a `/monitor/<unit>/<camera>/p2pStreaming` answer for OUR sessionId
/// (protocol 5.4): devices.services.p2pStreaming.events.<event>.parameters
/// carries `offerResponse` (a stringified candidate array) and `sessionId`.
pub fn parse_p2p_callback(topic: &str, payload: &str, session_id: &str) -> Option<Callback> {
    if !topic.contains("/p2pStreaming") {
        return None;
    }
    let value: Value = serde_json::from_str(payload).ok()?;
    let devices = value.get("devices")?;
    let events = devices
        .pointer("/services/p2pStreaming/events")?
        .as_object()?;
    for (event, entry) in events {
        let Some(parameters) = entry.get("parameters") else {
            continue;
        };
        if parameters.get("sessionId").and_then(Value::as_str) != Some(session_id) {
            continue;
        }
        let offer_response = parameters
            .get("offerResponse")
            .and_then(Value::as_str)
            .unwrap_or("[]");
        return Some(Callback {
            event: event.clone(),
            session_id: session_id.to_owned(),
            handle_name: devices
                .get("handleName")
                .and_then(Value::as_str)
                .map(str::to_owned),
            candidates: Candidate::parse_list(offer_response),
        });
    }
    None
}

/// Flatten candidates into `(ip, port)` endpoints - host first, then the
/// srflx entries - deduplicated in order.
pub fn candidate_endpoints(candidates: &[Candidate]) -> Vec<(String, u16)> {
    let mut out: Vec<(String, u16)> = Vec::new();
    for value in candidates
        .iter()
        .flat_map(|candidate| candidate.host.iter().chain(candidate.srflx.iter()))
    {
        let Some((ip, port)) = value.rsplit_once(':') else {
            continue;
        };
        let Ok(port) = port.parse::<u16>() else {
            continue;
        };
        let entry = (ip.to_owned(), port);
        if !out.contains(&entry) {
            out.push(entry);
        }
    }
    out
}
