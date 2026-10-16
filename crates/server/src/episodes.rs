//! Motion episodes: the markers a recording timeline is made of.
//!
//! The cloud has no clip-list endpoint (measured 2026-10-16), so this
//! server keeps its own memory of when each camera saw motion. It is
//! deliberately not the notification bell: the bell's ring is short and
//! aged for a screen, while a timeline is browsed by day and outlives it.
//! Episodes are coalesced per camera on the same 30-second rule as the
//! bell, and aged out after a retention window so the file stays bounded.

use std::collections::VecDeque;
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::notify::now_ms;

/// One motion episode: when it began, and the still kept for it when one
/// was.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    pub id: String,
    pub device_id: String,
    pub start_ms: i64,
    pub snapshot_url: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    episodes: VecDeque<Episode>,
    #[serde(default)]
    sequence: u64,
}

/// The house's motion memory: what happened, when, newest first.
pub struct EpisodeStore {
    path: PathBuf,
    window: Duration,
    retention: Duration,
    inner: Mutex<Ledger>,
}

impl EpisodeStore {
    /// Open the store at `path`, carrying what it already holds and
    /// trimming anything past `retention`. A file that cannot be read is
    /// not fatal - the memory starts empty - but it is kept aside, never
    /// overwritten: the next save would erase whatever was in it.
    pub fn open(path: PathBuf, window: Duration, retention: Duration) -> Self {
        let mut ledger = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(ledger) => ledger,
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "the episode log is unreadable; keeping it aside and starting empty");
                    preserve(&path);
                    Ledger::default()
                }
            },
            // A missing file is a fresh start; any other read error is a
            // log that exists and would be overwritten by the next save.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ledger::default(),
            Err(err) => {
                tracing::warn!(%err, path = %path.display(), "the episode log could not be read; keeping it aside and starting empty");
                preserve(&path);
                Ledger::default()
            }
        };
        trim(&mut ledger, retention, now_ms());
        Self {
            path,
            window,
            retention,
            inner: Mutex::new(ledger),
        }
    }

    /// Remember a motion at `at_ms`, unless the camera's episode is still
    /// inside the window - a burst is one marker, and its start is where
    /// the episode began. `None` when it was coalesced.
    pub fn record(
        &self,
        device_id: &str,
        at_ms: i64,
        snapshot_url: Option<String>,
    ) -> Option<Episode> {
        let mut ledger = self.lock();
        let now = now_ms();
        if at_ms < retention_cutoff(self.retention, now) {
            // Older than the memory itself: remembering it would only
            // write an entry the next trim drops.
            return None;
        }
        let inside = ledger
            .episodes
            .iter()
            .find(|episode| episode.device_id == device_id)
            .is_some_and(|last| at_ms - last.start_ms < ms(self.window));
        if inside {
            return None;
        }
        trim(&mut ledger, self.retention, now);
        ledger.sequence += 1;
        let episode = Episode {
            id: format!("{at_ms}-{}", ledger.sequence),
            device_id: device_id.to_owned(),
            start_ms: at_ms,
            snapshot_url,
        };
        ledger.episodes.push_front(episode.clone());
        self.save(&ledger);
        Some(episode)
    }

    /// Every episode the memory holds, newest first.
    pub fn list(&self) -> Vec<Episode> {
        self.lock().episodes.iter().cloned().collect()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Ledger> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn save(&self, ledger: &Ledger) {
        let raw = serde_json::to_vec_pretty(ledger).unwrap_or_default();
        let dir = self
            .path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."));
        let path = &self.path;
        let write = || -> std::io::Result<()> {
            let mut file = tempfile::NamedTempFile::new_in(dir)?;
            file.write_all(&raw)?;
            file.persist(path).map_err(|err| err.error)?;
            Ok(())
        };
        if let Err(err) = write() {
            tracing::warn!(%err, path = %self.path.display(), "the episode log did not save");
        }
    }
}

/// Move an unreadable log aside, `x.json` to `x.<millis>.bak`, so the
/// evidence survives the next save and a later corruption cannot overwrite
/// it.
pub(crate) fn preserve(path: &std::path::Path) {
    let stamp = now_ms();
    let mut backup = path.with_extension(format!("{stamp}.bak"));
    // Two preservations inside one millisecond must not share a name.
    let mut n = 1u32;
    while backup.exists() {
        backup = path.with_extension(format!("{stamp}-{n}.bak"));
        n += 1;
    }
    if let Err(err) = std::fs::rename(path, &backup) {
        tracing::warn!(%err, path = %path.display(), "the unreadable log could not be set aside");
    }
}

/// The instant before which the memory keeps nothing: the start of the
/// household's day the retention window begins in, so retention drops
/// WHOLE days - the oldest day a picker can offer never empties through
/// the day as the clock advances.
fn retention_cutoff(retention: Duration, now_ms: i64) -> i64 {
    crate::recordings::ist_day_start(now_ms - ms(retention))
}

/// Drop everything that began before the retention window's own day.
fn trim(ledger: &mut Ledger, retention: Duration, now_ms: i64) {
    let cutoff = retention_cutoff(retention, now_ms);
    ledger.episodes.retain(|episode| episode.start_ms >= cutoff);
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

    use super::*;

    /// 16 October 2026, 12:00 IST - midday, so a mid-day cutoff and a
    /// whole-day one land in different places.
    const NOON: i64 = 1_792_132_200_000;
    const HOUR: i64 = 3_600_000;

    fn episode(start_ms: i64) -> Episode {
        Episode {
            id: format!("{start_ms}"),
            device_id: "hall".to_owned(),
            start_ms,
            snapshot_url: None,
        }
    }

    #[test]
    fn retention_drops_whole_days_only() {
        let cutoff = retention_cutoff(Duration::from_hours(48), NOON);
        assert_eq!(
            cutoff,
            crate::recordings::ist_day_start(NOON - 48 * HOUR),
            "the cutoff is the window's own day's first instant"
        );
        assert!(
            NOON - 48 * HOUR > cutoff,
            "midday sits inside the window's own day - the boundary the whole-day rule moves to"
        );

        let mut ledger = Ledger::default();
        ledger.episodes.push_back(episode(cutoff));
        ledger.episodes.push_back(episode(cutoff + 30 * 60_000));
        ledger.episodes.push_back(episode(cutoff - 60_000));
        trim(&mut ledger, Duration::from_hours(48), NOON);

        let kept: Vec<i64> = ledger.episodes.iter().map(|entry| entry.start_ms).collect();
        assert_eq!(
            kept.len(),
            2,
            "the window's own day is kept whole: {kept:?}"
        );
        assert!(
            ledger.episodes.iter().all(|entry| entry.start_ms >= cutoff),
            "and nothing before it survives"
        );
    }
}
