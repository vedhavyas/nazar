//! Recordings browsing: the cloud's calendar, and a day's segments from
//! the house's own motion memory.
//!
//! The cloud has no clip-list endpoint (measured 2026-10-16,
//! docs/qubo-vod-verification.md): the app's own timeline is built from
//! the motion events the MQTT feed carries, and so is this server's.

use serde::Serialize;

use crate::episodes::EpisodeStore;

/// One segment of a day: when a motion episode began, and the still kept
/// for it when one was. Playback re-mints the cloud session at
/// `start_ms`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Clip {
    pub id: String,
    pub start_ms: i64,
    pub snapshot_url: Option<String>,
}

/// The clips for `device_id` on `day` (`DD-MM-YYYY`, the household's own
/// day), newest first. `None` when the day is not a day.
pub fn clips_for(store: &EpisodeStore, device_id: &str, day: &str) -> Option<Vec<Clip>> {
    let (start, end) = ist_day_bounds(day)?;
    let mut clips: Vec<Clip> = store
        .list()
        .into_iter()
        .filter(|episode| {
            episode.device_id == device_id && episode.start_ms >= start && episode.start_ms < end
        })
        .map(|episode| Clip {
            id: episode.id,
            start_ms: episode.start_ms,
            snapshot_url: episode.snapshot_url,
        })
        .collect();
    // The store's own order is its ledger's, not a promise: sort here so
    // a hand-tended or half-written ledger still lists a day in order.
    clips.sort_by_key(|clip| std::cmp::Reverse(clip.start_ms));
    Some(clips)
}

/// IST is a fixed +05:30, so a day is a constant offset from UTC midnight.
/// This and the calendar's `timezone=Asia/Kolkata` are the same claim about
/// the account's own day: if the account ever moves, they move together.
const IST_OFFSET_MS: i64 = 5 * 3_600_000 + 30 * 60_000;
const DAY_MS: i64 = 86_400_000;

/// The household's day `DD-MM-YYYY` as `[start, end)` in epoch
/// milliseconds.
fn ist_day_bounds(day: &str) -> Option<(i64, i64)> {
    let mut parts = day.split('-');
    let day = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let year = parts.next()?.parse::<i64>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=days_in_month(year, month)).contains(&day) {
        return None;
    }
    let start = days_from_civil(year, month, day) * DAY_MS - IST_OFFSET_MS;
    Some((start, start + DAY_MS))
}

/// Days in a civil month, the Gregorian leap rule included, so a
/// well-formed date like `31-02-2026` is refused rather than silently
/// mapping to another day's window.
fn days_in_month(year: i64, month: i64) -> i64 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (year % 4 == 0 && year % 100 != 0) || year % 400 == 0 => 29,
        _ => 28,
    }
}

/// Days since the epoch for a civil date (Howard Hinnant's
/// `days_from_civil`). The server has no date library and needs exactly
/// one conversion; this one is exact for every date the calendar carries.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let y = if month <= 2 { year - 1 } else { year };
    let era = (if y >= 0 { y } else { y - 399 }) / 400;
    let yoe = y - era * 400;
    let mp = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    #[test]
    fn a_day_is_the_households_own_day() {
        assert_eq!(
            ist_day_bounds("16-10-2026"),
            Some((1_792_089_000_000, 1_792_175_400_000)),
            "midnight to midnight in IST"
        );
        assert_eq!(
            ist_day_bounds("17-10-2026"),
            Some((1_792_175_400_000, 1_792_261_800_000)),
            "the next day starts where the last one ended"
        );
        assert_eq!(
            ist_day_bounds("01-01-1970"),
            Some((-19_800_000, 66_600_000)),
            "the epoch's own day begins 5h30 before the epoch in IST"
        );
    }

    #[test]
    fn a_string_that_is_not_a_day_is_not_a_day() {
        assert_eq!(ist_day_bounds("2026-10-16"), None, "the wire's own order");
        assert_eq!(ist_day_bounds("16/10/2026"), None);
        assert_eq!(ist_day_bounds("32-10-2026"), None);
        assert_eq!(ist_day_bounds("16-13-2026"), None);
        assert_eq!(ist_day_bounds("16-10-2026-1"), None);
        // In range for the wire, but no such day: a well-formed
        // non-existent day must not silently map to another day's window.
        assert_eq!(ist_day_bounds("31-02-2026"), None, "February has 28 in 2026");
        assert_eq!(ist_day_bounds("31-04-2026"), None, "April has 30");
        assert_eq!(ist_day_bounds("30-02-2028"), None, "February has 29 in 2028");
    }
}
