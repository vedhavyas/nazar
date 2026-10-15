//! Nazar's server binary.

use anyhow::Context as _;
use qubo::{BASE_URL, QuboClient, SessionStore};
use server::config::Config;
use server::go2rtc::Go2rtc;
use server::http::{AppState, Wiring, app};
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
    if config.trusted_proxies.is_empty() {
        tracing::warn!(
            "NAZAR_TRUSTED_PROXY is unset: every client behind the proxy shares one \
             rate-limit bucket; set it to the proxy's address or subnet"
        );
    }
    let go2rtc = Go2rtc::start(&config).context("starting go2rtc")?;
    let qubo = QuboClient::with_device_id(
        BASE_URL.parse().context("the gateway URL parses")?,
        config.service_provider.clone(),
        config.device_id(),
    );
    let sessions = SessionStore::new(config.data_dir.join("session.json"));
    let supervisor = Supervisor::new(qubo.clone(), sessions.clone(), go2rtc, Pacing::default());

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
            pin: PinGate::new(&config.data_dir),
            go2rtc_api: server::go2rtc::API_BASE.to_owned(),
            trusted_proxies: config.trusted_proxies.clone(),
            secure_cookies: !config.insecure_cookie,
        },
    );
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", PORT))
        .await
        .context("binding the service port")?;
    tracing::info!(
        "nazar listening on {}",
        listener.local_addr().context("the local address")?
    );
    axum::serve(
        listener,
        app(state).into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await
    .context("serving")?;
    Ok(())
}
