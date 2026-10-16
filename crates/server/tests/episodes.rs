//! The motion memory a recording timeline reads: bursts that are one
//! marker, a retention window that ages old days out, and a store that
//! outlives its process.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use server::episodes::EpisodeStore;
use server::notify::COALESCE;

const DAY: Duration = Duration::from_hours(24);
const KEEP_30_DAYS: Duration = Duration::from_hours(30 * 24);

static NEXT_DIR: AtomicUsize = AtomicUsize::new(0);

fn temp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nazar-{name}-{}-{}",
        std::process::id(),
        NEXT_DIR.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).expect("a scratch dir");
    dir
}

fn store_in(dir: &std::path::Path) -> EpisodeStore {
    EpisodeStore::open(dir.join("episodes.json"), COALESCE, KEEP_30_DAYS)
}

fn now_ms() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("a clock")
            .as_millis(),
    )
    .expect("milliseconds fit an i64")
}

#[test]
fn a_burst_inside_the_window_is_one_marker() {
    let dir = temp_dir("episodes-burst");
    let store = store_in(&dir);
    let start = now_ms();

    let first = store.record("hall", start, None);
    assert!(first.is_some(), "the first sighting is a marker");

    for secs in [1, 5, 29] {
        assert!(
            store.record("hall", start + secs * 1_000, None).is_none(),
            "a sighting inside the window is the same episode"
        );
    }
    assert_eq!(store.list().len(), 1, "one burst, one marker");

    let later = store.record("hall", start + 31_000, None);
    let later = later.expect("past the window, a fresh episode");
    assert_eq!(
        later.start_ms,
        start + 31_000,
        "and it is marked where it began"
    );

    // Another camera's motion is its own marker, however close in time.
    assert!(
        store.record("gate", start + 31_000, None).is_some(),
        "a camera's burst is its own episode"
    );
    assert_eq!(store.list().len(), 3);
    let _ = std::fs::remove_dir_all(&dir);
}

/// The two stores must agree on the edge: a sighting exactly one window
/// after the last is a new episode in both.
#[test]
fn the_exact_window_edge_is_a_new_marker() {
    let dir = temp_dir("episodes-edge");
    let store = store_in(&dir);
    let start = now_ms();

    assert!(store.record("hall", start, None).is_some());
    assert!(
        store.record("hall", start + 30_000, None).is_some(),
        "at exactly the window edge the episode is over"
    );
    assert!(
        store.record("hall", start + 59_999, None).is_none(),
        "and a sighting just inside the next window is that episode"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_episode_past_its_retention_is_forgotten() {
    let dir = temp_dir("episodes-retention");
    let store = store_in(&dir);
    let now = now_ms();

    // An episode from forty days back cannot be recorded into a
    // thirty-day memory... it lands only to be trimmed at once, so it is
    // simply not there.
    store.record("hall", now - 40 * 86_400_000, None);
    assert!(
        store
            .list()
            .iter()
            .all(|episode| episode.start_ms > now - 30 * 86_400_000),
        "nothing older than the window is kept: {:?}",
        store.list()
    );

    store.record("hall", now - 2 * 86_400_000, None);
    store.record("gate", now, None);
    assert_eq!(store.list().len(), 2, "inside the window, both are kept");

    // A restart trims what the days since carried past the window.
    let aged = EpisodeStore::open(
        dir.join("episodes.json"),
        COALESCE,
        DAY, // a one-day memory, as a tighter retention would be
    );
    assert!(
        aged.list()
            .iter()
            .all(|episode| episode.start_ms > now - 86_400_000),
        "reopening past the window ages the old episodes out: {:?}",
        aged.list()
    );

    // The in-place trim's whole-day semantics live in the store's own unit
    // tests (`retention_drops_whole_days_only`): a sub-day retention cannot
    // be observed through this clock, since retention now drops whole days.
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unreadable_log_is_kept_aside_not_overwritten() {
    let dir = temp_dir("episodes-broken");
    let path = dir.join("episodes.json");
    std::fs::write(&path, "{ not json at all").expect("a broken log");

    let store = EpisodeStore::open(path.clone(), COALESCE, KEEP_30_DAYS);
    assert_eq!(store.list().len(), 0, "an unreadable log starts empty");

    let backup = dir.join("episodes.bak");
    assert_eq!(
        std::fs::read_to_string(&backup).expect("the original is kept aside"),
        "{ not json at all",
        "the unreadable bytes survive: the next save must not erase whatever they were"
    );

    // And the store writes again, beside the kept original.
    store.record("hall", now_ms(), None);
    assert!(path.exists(), "the fresh log lands where the store expects it");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_memory_survives_a_restart() {
    let dir = temp_dir("episodes-restart");
    {
        let store = store_in(&dir);
        store.record("hall", now_ms(), Some("/api/snaps/e1".to_owned()));
    }
    let reopened = store_in(&dir);
    let episodes = reopened.list();
    assert_eq!(episodes.len(), 1, "the marker outlived its process");
    assert_eq!(episodes[0].device_id, "hall");
    assert_eq!(episodes[0].snapshot_url.as_deref(), Some("/api/snaps/e1"));

    // A marker recorded after the restart still gets its own id.
    let next = reopened
        .record("hall", now_ms() + 60_000, None)
        .expect("a fresh episode");
    assert_ne!(
        next.id, episodes[0].id,
        "ids do not repeat across a restart"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
