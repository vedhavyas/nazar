//! Nazar's server binary.

use anyhow::Context as _;
use qubo::{BASE_URL, QuboClient, SessionStore};
use server::config::Config;
use server::go2rtc::Go2rtc;
use server::http::{AppState, app};
use server::pin::PinGate;
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
    let qubo = QuboClient::new(
        BASE_URL.parse().context("the gateway URL parses")?,
        config.service_provider.clone(),
    );
    let sessions = SessionStore::new(config.data_dir.join("session.json"));
    let supervisor = Supervisor::new(qubo.clone(), sessions.clone(), go2rtc, Pacing::default());

    if sessions
        .load()
        .context("reading the stored session")?
        .is_some()
        && let Err(err) = supervisor.refresh_devices().await
    {
        tracing::warn!(%err, "device discovery at boot failed");
    }

    let state = AppState::new(
        env!("CARGO_PKG_VERSION"),
        sessions,
        qubo,
        supervisor,
        PinGate::new(&config.data_dir),
        server::go2rtc::API_BASE,
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
