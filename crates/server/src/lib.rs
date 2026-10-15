//! Nazar server: the Qubo session owner, go2rtc orchestration and the
//! HTTP surface the web UI talks to.

// `unused_crate_dependencies` is deliberately not denied here: this
// package's binary needs deps its library does not (the tracing
// subscriber, anyhow).
pub mod config;
pub mod events;
pub mod go2rtc;
pub mod http;
pub mod motion;
pub mod mqtt;
pub mod supervisor;
