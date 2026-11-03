//! Nazar server: the Qubo session owner, go2rtc orchestration and the
//! HTTP surface the web UI talks to.

// `unused_crate_dependencies` is deliberately not denied here: this
// package's binary needs deps its library does not (the tracing
// subscriber, anyhow).
#[cfg(test)]
mod capture;
pub mod config;
pub mod episodes;
pub mod events;
pub mod fast_lane;
pub mod go2rtc;
pub mod http;
pub mod motion;
pub mod mqtt;
pub mod notify;
pub mod recordings;
mod redact;
pub mod supervisor;
mod throttle;
