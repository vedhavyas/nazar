//! The in-process fan-out of broker events.

use tokio::sync::broadcast;

use crate::mqtt::UnitEvent;

/// How far a slow watcher may fall behind before it starts missing events:
/// a motion is a nudge, not a ledger.
const CAPACITY: usize = 256;

/// Every parsed MQTT event, broadcast to whoever is watching.
#[derive(Debug, Clone)]
pub struct EventBus {
    tx: broadcast::Sender<UnitEvent>,
}

impl EventBus {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(CAPACITY);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<UnitEvent> {
        self.tx.subscribe()
    }

    /// Publishing with nobody watching is the normal case at boot.
    pub fn publish(&self, event: UnitEvent) {
        let _ = self.tx.send(event);
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}
