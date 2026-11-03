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

/// A cause with no traffic for this long is forgotten.
const STALE_AFTER: Duration = Duration::from_hours(6);

/// How many causes one stream may remember: the cap is local, so one noisy
/// stream cannot evict another stream's records.
const MAX_PER_STREAM: usize = 8;

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
pub(crate) struct RepeatLog {
    summary_every: Duration,
    recovery_after: u32,
    stale_after: Duration,
    state: Arc<Mutex<State>>,
}

impl Default for RepeatLog {
    fn default() -> Self {
        Self::new(SUMMARY_EVERY, RECOVERY_AFTER, STALE_AFTER)
    }
}

impl RepeatLog {
    pub(crate) fn new(summary_every: Duration, recovery_after: u32, stale_after: Duration) -> Self {
        Self {
            summary_every,
            recovery_after,
            stale_after,
            state: Arc::new(Mutex::new(State::default())),
        }
    }

    pub(crate) fn note(&self, key: &str, cause: &str) -> Repeat {
        let now = Instant::now();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // A failure interrupts any run of successes.
        state.streaks.remove(key);
        // A cause with no traffic inside the staleness window is forgotten,
        // so its return reads as a first failure, not a summary.
        drop_stale(&mut state.entries, now, self.stale_after);
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
        evict_within_key(&mut state.entries, key);
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
    /// when nothing was failing. A record older than the staleness window
    /// is already dead and clears nothing.
    pub(crate) fn recovered(&self, key: &str) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop_stale(&mut state.entries, Instant::now(), self.stale_after);
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
    pub(crate) fn recovered_now(&self, key: &str) -> Option<u64> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.streaks.remove(key);
        drop_stale(&mut state.entries, Instant::now(), self.stale_after);
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

/// Forget every record with no traffic inside the window. A record whose
/// count never reached a summary or a recovery is a loss, so it is said.
fn drop_stale(entries: &mut HashMap<Record, Entry>, now: Instant, stale_after: Duration) {
    entries.retain(|record, entry| {
        let stale = now.duration_since(entry.last_seen) >= stale_after;
        if stale && entry.suppressed > 0 {
            tracing::warn!(
                stream = %record.key,
                cause = %record.cause,
                suppressed = entry.suppressed,
                "a stale failure record is dropped"
            );
        }
        !stale
    });
}

/// Make room for one more cause of `key`. The cap is per stream: one noisy
/// stream cannot evict another's records. A record with nothing pending is
/// preferred for eviction; a count that would be lost is said.
fn evict_within_key(entries: &mut HashMap<Record, Entry>, key: &str) {
    if entries.keys().filter(|record| record.key == key).count() < MAX_PER_STREAM {
        return;
    }
    let victim = entries
        .iter()
        .filter(|(record, _)| record.key == key)
        .min_by_key(|(_, entry)| (entry.suppressed != 0, entry.last_seen))
        .map(|(record, _)| record.clone());
    let Some(victim) = victim else {
        return;
    };
    if let Some(entry) = entries.remove(&victim)
        && entry.suppressed > 0
    {
        tracing::warn!(
            stream = %victim.key,
            cause = %victim.cause,
            suppressed = entry.suppressed,
            "a failure record is evicted at the stream's cap"
        );
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// Nothing is near a window: tests that do not exercise time must not
    /// depend on the scheduler.
    fn quick() -> RepeatLog {
        RepeatLog::new(Duration::from_secs(60), 3, Duration::from_secs(60))
    }

    /// Only the summary window is short.
    fn windowed() -> RepeatLog {
        RepeatLog::new(Duration::from_millis(20), 3, Duration::from_secs(60))
    }

    /// Only the staleness window is short.
    fn stale() -> RepeatLog {
        RepeatLog::new(Duration::from_secs(60), 3, Duration::from_millis(20))
    }

    #[test]
    fn one_line_per_cause_with_a_periodic_count() {
        let log = windowed();
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
        let log = windowed();
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
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::First,
            "a failure after the recovery logs in full again"
        );
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
    fn a_stale_cause_logs_in_full_again() {
        let log = stale();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::First,
            "a cause with no traffic for the staleness window starts over"
        );
    }

    #[test]
    fn a_stale_record_is_not_a_recovery() {
        let log = stale();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(log.recovered("hall"), None, "one success");
        assert_eq!(log.recovered("hall"), None, "two successes");
        assert_eq!(
            log.recovered("hall"),
            None,
            "a long-dead record reports no recovery"
        );
    }

    #[test]
    fn recovered_now_clears_immediately_and_reports_the_count() {
        let log = quick();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        assert_eq!(
            log.recovered_now("hall"),
            Some(1),
            "the hold's healthy run needs no streak"
        );
        assert_eq!(log.recovered_now("hall"), None, "the record is gone");
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::First,
            "the next failure logs in full"
        );
    }

    #[test]
    fn a_streams_cap_evicts_only_its_own_records() {
        let log = quick();
        for index in 0..MAX_PER_STREAM {
            assert_eq!(log.note("hall", &format!("cause {index}")), Repeat::First);
        }
        assert_eq!(log.note("porch", "connection refused"), Repeat::First);
        assert_eq!(
            log.note("hall", "one cause too many"),
            Repeat::First,
            "the cap makes room for the stream's new cause"
        );
        assert_eq!(
            log.note("hall", "cause 0"),
            Repeat::First,
            "the stream's least recently seen cause was evicted"
        );
        assert_eq!(
            log.note("porch", "connection refused"),
            Repeat::Suppressed,
            "another stream's record survives the cap"
        );
    }

    #[test]
    fn the_cap_prefers_evicting_a_record_with_nothing_pending() {
        let log = quick();
        // `cause 0` is the oldest and holds a pending count; the rest are
        // newer and hold nothing.
        assert_eq!(log.note("hall", "cause 0"), Repeat::First);
        assert_eq!(log.note("hall", "cause 0"), Repeat::Suppressed);
        for index in 1..MAX_PER_STREAM {
            assert_eq!(log.note("hall", &format!("cause {index}")), Repeat::First);
        }
        assert_eq!(log.note("hall", "one cause too many"), Repeat::First);
        assert_eq!(
            log.note("hall", "cause 0"),
            Repeat::Suppressed,
            "the record with a pending count survives"
        );
        assert_eq!(
            log.note("hall", "cause 1"),
            Repeat::First,
            "a record with nothing pending was evicted instead"
        );
    }
}
