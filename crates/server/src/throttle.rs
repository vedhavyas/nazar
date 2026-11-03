//! Repeated failure logging: one line for a cause, a count while it repeats.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long an unchanged cause is counted before its next summary line.
/// An outage that lasts hours leaves a heartbeat, not a wall.
const SUMMARY_EVERY: Duration = Duration::from_mins(30);

/// Consecutive successes that end a key's failure records: a key failing
/// every other poll must not log a failure-and-recovery pair.
const RECOVERY_AFTER: u32 = 3;

/// A cause with no traffic for this long is forgotten.
const STALE_AFTER: Duration = Duration::from_hours(6);

/// How many causes one key may remember. A key is a stream at one call
/// site (the hold keeps an exit key and a spawn key), so a stream holds at
/// most twice this. The cap is local, so one noisy key cannot evict
/// another's records.
const MAX_PER_KEY: usize = 8;

/// What a repeated failure should log.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Repeat {
    /// The cause is new for this key: log it in full.
    First,
    /// The same cause again, before its summary is due: log nothing.
    Suppressed,
    /// The same cause, and the window elapsed: log the count.
    Summary { suppressed: u64 },
}

/// One cause failing on one key. Keying by the cause, not the key alone,
/// keeps alternating failures from replacing one another's counts.
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

/// A record the log had to give up, with the count it held.
struct Dropped {
    key: String,
    cause: String,
    suppressed: u64,
    why: &'static str,
}

#[derive(Default)]
struct State {
    entries: HashMap<Record, Entry>,
    /// Consecutive successes per key, for the recovery debounce.
    streaks: HashMap<String, u32>,
}

/// Per-key, per-cause failure deduplication. A new cause logs at once, an
/// identical repeat is counted, a summary carrying the count lands every
/// `summary_every`, and recovery clears the key's records and hands back
/// the suppressed total they held.
///
/// The dropped-record lines carry `key`: the caller's own key, which is a
/// stream name at the supervisor's sites and `<stream>:exit` /
/// `<stream>:spawn` for the hold. The sites' own failure lines use a
/// `stream` field, so the two are filtered differently by design.
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
        let (repeat, dropped) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let State { entries, streaks } = &mut *state;
            // A failure interrupts any run of successes.
            streaks.remove(key);
            // A cause with no traffic inside the staleness window is
            // forgotten, so its return reads as a first failure, not a
            // summary.
            let mut dropped = drop_stale(entries, streaks, now, self.stale_after);
            let record = Record {
                key: key.to_owned(),
                cause: cause.to_owned(),
            };
            let repeat = if let Some(entry) = entries.get_mut(&record) {
                entry.suppressed += 1;
                entry.last_seen = now;
                if now.duration_since(entry.last_line) < self.summary_every {
                    Repeat::Suppressed
                } else {
                    entry.last_line = now;
                    Repeat::Summary {
                        suppressed: std::mem::take(&mut entry.suppressed),
                    }
                }
            } else {
                if let Some(victim) = evict_within_key(entries, key) {
                    dropped.push(victim);
                }
                entries.insert(
                    record,
                    Entry {
                        suppressed: 0,
                        last_line: now,
                        last_seen: now,
                    },
                );
                Repeat::First
            };
            (repeat, dropped)
        };
        report(dropped);
        repeat
    }

    /// One success for `key`. After `recovery_after` consecutive ones the
    /// key's records are cleared, and the suppressed total they held is
    /// returned for the recovery line; `None` while the streak is short or
    /// when nothing was failing. A record older than the staleness window
    /// is already dead and clears nothing.
    pub(crate) fn recovered(&self, key: &str) -> Option<u64> {
        let (recovered, dropped) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let State { entries, streaks } = &mut *state;
            let now = Instant::now();
            let dropped = drop_stale(entries, streaks, now, self.stale_after);
            let recovered = if entries.keys().all(|record| record.key != key) {
                streaks.remove(key);
                None
            } else {
                let streak = {
                    let value = streaks.entry(key.to_owned()).or_insert(0);
                    *value += 1;
                    *value
                };
                if streak < self.recovery_after {
                    None
                } else {
                    streaks.remove(key);
                    take(entries, key)
                }
            };
            (recovered, dropped)
        };
        report(dropped);
        recovered
    }

    /// Clear now and return the suppressed total: for a caller whose own
    /// quiet rule (the hold's healthy run) is the debounce.
    pub(crate) fn recovered_now(&self, key: &str) -> Option<u64> {
        let (recovered, dropped) = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let State { entries, streaks } = &mut *state;
            let dropped = drop_stale(entries, streaks, Instant::now(), self.stale_after);
            streaks.remove(key);
            (take(entries, key), dropped)
        };
        report(dropped);
        recovered
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

/// Forget every record with no traffic inside the window, and the streak it
/// belonged to. A record whose count never reached a summary or a recovery
/// is a loss, so it is said.
fn drop_stale(
    entries: &mut HashMap<Record, Entry>,
    streaks: &mut HashMap<String, u32>,
    now: Instant,
    stale_after: Duration,
) -> Vec<Dropped> {
    let mut dropped = Vec::new();
    entries.retain(|record, entry| {
        let stale = now.duration_since(entry.last_seen) >= stale_after;
        if stale && entry.suppressed > 0 {
            dropped.push(Dropped {
                key: record.key.clone(),
                cause: record.cause.clone(),
                suppressed: entry.suppressed,
                why: "a stale failure record is dropped",
            });
        }
        !stale
    });
    // A streak survives while its key still holds a record: one stale
    // cause beside a fresh one must not reset the recovery debounce.
    streaks.retain(|key, _| entries.keys().any(|record| &record.key == key));
    dropped
}

/// Make room for one more cause of `key`. The cap is per key, so one noisy
/// key cannot evict another's records. A record with nothing pending is
/// preferred for eviction; a count that would be lost is said.
fn evict_within_key(entries: &mut HashMap<Record, Entry>, key: &str) -> Option<Dropped> {
    if entries.keys().filter(|record| record.key == key).count() < MAX_PER_KEY {
        return None;
    }
    let victim = entries
        .iter()
        .filter(|(record, _)| record.key == key)
        .min_by_key(|(_, entry)| (entry.suppressed != 0, entry.last_seen))
        .map(|(record, _)| record.clone())?;
    let entry = entries.remove(&victim)?;
    (entry.suppressed > 0).then_some(Dropped {
        key: victim.key,
        cause: victim.cause,
        suppressed: entry.suppressed,
        why: "a failure record is evicted at the key's cap",
    })
}

/// Say what the log had to give up, outside the lock the caller held.
fn report(dropped: Vec<Dropped>) {
    for record in dropped {
        tracing::warn!(
            key = %record.key,
            cause = %record.cause,
            suppressed = record.suppressed,
            "{}",
            record.why
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
        RepeatLog::new(Duration::from_millis(50), 3, Duration::from_secs(60))
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
        std::thread::sleep(Duration::from_millis(100));
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
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Summary { suppressed: 2 }
        );
        assert_eq!(
            log.note("hall", "connection refused"),
            Repeat::Suppressed,
            "the next window starts empty"
        );
        std::thread::sleep(Duration::from_millis(100));
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
    fn recovered_now_ignores_a_stale_record() {
        let log = stale();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(
            log.recovered_now("hall"),
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
    fn a_keys_cap_evicts_only_its_own_records() {
        let log = quick();
        // Porch first, so a global least-recently-seen policy would evict it.
        assert_eq!(log.note("porch", "connection refused"), Repeat::First);
        for index in 0..MAX_PER_KEY {
            assert_eq!(log.note("hall", &format!("cause {index}")), Repeat::First);
        }
        assert_eq!(
            log.note("hall", "one cause too many"),
            Repeat::First,
            "the cap makes room for the key's new cause"
        );
        assert_eq!(
            log.note("hall", "cause 0"),
            Repeat::First,
            "the key's least recently seen cause was evicted"
        );
        assert_eq!(
            log.note("porch", "connection refused"),
            Repeat::Suppressed,
            "another key's record survives the cap"
        );
    }

    #[test]
    fn the_cap_prefers_evicting_a_record_with_nothing_pending() {
        let log = quick();
        // `cause 0` is the oldest and holds a pending count; the rest are
        // newer and hold nothing.
        assert_eq!(log.note("hall", "cause 0"), Repeat::First);
        assert_eq!(log.note("hall", "cause 0"), Repeat::Suppressed);
        for index in 1..MAX_PER_KEY {
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

    #[test]
    fn a_stale_dropped_count_is_announced() {
        let log = stale();
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        assert_eq!(log.note("hall", "connection refused"), Repeat::Suppressed);
        let captured = crate::capture::Captured::start();
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(log.note("hall", "connection refused"), Repeat::First);
        let text = captured.text();
        assert!(
            text.contains("a stale failure record is dropped") && text.contains("suppressed=1"),
            "the dropped count is named: {text}"
        );
    }

    #[test]
    fn an_evicted_count_is_announced() {
        let log = quick();
        for index in 0..MAX_PER_KEY {
            assert_eq!(log.note("hall", &format!("cause {index}")), Repeat::First);
            assert_eq!(
                log.note("hall", &format!("cause {index}")),
                Repeat::Suppressed
            );
        }
        let captured = crate::capture::Captured::start();
        assert_eq!(log.note("hall", "one cause too many"), Repeat::First);
        let text = captured.text();
        assert!(
            text.contains("a failure record is evicted at the key's cap")
                && text.contains("suppressed=1"),
            "the evicted count is named: {text}"
        );
    }
}
