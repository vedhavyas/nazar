//! Runtime configuration.

use std::fs::OpenOptions;
use std::io::Write as _;
use std::net::IpAddr;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::PathBuf;

use qubo::ServiceProvider;

/// Everything the server needs from its environment.
#[derive(Debug, Clone)]
pub struct Config {
    /// Durable state: the Qubo session, the PIN, the generated go2rtc
    /// config.
    pub data_dir: PathBuf,
    /// The go2rtc binary to run.
    pub go2rtc_bin: PathBuf,
    /// The ffmpeg binary go2rtc's sources run.
    pub ffmpeg_bin: PathBuf,
    /// The Qubo service provider.
    pub service_provider: ServiceProvider,
    /// One WebRTC candidate viewers can reach, `ip:port`. Empty means
    /// go2rtc's own discovery, which finds container-internal addresses.
    pub webrtc_candidates: Option<String>,
    /// Whose `X-Forwarded-For` the rate limiter believes.
    pub trusted_proxies: TrustedProxies,
    /// Keeps the session cookie off plain http for local runs only.
    pub insecure_cookie: bool,
}

impl Config {
    /// Read the environment: `DATA_DIR` (default `/data`), `GO2RTC_BIN`
    /// and `FFMPEG_BIN` (default to PATH lookups), `QUBO_SP`,
    /// `NAZAR_WEBRTC_CANDIDATES`, `NAZAR_TRUSTED_PROXY` (ips or v4 CIDRs,
    /// comma-separated), `NAZAR_INSECURE_COOKIE=1` for local http runs.
    pub fn from_env() -> Self {
        Self {
            data_dir: var("DATA_DIR").unwrap_or_else(|| "/data".to_owned()).into(),
            go2rtc_bin: var("GO2RTC_BIN")
                .unwrap_or_else(|| "go2rtc".to_owned())
                .into(),
            ffmpeg_bin: var("FFMPEG_BIN")
                .unwrap_or_else(|| "ffmpeg".to_owned())
                .into(),
            service_provider: var("QUBO_SP")
                .map_or_else(ServiceProvider::default, ServiceProvider::new),
            webrtc_candidates: var("NAZAR_WEBRTC_CANDIDATES"),
            trusted_proxies: var("NAZAR_TRUSTED_PROXY")
                .map_or_else(TrustedProxies::default, |spec| TrustedProxies::parse(&spec)),
            insecure_cookie: var("NAZAR_INSECURE_COOKIE").is_some_and(|value| value == "1"),
        }
    }

    /// The account's `Source-Device-Id`: created once under the data dir
    /// (0600) and reused, so restarts do not add device rows to the real
    /// account. An unwritable data dir falls back to an ephemeral id.
    pub fn device_id(&self) -> String {
        let path = self.data_dir.join("device_id");
        if let Ok(stored) = std::fs::read_to_string(&path) {
            let stored = stored.trim();
            if !stored.is_empty() {
                return stored.to_owned();
            }
        }
        let fresh = uuid::Uuid::new_v4().to_string();
        let written = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&path)
            .and_then(|mut file| file.write_all(fresh.as_bytes()));
        if let Err(err) = written {
            tracing::warn!(%err, path = %path.display(), "could not persist the device id");
        }
        fresh
    }
}

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

/// The addresses whose `X-Forwarded-For` is believed: caddy's, in
/// deployment. Anything not in here keys the rate limiter by its transport
/// peer, so a peer that can reach the server directly cannot rotate its
/// own bucket.
#[derive(Debug, Clone, Default)]
pub struct TrustedProxies {
    exact: Vec<IpAddr>,
    v4_nets: Vec<(u32, u32)>,
}

impl TrustedProxies {
    pub fn parse(spec: &str) -> Self {
        let mut proxies = Self::default();
        for entry in spec.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            match entry.split_once('/') {
                Some((base, bits)) => {
                    if let (Ok(IpAddr::V4(net)), Ok(bits)) = (base.parse(), bits.parse::<u32>())
                        && bits <= 32
                        && bits > 0
                    {
                        let mask = u32::MAX << (32 - bits);
                        proxies.v4_nets.push((u32::from(net) & mask, mask));
                    }
                }
                None => {
                    if let Ok(ip) = entry.parse() {
                        proxies.exact.push(ip);
                    }
                }
            }
        }
        proxies
    }

    pub fn trusts(&self, peer: IpAddr) -> bool {
        if self.exact.contains(&peer) {
            return true;
        }
        match peer {
            IpAddr::V4(peer) => {
                let peer = u32::from(peer);
                self.v4_nets.iter().any(|(net, mask)| (peer & mask) == *net)
            }
            IpAddr::V6(_) => false,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.exact.is_empty() && self.v4_nets.is_empty()
    }
}
