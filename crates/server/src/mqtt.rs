//! The broker side of the house: Qubo's MQTT feed of device events.

use std::time::Duration;

use qubo::{Device, QuboClient, QuboError, SessionStore, list_devices};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, TlsConfiguration, Transport};
use serde_json::Value;

use crate::events::EventBus;

/// The broker the app itself uses, reached over TLS with the session
/// token as the password (spec §5).
const BROKER_HOST: &str = "mqtt.platform.quboworld.com";
const BROKER_PORT: u16 = 8883;
const KEEP_ALIVE: Duration = Duration::from_secs(30);
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(120);
/// Bound on requests in flight; the feed subscribes once per unit at most.
const REQUEST_CAPACITY: usize = 32;

/// The device-event topic family: `/monitor/<unit>/<device>/<service>`.
const MONITOR_PREFIX: &str = "/monitor/";

/// What the monitor feed tells the house.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnitEvent {
    /// A camera saw something move.
    Motion { device_id: String },
    /// A device reported itself present or absent.
    Liveness { device_id: String, online: bool },
}

#[derive(Debug, thiserror::Error)]
pub enum MqttError {
    #[error("no stored session yet")]
    NoSession,
    #[error("the account has no units")]
    NoUnits,
    #[error(transparent)]
    Qubo(#[from] QuboError),
    #[error("subscribing failed: {0}")]
    Subscribe(String),
    #[error("the broker connection failed: {0}")]
    Connection(String),
}

/// The house's MQTT feed.
pub struct MqttService;

impl MqttService {
    /// Spawn the feed: it connects once a session exists, re-reads the
    /// token on every reconnect, and backs off while the broker or the
    /// login is missing.
    pub fn start(
        client: QuboClient,
        sessions: SessionStore,
        events: EventBus,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(run(client, sessions, events))
    }
}

/// Parse one message off the monitor family; `None` for everything this
/// milestone does not act on. The topic names the device: the broker routes
/// events by it, so it is the identity the payload cannot contradict.
pub fn parse_monitor_message(topic: &str, payload: &str) -> Option<UnitEvent> {
    let parts: Vec<&str> = topic.split('/').collect();
    let ["", "monitor", _unit, device, service] = parts.as_slice() else {
        return None;
    };
    let value: Value = serde_json::from_str(payload).ok()?;
    let devices = value.get("devices")?;
    match *service {
        "recordingControl" => {
            let cause = devices
                .pointer("/services/recordingControl/events/stateChanged/cause")?
                .as_str()?;
            if matches!(cause, "MOTION_DETECTED" | "PERSON_DETECTED") {
                Some(UnitEvent::Motion {
                    device_id: (*device).to_owned(),
                })
            } else {
                None
            }
        }
        "heartbeat" => {
            let online = match devices.get("operationState")?.as_str()? {
                "online" => true,
                "offline" => false,
                _ => return None,
            };
            Some(UnitEvent::Liveness {
                device_id: (*device).to_owned(),
                online,
            })
        }
        _ => None,
    }
}

async fn run(client: QuboClient, sessions: SessionStore, events: EventBus) {
    let mut backoff = RETRY_MIN;
    loop {
        match feed(&client, &sessions, &events).await {
            Ok(()) => backoff = RETRY_MIN,
            Err(err @ MqttError::NoSession) => {
                tracing::debug!(%err, "mqtt feed idles until a login lands");
                backoff = RETRY_MIN;
            }
            Err(err) => {
                tracing::warn!(%err, "mqtt feed down; reconnecting");
                backoff = (backoff * 2).min(RETRY_MAX);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// One connection's life: resolve the units, connect, and pump events into
/// the bus until the broker drops us.
async fn feed(
    client: &QuboClient,
    sessions: &SessionStore,
    events: &EventBus,
) -> Result<(), MqttError> {
    let Some(session) = sessions.load()? else {
        return Err(MqttError::NoSession);
    };
    let units = units_of(&list_devices(client, &session).await?);
    let Some(first) = units.first() else {
        return Err(MqttError::NoUnits);
    };

    let mut options = MqttOptions::new(client_id(first), BROKER_HOST, BROKER_PORT);
    options.set_credentials(session.uuid, session.access_token);
    options.set_keep_alive(KEEP_ALIVE);
    options.set_transport(Transport::Tls(TlsConfiguration::default()));
    let (broker, mut stream) = AsyncClient::new(options, REQUEST_CAPACITY);
    for unit in &units {
        broker
            .subscribe(format!("{MONITOR_PREFIX}{unit}/#"), QoS::AtLeastOnce)
            .await
            .map_err(|err| MqttError::Subscribe(err.to_string()))?;
    }

    loop {
        match stream.poll().await {
            Ok(Event::Incoming(Incoming::Publish(message))) => {
                let Ok(payload) = std::str::from_utf8(&message.payload) else {
                    continue;
                };
                if let Some(event) = parse_monitor_message(&message.topic, payload) {
                    events.publish(event);
                }
            }
            Ok(_) => {}
            Err(err) => return Err(MqttError::Connection(err.to_string())),
        }
    }
}

fn client_id(unit: &str) -> String {
    format!("CLIENT_nazar_MainMqttClient_{unit}")
}

fn units_of(devices: &[Device]) -> Vec<String> {
    let mut units: Vec<String> = devices
        .iter()
        .map(|device| device.unit_id.clone())
        .collect();
    units.sort();
    units.dedup();
    units
}
