//! The broker side of the house: Qubo's MQTT feed of device events.

use std::time::{Duration, Instant};

use qubo::{Device, QuboClient, QuboError, SessionStore, list_devices};
use rumqttc::{AsyncClient, Event, Incoming, MqttOptions, QoS, TlsConfiguration, Transport};
use serde_json::Value;

use crate::events::EventBus;
use crate::supervisor::Supervisor;

/// The broker the app itself uses, reached over TLS with the session
/// token as the password (spec §5).
const BROKER_HOST: &str = "mqtt.platform.quboworld.com";
const BROKER_PORT: u16 = 8883;
const KEEP_ALIVE: Duration = Duration::from_secs(30);
/// Bound on requests in flight; the feed subscribes once per unit at most.
const REQUEST_CAPACITY: usize = 32;

/// The device-event topic family: `/monitor/<unit>/<device>/<service>`.
const MONITOR_PREFIX: &str = "/monitor/";

/// How the feed paces itself and when it calls itself deaf.
#[derive(Debug, Clone)]
pub struct FeedConfig {
    pub host: String,
    pub port: u16,
    pub tls: bool,
    /// How long the feed may hear nothing before it drops the connection
    /// and reconnects. Heartbeats arrive about once a minute per device.
    pub quiet_window: Duration,
    /// How often a connected feed re-reads the stored session, so a fresh
    /// login reconnects onto the new token.
    pub session_check: Duration,
    pub retry_min: Duration,
    pub retry_max: Duration,
}

impl Default for FeedConfig {
    fn default() -> Self {
        Self {
            host: BROKER_HOST.to_owned(),
            port: BROKER_PORT,
            tls: true,
            quiet_window: Duration::from_secs(300),
            session_check: Duration::from_secs(30),
            retry_min: Duration::from_secs(2),
            retry_max: Duration::from_secs(120),
        }
    }
}

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
        supervisor: Supervisor,
        events: EventBus,
        config: FeedConfig,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(run(client, sessions, supervisor, events, config))
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

async fn run(
    client: QuboClient,
    sessions: SessionStore,
    supervisor: Supervisor,
    events: EventBus,
    config: FeedConfig,
) {
    let mut backoff = config.retry_min;
    loop {
        match feed(&client, &sessions, &supervisor, &events, &config).await {
            // A deliberate reconnect (the session moved, or the house went
            // quiet): straight back on, no backoff.
            Ok(()) => continue,
            Err(err @ MqttError::NoSession) => {
                tracing::debug!(%err, "mqtt feed idles until a login lands");
                backoff = config.retry_min;
            }
            Err(err) => {
                tracing::warn!(%err, "mqtt feed down; reconnecting");
                backoff = (backoff * 2).min(config.retry_max);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

/// One connection's life: resolve the units, connect, and pump events into
/// the bus until the broker drops us, the session moves under us, or the
/// house has been quiet long enough to be suspicious.
///
/// The broker accepts a connection on an expired token and then delivers
/// nothing (measured 2026-10-15), so silence - not a refusal - is what a
/// dead session looks like from here. The reconnect is what tells the two
/// apart: it re-resolves the units over REST, where an expired token is an
/// error the login flag can carry.
async fn feed(
    client: &QuboClient,
    sessions: &SessionStore,
    supervisor: &Supervisor,
    events: &EventBus,
    config: &FeedConfig,
) -> Result<(), MqttError> {
    let Some(session) = sessions.load()? else {
        return Err(MqttError::NoSession);
    };
    let units = match list_devices(client, &session).await {
        Ok(devices) => units_of(&devices),
        Err(err) => {
            if matches!(err, QuboError::Auth { .. }) {
                supervisor.mark_needs_login();
            }
            return Err(err.into());
        }
    };
    let Some(first) = units.first() else {
        return Err(MqttError::NoUnits);
    };

    let mut options = MqttOptions::new(client_id(first), &config.host, config.port);
    options.set_credentials(session.uuid.clone(), session.access_token.clone());
    options.set_keep_alive(KEEP_ALIVE);
    options.set_transport(if config.tls {
        Transport::Tls(TlsConfiguration::default())
    } else {
        Transport::Tcp
    });
    let (broker, mut stream) = AsyncClient::new(options, REQUEST_CAPACITY);
    for unit in &units {
        broker
            .subscribe(format!("{MONITOR_PREFIX}{unit}/#"), QoS::AtLeastOnce)
            .await
            .map_err(|err| MqttError::Subscribe(err.to_string()))?;
    }
    tracing::info!(units = units.len(), "mqtt feed connected");

    let mut last_heard = Instant::now();
    let mut check = tokio::time::interval(config.session_check);
    loop {
        tokio::select! {
            polled = stream.poll() => match polled {
                Ok(Event::Incoming(Incoming::Publish(message))) => {
                    last_heard = Instant::now();
                    let Ok(payload) = std::str::from_utf8(&message.payload) else {
                        continue;
                    };
                    if let Some(event) = parse_monitor_message(&message.topic, payload) {
                        events.publish(event);
                    }
                }
                Ok(_) => {}
                Err(err) => return Err(MqttError::Connection(err.to_string())),
            },
            _ = check.tick() => {
                if last_heard.elapsed() >= config.quiet_window {
                    tracing::warn!(
                        quiet = %last_heard.elapsed().as_secs(),
                        "the monitor feed has gone quiet; reconnecting"
                    );
                    return Ok(());
                }
                if let Some(current) = sessions.load()?
                    && current.access_token != session.access_token
                {
                    tracing::info!("the stored session changed; reconnecting the feed");
                    return Ok(());
                }
            }
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
