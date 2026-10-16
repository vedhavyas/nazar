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
    /// not fatal: the memory starts empty.
    pub fn open(path: PathBuf, window: Duration, retention: Duration) -> Self {
        let mut ledger = match std::fs::read_to_string(&path) {
            Ok(raw) => serde_json::from_str(&raw).unwrap_or_else(|err| {
                tracing::warn!(%err, path = %path.display(), "the episode log is unreadable; starting empty");
                Ledger::default()
            }),
            Err(_) => Ledger::default(),
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
        if at_ms < now - ms(self.retention) {
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

/// Drop everything that began before the retention window.
fn trim(ledger: &mut Ledger, retention: Duration, now_ms: i64) {
    let cutoff = now_ms - ms(retention);
    ledger.episodes.retain(|episode| episode.start_ms >= cutoff);
}

fn ms(duration: Duration) -> i64 {
    i64::try_from(duration.as_millis()).unwrap_or(i64::MAX)
}
