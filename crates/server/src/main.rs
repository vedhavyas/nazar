//! Nazar's server binary.

use std::sync::{Arc, Mutex};

use anyhow::Context as _;
use qubo::{BASE_URL, QuboClient, SessionStore};
use server::config::Config;
use server::episodes::EpisodeStore;
use server::events::EventBus;
use server::go2rtc::Go2rtc;
use server::http::{AppState, Wiring, app};
use server::motion::MotionOrder;
use server::mqtt::{FeedConfig, MqttService};
use server::notify::{COALESCE, NotificationStore, Snapshots};
use server::supervisor::{Pacing, Supervisor};
use tracing_subscriber::EnvFilter;

/// The single port the container serves everything on.
const PORT: u16 = 8080;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = Config::from_env();
    let go2rtc = Go2rtc::start(&config).context("starting go2rtc")?;
    let qubo = QuboClient::with_device_id(
        BASE_URL.parse().context("the gateway URL parses")?,
        config.service_provider.clone(),
        config.device_id(),
    );
    let sessions = SessionStore::new(config.data_dir.join("session.json"));
    let supervisor = Supervisor::new(qubo.clone(), sessions.clone(), go2rtc, Pacing::default());
    let events = EventBus::new();
    let motion = Arc::new(Mutex::new(MotionOrder::new()));
    let _motion = server::motion::follow(&events, Arc::clone(&motion), &supervisor);
    let notifications = Arc::new(
        NotificationStore::open(config.data_dir.join("notifications.json"), COALESCE)
            .with_aqi(config.aqi_alert_at),
    );
    let snapshots = Snapshots::new(
        Go2rtc::connect(server::go2rtc::API_BASE.parse().context("the go2rtc URL")?)
            .context("the go2rtc client")?,
        config.data_dir.join("snaps"),
    );
    let episodes = Arc::new(EpisodeStore::open(
        config.data_dir.join("episodes.json"),
        COALESCE,
        config.episodes_retention(),
    ));
    let _notify = server::notify::start(
        &events,
        Arc::clone(&notifications),
        Arc::clone(&episodes),
        snapshots.clone(),
    );
    let _mqtt = MqttService::start(
        qubo.clone(),
        sessions.clone(),
        supervisor.clone(),
        events.clone(),
        FeedConfig::default(),
    );

    // A session that cannot be read is not fatal: the UI can serve login.
    match sessions.load() {
        Ok(Some(_)) => {
            if let Err(err) = supervisor.refresh_devices().await {
                tracing::warn!(%err, "device discovery at boot failed");
            }
        }
        Ok(None) => {}
        Err(err) => {
            tracing::warn!(%err, "the stored session could not be read; starting logged out");
        }
    }

    let state = AppState::new(
        env!("CARGO_PKG_VERSION"),
        Wiring {
            sessions,
            qubo,
            supervisor,
            go2rtc_api: server::go2rtc::API_BASE.to_owned(),
            events,
            motion,
            notifications,
            episodes,
            snapshots,
        },
    );
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT))
        .await
        .context("binding the service port")?;
    tracing::info!(
        "nazar listening on {}",
        listener.local_addr().context("the local address")?
    );
    axum::serve(listener, app(state)).await.context("serving")?;
    Ok(())
}
