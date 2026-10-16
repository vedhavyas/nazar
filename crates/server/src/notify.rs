//! Notifications: the house's memory of what happened, and the shapes the
//! bell reads.
//!
//! Everything here derives from the MQTT event stream (design §5): there is
//! no push infrastructure, and a notification is what this server decided
//! was worth telling the family about.

use std::collections::{HashMap, VecDeque};
use std::io::Write as _;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::broadcast;

use crate::episodes::EpisodeStore;
use crate::events::EventBus;
use crate::go2rtc::Go2rtc;
use crate::mqtt::UnitEvent;

/// How many notifications the store keeps.
const RING: usize = 200;

/// How many notifications a slow listener may fall behind before it starts
/// losing them; the list itself is the durable record.
const ANNOUNCE_CAPACITY: usize = 64;

/// A PM2.5 reading at or above this is worth a notification. The plan names
/// the kind but no number; 60 is the CPCB line between moderate and poor.
/// `NAZAR_AQI_ALERT_AT` moves the line without a rebuild.
pub const AQI_ALERT_AT: u32 = 60;

/// How long one kind's burst is one notification: the plan's 30 seconds.
pub const COALESCE: Duration = Duration::from_secs(30);

/// What happened, tagged so the web reads one union.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    Motion,
    Offline,
    Online,
    Aqi { value: u32 },
}

impl Kind {
    /// The coalescing key: a burst of one kind is one entry.
    fn key(&self) -> &'static str {
        match self {
            Self::Motion => "motion",
            Self::Offline => "offline",
            Self::Online => "online",
            Self::Aqi { .. } => "aqi",
        }
    }
}

/// One entry in the bell's list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub id: String,
    pub device_id: String,
    /// Milliseconds since the epoch, when the event reached this server.
    pub at: i64,
    pub snapshot_url: Option<String>,
    #[serde(flatten)]
    pub kind: Kind,
}

/// What the store persists: the entries and enough state to keep ids unique
/// across a restart. The coalescing clocks are deliberately not persisted -
/// after a restart a fresh burst is a fresh notification.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Ledger {
    #[serde(default)]
    entries: VecDeque<Notification>,
    #[serde(default)]
    sequence: u64,
}

struct Inner {
    ledger: Ledger,
    /// When each (device, kind) last produced an entry.
    seen: HashMap<(String, &'static str), Instant>,
    /// The entry each (device, kind) is currently in.
    by_key: HashMap<(String, &'static str), String>,
}

impl Inner {
    /// Record `event` as a new entry, unless its kind is still inside a
    /// burst already told for that device - in which case the episode's own
    /// entry moves its clock forward instead and nothing is announced.
    fn record(
        &mut self,
        event: &UnitEvent,
        now: Instant,
        window: Duration,
        aqi_alert_at: u32,
    ) -> Option<Notification> {
        let (device_id, kind) = match event {
            UnitEvent::Motion { device_id, .. } => (device_id.clone(), Kind::Motion),
            UnitEvent::Liveness { device_id, online } => (
                device_id.clone(),
                if *online { Kind::Online } else { Kind::Offline },
            ),
            UnitEvent::Aqi { device_id, value } => {
                if *value < aqi_alert_at {
                    return None;
                }
                (device_id.clone(), Kind::Aqi { value: *value })
            }
        };
        let key = (device_id.clone(), kind.key());
        if let Some(last) = self.seen.get(&key)
            && now.duration_since(*last) < window
            && let Some(id) = self.by_key.get(&key)
            && let Some(entry) = self.ledger.entries.iter_mut().find(|entry| &entry.id == id)
        {
            entry.at = now_ms();
            self.seen.insert(key, now);
            return None;
        }
        self.ledger.sequence += 1;
        let notification = Notification {
            id: format!("{}-{}", now_ms(), self.ledger.sequence),
            device_id,
            at: now_ms(),
            snapshot_url: None,
            kind,
        };
        self.ledger.entries.push_front(notification.clone());
        while self.ledger.entries.len() > RING {
            self.ledger.entries.pop_back();
        }
        self.seen.insert(key.clone(), now);
        self.by_key.insert(key, notification.id.clone());
        Some(notification)
    }
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            i64::try_from(since.as_millis()).unwrap_or(i64::MAX)
        })
}

/// The notifications the house has told, newest first.
pub struct NotificationStore {
    path: PathBuf,
    window: Duration,
    aqi_alert_at: u32,
    inner: Mutex<Inner>,
    tx: broadcast::Sender<Notification>,
}

impl NotificationStore {
    /// Open the store at `path`, carrying whatever it already holds. A file
    /// that cannot be read is not fatal: the list starts empty.
    pub fn open(path: PathBuf, window: Duration) -> Self {
        let ledger = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str(&raw) {
                Ok(ledger) => ledger,
                Err(err) => {
                    tracing::warn!(%err, path = %path.display(), "the notification log is unreadable; keeping it aside and starting empty");
                    crate::episodes::preserve(&path);
                    Ledger::default()
                }
            },
            Err(_) => Ledger::default(),
        };
        let (tx, _) = broadcast::channel(ANNOUNCE_CAPACITY);
        Self {
            path,
            window,
            aqi_alert_at: AQI_ALERT_AT,
            inner: Mutex::new(Inner {
                ledger,
                seen: HashMap::new(),
                by_key: HashMap::new(),
            }),
            tx,
        }
    }

    /// Tell at a different PM2.5 line than the default.
    #[must_use]
    pub fn with_aqi(mut self, at: u32) -> Self {
        self.aqi_alert_at = at;
        self
    }

    /// Record an event. `None` when it fell inside a burst already told.
    pub fn push(&self, event: &UnitEvent, now: Instant) -> Option<Notification> {
        let mut inner = self.lock();
        let made = inner.record(event, now, self.window, self.aqi_alert_at);
        // Only a new entry is worth a write: the coalesced branch bumped an
        // entry's `at` in memory, and the entry itself is already
        // persisted - a crash losing the bump is cosmetic, and the ring
        // write is O(ring) per sighting.
        if made.is_some() {
            self.save(&inner);
        }
        made
    }

    /// Attach the still that was resolved for an entry.
    pub fn attach(&self, id: &str, url: String) {
        let mut inner = self.lock();
        if let Some(entry) = inner.ledger.entries.iter_mut().find(|entry| entry.id == id) {
            entry.snapshot_url = Some(url);
        }
        self.save(&inner);
    }

    /// One entry, as it stands now.
    pub fn get(&self, id: &str) -> Option<Notification> {
        self.lock()
            .ledger
            .entries
            .iter()
            .find(|entry| entry.id == id)
            .cloned()
    }

    /// Every entry, newest first.
    pub fn list(&self) -> Vec<Notification> {
        self.lock().ledger.entries.iter().cloned().collect()
    }

    /// Tell whoever is watching that an entry is final.
    pub fn announce(&self, notification: &Notification) {
        let _ = self.tx.send(notification.clone());
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Notification> {
        self.tx.subscribe()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn save(&self, inner: &Inner) {
        let raw = serde_json::to_vec_pretty(&inner.ledger).unwrap_or_default();
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
            tracing::warn!(%err, path = %self.path.display(), "the notification log did not save");
        }
    }
}

/// Stills for events: the feed's own image when it was fetchable, else one
/// frame from a pipeline that is already running (design §5 - there is no
/// on-demand still endpoint on the camera).
#[derive(Clone)]
pub struct Snapshots {
    go2rtc: Go2rtc,
    dir: PathBuf,
}

impl Snapshots {
    pub fn new(go2rtc: Go2rtc, dir: PathBuf) -> Self {
        Self { go2rtc, dir }
    }

    /// A served URL for `id`'s still, grabbed only when this device already
    /// has a live pipeline: a frame is free where one is running and a
    /// spawned producer where none is.
    pub async fn grab(&self, id: &str, device_id: &str) -> Option<String> {
        let streams = match self.go2rtc.streams().await {
            Ok(streams) => streams,
            Err(err) => {
                tracing::warn!(%err, "the snapshot check could not read go2rtc");
                return None;
            }
        };
        let name = streams
            .iter()
            .find(|stream| {
                stream.live
                    && (stream.name
                        == crate::supervisor::stream_name(device_id, qubo::Quality::High)
                        || stream.name
                            == crate::supervisor::stream_name(device_id, qubo::Quality::Low))
            })
            .map(|stream| stream.name.clone())?;
        let frame = match self.go2rtc.frame(&name).await {
            Ok(frame) => frame,
            Err(err) => {
                tracing::warn!(%err, stream = %name, "no frame for the snapshot");
                return None;
            }
        };
        let dir = self.dir.clone();
        let file = dir.join(format!("{id}.jpg"));
        let written = tokio::task::spawn_blocking(move || {
            std::fs::create_dir_all(&dir)?;
            std::fs::write(&file, &frame)
        })
        .await;
        match written {
            Ok(Ok(())) => Some(format!("/api/snaps/{id}")),
            other => {
                tracing::warn!(?other, "the snapshot did not write");
                None
            }
        }
    }

    /// The still for `id`, if one was kept.
    pub async fn read(&self, id: &str) -> Option<Vec<u8>> {
        let file = self.dir.join(format!("{id}.jpg"));
        tokio::task::spawn_blocking(move || std::fs::read(file).ok())
            .await
            .ok()
            .flatten()
    }
}

/// Follow the house's events into the bell, and motion into the recording
/// timeline.
pub fn start(
    events: &EventBus,
    store: Arc<NotificationStore>,
    episodes: Arc<EpisodeStore>,
    snapshots: Snapshots,
) -> tokio::task::JoinHandle<()> {
    // Subscribed here, not inside the task: a broadcast channel only
    // delivers what arrives after the subscription.
    let mut events = events.subscribe();
    tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let Some(notification) = store.push(&event, Instant::now()) else {
                        continue;
                    };
                    // The telling goes out before the still is resolved: a
                    // grab is a go2rtc round-trip per event, and a burst
                    // waiting on those overflows the consumer. The still
                    // attaches when it lands; the list serves it.
                    store.announce(&notification);
                    let url = match &event {
                        // The feed's own still, when it was fetchable.
                        UnitEvent::Motion {
                            image: Some(image), ..
                        } => Some(image.clone()),
                        UnitEvent::Motion { device_id, .. } => {
                            snapshots.grab(&notification.id, device_id).await
                        }
                        _ => None,
                    };
                    let attached = match url {
                        Some(url) => {
                            store.attach(&notification.id, url);
                            store.get(&notification.id).unwrap_or(notification)
                        }
                        None => notification,
                    };
                    // The timeline keeps its own, longer memory of motion -
                    // the bell's ring is a screen's worth, not a day's.
                    if matches!(attached.kind, Kind::Motion) {
                        episodes.record(
                            &attached.device_id,
                            attached.at,
                            attached.snapshot_url.clone(),
                        );
                    }
                }
                Err(broadcast::error::RecvError::Lagged(missed)) => {
                    tracing::warn!(missed, "the notification service fell behind a burst");
                }
                Err(broadcast::error::RecvError::Closed) => return,
            }
        }
    })
}
