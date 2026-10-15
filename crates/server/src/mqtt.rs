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
    /// How long the feed may hear no *motion* before it reconnects to be
    /// sure its subscription is live. A house nobody walks past is quiet
    /// for hours, so this is deliberately far longer than `quiet_window`.
    pub motion_window: Duration,
    /// How often a connected feed re-reads the stored session, so a fresh
    /// login reconnects onto the new token.
    pub session_check: Duration,
    /// How often a feed with no session at all looks again. Nobody is
    /// logged in yet, so this is a slow poll, not a two-second one: a
    /// login is noticed within one interval either way.
    pub idle_retry: Duration,
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
            motion_window: Duration::from_mins(30),
            session_check: Duration::from_secs(30),
            idle_retry: Duration::from_secs(10),
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

/// How a feed cycle ended, which is what the backoff reads.
#[derive(Debug, Clone, Copy)]
enum FeedEnd {
    /// Deliberate: the session moved, the house went quiet, or the broker
    /// refused the subscription. Straight back on, no backoff.
    Reconnect,
    /// The connection failed, with `healthy` saying whether it ever
    /// reached the broker: a blip after a good stretch is not a reason to
    /// start backing off from the maximum.
    Lost { healthy: bool },
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

/// The stored session, read off the runtime's worker threads: a slow file
/// there must not stall the feed's timers, let alone tear a healthy one.
async fn stored_session(sessions: &SessionStore) -> Result<Option<qubo::Session>, MqttError> {
    let sessions = sessions.clone();
    tokio::task::spawn_blocking(move || sessions.load())
        .await
        .map_err(|err| MqttError::Connection(format!("the session read panicked: {err}")))?
        .map_err(MqttError::from)
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
            Ok(FeedEnd::Reconnect) => continue,
            Ok(FeedEnd::Lost { healthy: true }) => {
                tracing::warn!("the mqtt feed dropped after a healthy stretch; reconnecting");
                backoff = config.retry_min;
            }
            Ok(FeedEnd::Lost { healthy: false }) => {
                tracing::warn!("the mqtt feed could not connect");
                backoff = (backoff * 2).min(config.retry_max);
            }
            Err(err @ MqttError::NoSession) => {
                tracing::debug!(%err, "mqtt feed idles until a login lands");
                backoff = config.idle_retry;
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
) -> Result<FeedEnd, MqttError> {
    let Some(session) = stored_session(sessions).await? else {
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
        // The platform's own TLS: rumqttc's rustls path pins a webpki line
        // with unpatched advisories, and the platform store is what the
        // broker's public certificate verifies against.
        Transport::Tls(TlsConfiguration::Native)
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
    let mut last_motion = Instant::now();
    let mut healthy = false;
    let mut check = tokio::time::interval(config.session_check);
    loop {
        tokio::select! {
            polled = stream.poll() => match polled {
                Ok(Event::Incoming(Incoming::Publish(message))) => {
                    healthy = true;
                    last_heard = Instant::now();
                    let Ok(payload) = std::str::from_utf8(&message.payload) else {
                        continue;
                    };
                    if let Some(event) = parse_monitor_message(&message.topic, payload) {
                        if matches!(event, UnitEvent::Motion { .. }) {
                            last_motion = Instant::now();
                        }
                        events.publish(event);
                    }
                }
                // A rejected subscription is not a quiet house: the broker
                // said no, and only a fresh connect (and the REST check
                // behind it) can tell an expired session from a refusal.
                Ok(Event::Incoming(Incoming::SubAck(ack))) => {
                    healthy = true;
                    let refused = ack.return_codes.iter().any(|code| {
                        !matches!(code, rumqttc::SubscribeReasonCode::Success(_))
                    });
                    if refused {
                        // A refusal is a broker-level failure, not a
                        // deliberate restart: it takes the backoff, or a
                        // broker that keeps refusing becomes a hot loop of
                        // REST calls and reconnects.
                        tracing::warn!(?ack, "the broker refused a subscription");
                        return Ok(FeedEnd::Lost { healthy: false });
                    }
                }
                Ok(_) => {}
                Err(err) => {
                    tracing::warn!(%err, "the broker connection failed");
                    return Ok(FeedEnd::Lost { healthy });
                }
            },
            _ = check.tick() => {
                if last_heard.elapsed() >= config.quiet_window {
                    tracing::warn!(
                        quiet = %last_heard.elapsed().as_secs(),
                        "the monitor feed has gone quiet; reconnecting"
                    );
                    return Ok(FeedEnd::Reconnect);
                }
                // Heartbeats can keep arriving while the motion
                // subscription is silently dead, so motion is timed on its
                // own, far coarser clock.
                if last_motion.elapsed() >= config.motion_window {
                    tracing::info!(
                        quiet = %last_motion.elapsed().as_secs(),
                        "no motion for a long while; reconnecting to be sure"
                    );
                    return Ok(FeedEnd::Reconnect);
                }
                if let Some(current) = stored_session(sessions).await?
                    && current.access_token != session.access_token
                {
                    tracing::info!("the stored session changed; reconnecting the feed");
                    return Ok(FeedEnd::Reconnect);
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
