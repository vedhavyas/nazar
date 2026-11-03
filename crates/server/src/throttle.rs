//! Repeated failure logging: one line for a cause, a count while it repeats.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an unchanged failure is counted before its next summary line.
/// An outage that lasts hours leaves a heartbeat, not a wall.
const SUMMARY_EVERY: Duration = Duration::from_mins(30);

/// What a repeated failure should log.
#[derive(Debug, PartialEq, Eq)]
pub enum Repeat {
    /// The cause is new for this key: log it in full.
    First,
    /// The same cause again, before its summary is due: log nothing.
    Suppressed,
    /// The same cause, and the window elapsed: log the count.
    Summary { suppressed: u64 },
}

struct Entry {
    signature: String,
    suppressed: u64,
    last_line: Instant,
}

/// Per-key failure deduplication. Keys are the thing that fails (a stream),
/// signatures the failure's words: a new key or a changed signature logs at
/// once, an identical repeat is counted, and a summary carrying the count
/// lands every `summary_every`. A caller reports recovery so the next
/// failure of the same kind logs in full again.
#[derive(Clone)]
pub struct RepeatLog {
    summary_every: Duration,
    entries: Arc<Mutex<HashMap<String, Entry>>>,
}

impl Default for RepeatLog {
    fn default() -> Self {
        Self::new(SUMMARY_EVERY)
    }
}

impl RepeatLog {
    pub fn new(summary_every: Duration) -> Self {
        Self {
            summary_every,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn note(&self, key: &str, signature: &str) -> Repeat {
        let now = Instant::now();
        let mut entries = self
            .entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match entries.get_mut(key) {
            Some(entry) if entry.signature == signature => {
                entry.suppressed += 1;
                if now.duration_since(entry.last_line) < self.summary_every {
                    Repeat::Suppressed
                } else {
                    entry.last_line = now;
                    Repeat::Summary {
                        suppressed: std::mem::take(&mut entry.suppressed),
                    }
                }
            }
            _ => {
                entries.insert(
                    key.to_owned(),
                    Entry {
                        signature: signature.to_owned(),
                        suppressed: 0,
                        last_line: now,
                    },
                );
                Repeat::First
            }
        }
    }

    /// The key answers again: whether it had a failure on record.
    pub fn recovered(&self, key: &str) -> bool {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(key)
            .is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_line_per_cause_with_a_periodic_count() {
        let log = RepeatLog::new(Duration::from_millis(20));
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Summary { suppressed: 2 },
            "the window elapses with both repeats counted"
        );
    }

    #[test]
    fn a_changed_cause_logs_at_once() {
        let log = RepeatLog::new(Duration::from_secs(60));
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "no route to host"), Repeat::First);
    }

    #[test]
    fn a_recovered_key_logs_its_next_failure_in_full() {
        let log = RepeatLog::new(Duration::from_secs(60));
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("porch", "connection refused"), Repeat::First);
        assert!(log.recovered("hall"), "the failure was on record");
        assert!(!log.recovered("hall"), "the record is gone");
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::First,
            "a new failure logs in full after a recovery"
        );
        assert_eq!(
            log.note("porch", "connection refused"),
            Repeat::Suppressed,
            "one stream's recovery leaves another's record alone"
        );
    }
}
