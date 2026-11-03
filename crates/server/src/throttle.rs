//! Repeated failure logging: one line for a cause, a count while it repeats.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an unchanged cause is counted before its next summary line.
/// An outage that lasts hours leaves a heartbeat, not a wall.
const SUMMARY_EVERY: Duration = Duration::from_mins(30);

/// Consecutive successes that end a stream's failure records: a stream
/// failing every other poll must not log a failure-and-recovery pair.
const RECOVERY_AFTER: u32 = 3;

/// A cause with no traffic for this long is forgotten, so a long outage
/// that cycles through causes does not grow the memory without bound.
const STALE_AFTER: Duration = Duration::from_hours(6);

/// The hard cap on remembered causes, whichever comes first.
const MAX_RECORDS: usize = 64;

/// What a repeated failure should log.
#[derive(Debug, PartialEq, Eq)]
pub enum Repeat {
    /// The cause is new for this stream: log it in full.
    First,
    /// The same cause again, before its summary is due: log nothing.
    Suppressed,
    /// The same cause, and the window elapsed: log the count.
    Summary { suppressed: u64 },
}

/// One cause failing on one stream. Keying by the cause, not the stream
/// alone, keeps alternating failures from replacing one another's counts.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Record {
    key: String,
    cause: String,
}

struct Entry {
    suppressed: u64,
    last_line: Instant,
    last_seen: Instant,
}

#[derive(Default)]
struct State {
    entries: HashMap<Record, Entry>,
    /// Consecutive successes per stream, for the recovery debounce.
    streaks: HashMap<String, u32>,
}

/// Per-stream, per-cause failure deduplication. A new cause logs at once,
/// an identical repeat is counted, a summary carrying the count lands every
/// `summary_every`, and recovery clears the stream's records and hands back
/// the suppressed total they held.
#[derive(Clone)]
pub struct RepeatLog {
    summary_every: Duration,
    recovery_after: u32,
    state: Arc<Mutex<State>>,
}

impl Default for RepeatLog {
    fn default() -> Self {
        Self::new(SUMMARY_EVERY, RECOVERY_AFTER)
    }
}

impl RepeatLog {
    pub fn new(summary_every: Duration, recovery_after: u32) -> Self {
        Self {
            summary_every,
            recovery_after,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    pub fn note(&self, key: &str, cause: &str) -> Repeat {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A failure interrupts any run of successes.
        state.streaks.remove(key);
        let record = Record {
            key: key.to_owned(),
            cause: cause.to_owned(),
        };
        if let Some(entry) = state.entries.get_mut(&record) {
            entry.suppressed += 1;
            entry.last_seen = now;
            if now.duration_since(entry.last_line) < self.summary_every {
                return Repeat::Suppressed;
            }
            entry.last_line = now;
            return Repeat::Summary {
                suppressed: std::mem::take(&mut entry.suppressed),
            };
        }
        prune(&mut state.entries, now);
        state.entries.insert(
            record,
            Entry {
                suppressed: 0,
                last_line: now,
                last_seen: now,
            },
        );
        Repeat::First
    }

    /// One success for `key`. After `recovery_after` consecutive ones the
    /// stream's records are cleared, and the suppressed total they held is
    /// returned for the recovery line; `None` while the streak is short or
    /// when nothing was failing.
    pub fn recovered(&self, key: &str) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.entries.keys().any(|record| record.key == key) {
            state.streaks.remove(key);
            return None;
        }
        let streak = state.streaks.entry(key.to_owned()).or_insert(0);
        *streak += 1;
        if *streak < self.recovery_after {
            return None;
        }
        state.streaks.remove(key);
        take(&mut state.entries, key)
    }

    /// Clear now and return the suppressed total: for a caller whose own
    /// quiet rule (the hold's healthy run) is the debounce.
    pub fn recovered_now(&self, key: &str) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.streaks.remove(key);
        take(&mut state.entries, key)
    }
}

fn take(entries: &mut HashMap<Record, Entry>, key: &str) -> Option<u64> {
    let mut suppressed = 0;
    let mut had = false;
    entries.retain(|record, entry| {
        if record.key == key {
            suppressed += entry.suppressed;
            had = true;
            return false;
        }
        true
    });
    had.then_some(suppressed)
}

fn prune(entries: &mut HashMap<Record, Entry>, now: Instant) {
    entries.retain(|_, entry| now.duration_since(entry.last_seen) < STALE_AFTER);
    while entries.len() >= MAX_RECORDS {
        let Some(oldest) = entries
            .iter()
            .min_by_key(|(_, entry)| entry.last_seen)
            .map(|(record, _)| record.clone())
        else {
            return;
        };
        entries.remove(&oldest);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    fn quick() -> RepeatLog {
        RepeatLog::new(Duration::from_millis(20), 3)
    }

    #[test]
    fn one_line_per_cause_with_a_periodic_count() {
        let log = quick();
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
    fn alternating_causes_keep_their_own_records() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "operation timed out"), Repeat::First);
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Suppressed,
            "a second cause does not replace the first one's record"
        );
        assert_eq!(log.note("hall", "operation timed out"), Repeat::Suppressed);
    }

    #[test]
    fn the_window_resets_after_a_summary() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Summary { suppressed: 2 }
        );
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Suppressed,
            "the next window starts empty"
        );
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Summary { suppressed: 2 },
            "the fresh summary carries only the new window's two events"
        );
    }

    #[test]
    fn a_recovery_reports_the_suppressed_total() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        assert_eq!(log.recovered("hall"), None, "one success is not a recovery");
        assert_eq!(log.recovered("hall"), None, "two are not either");
        assert_eq!(
            log.recovered("hall"),
            Some(1),
            "the third success reports the repeat it covered"
        );
        assert_eq!(log.recovered("hall"), None, "the record is gone");
    }

    #[test]
    fn a_changed_cause_keeps_the_old_count_for_the_recovery() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        assert_eq!(log.note("hall", "operation timed out"), Repeat::First);
        let mut total = None;
        for _ in 0..3 {
            total = log.recovered("hall");
        }
        assert_eq!(
            total,
            Some(2),
            "the old cause's repeats ride the recovery even after a change"
        );
        assert_eq!(log.recovered("hall"), None, "the record is gone");
    }

    #[test]
    fn a_recovery_reports_the_total_across_causes() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        assert_eq!(log.note("hall", "operation timed out"), Repeat::First);
        assert_eq!(log.note("hall", "operation timed out"), Repeat::Suppressed);
        assert_eq!(log.recovered("hall"), None);
        assert_eq!(log.recovered("hall"), None);
        assert_eq!(
            log.recovered("hall"),
            Some(2),
            "both causes' repeats ride the one recovery"
        );
    }

    #[test]
    fn one_streams_recovery_leaves_anothers_record_alone() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("porch", "connection refused"), Repeat::First);
        for _ in 0..3 {
            let _ = log.recovered("hall");
        }
        assert_eq!(
            log.note("porch", "connection refused"),
            Repeat::Suppressed,
            "porch was never confirmed healthy"
        );
    }

    #[test]
    fn a_flapping_stream_recovers_only_after_a_streak() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.recovered("hall"), None);
        assert_eq!(log.recovered("hall"), None);
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Suppressed,
            "a failure inside the streak keeps the record"
        );
        assert_eq!(log.recovered("hall"), None, "the streak restarted");
        assert_eq!(log.recovered("hall"), None);
        assert_eq!(log.recovered("hall"), Some(1));
    }

    #[test]
    fn a_stale_record_is_evicted_on_the_next_new_cause() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        {
            let mut state = log.state.lock().expect("the state");
            prune(
                &mut state.entries,
                Instant::now() + STALE_AFTER + Duration::from_secs(1),
            );
        }
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::First,
            "a cause with no traffic for the staleness window is forgotten"
        );
    }

    #[test]
    fn the_cap_evicts_the_least_recently_seen_cause() {
        let log = quick();
        for index in 0..MAX_RECORDS {
            assert_eq!(log.note("hall", &format!("cause {index}")), Repeat::First);
        }
        assert_eq!(
            log.note("hall", "one cause too many"),
            Repeat::First,
            "the cap makes room for the new cause"
        );
        assert_eq!(
            log.note("hall", "cause 0"),
            Repeat::First,
            "the least recently seen cause was evicted"
        );
        assert_eq!(
            log.note("hall", "one cause too many"),
            Repeat::Suppressed,
            "the newest cause survived the eviction"
        );
    }
}
