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

/// The capture helper's own contract, tested once here: the copy the
/// integration tests include by path would otherwise run it in every
/// binary.
#[cfg(test)]
mod capture_tests {
    use crate::capture::Captured;

    /// A nested capture puts the outer buffer back when it goes.
    #[test]
    fn a_nested_capture_restores_the_outer_buffer() {
        let outer = Captured::start();
        tracing::warn!("outer one");
        let inner = Captured::start();
        tracing::warn!("inner one");
        assert!(
            inner.text().contains("inner one"),
            "the inner capture sees its own line: {}",
            inner.text()
        );
        assert!(
            !inner.text().contains("outer one"),
            "and not the outer's: {}",
            inner.text()
        );
        drop(inner);
        tracing::warn!("outer two");
        assert!(
            outer.text().contains("outer two"),
            "the outer capture is live again: {}",
            outer.text()
        );
        assert!(
            !outer.text().contains("inner one"),
            "and did not take the inner's lines: {}",
            outer.text()
        );
    }
}
