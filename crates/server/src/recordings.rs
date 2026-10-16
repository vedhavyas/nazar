//! Recordings browsing: the cloud's calendar, and a day's segments from
//! the house's own memory.
//!
//! The cloud has no clip-list endpoint (measured 2026-10-16,
//! docs/qubo-vod-verification.md): the app's own timeline is built from
//! the motion events the MQTT feed carries, and so is this server's.

use serde::Serialize;

use crate::notify::{Kind, NotificationStore};

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
pub fn clips_for(store: &NotificationStore, device_id: &str, day: &str) -> Option<Vec<Clip>> {
    let (start, end) = ist_day_bounds(day)?;
    let mut clips: Vec<Clip> = store
        .list()
        .into_iter()
        .filter(|notification| {
            notification.device_id == device_id
                && matches!(notification.kind, Kind::Motion)
                && notification.at >= start
                && notification.at < end
        })
        .map(|notification| Clip {
            id: notification.id,
            start_ms: notification.at,
            snapshot_url: notification.snapshot_url,
        })
        .collect();
    // The store's own order is its ledger's, not a promise: sort here so
    // a hand-tended or half-written ledger still lists a day in order.
    clips.sort_by_key(|clip| std::cmp::Reverse(clip.start_ms));
    Some(clips)
}

/// IST is a fixed +05:30, so a day is a constant offset from UTC midnight.
const IST_OFFSET_MS: i64 = 5 * 3_600_000 + 30 * 60_000;
const DAY_MS: i64 = 86_400_000;

/// The household's day `DD-MM-YYYY` as `[start, end)` in epoch
/// milliseconds.
fn ist_day_bounds(day: &str) -> Option<(i64, i64)> {
    let mut parts = day.split('-');
    let day = parts.next()?.parse::<i64>().ok()?;
    let month = parts.next()?.parse::<i64>().ok()?;
    let year = parts.next()?.parse::<i64>().ok()?;
    if parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    let start = days_from_civil(year, month, day) * DAY_MS - IST_OFFSET_MS;
    Some((start, start + DAY_MS))
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
    }
}
