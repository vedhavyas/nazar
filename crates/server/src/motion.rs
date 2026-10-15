//! Which camera holds the hero tile.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::events::EventBus;
use crate::mqtt::UnitEvent;

/// Record every motion the bus carries, against the arrival clock the
/// hysteresis reads: the server owns the hero decision, so the browser
/// never has to order cameras itself.
pub fn follow(events: &EventBus, order: Arc<Mutex<MotionOrder>>) -> tokio::task::JoinHandle<()> {
    let mut events = events.subscribe();
    tokio::spawn(async move {
        while let Ok(event) = events.recv().await {
            if let UnitEvent::Motion { device_id } = event {
                order
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .motion(&device_id, Instant::now());
            }
        }
    })
}

/// How much more recent a challenger must be than the incumbent, and how
/// long the hero is held, before the tile may change hands. Motion arrives
/// in bursts from several cameras; without this the hero flaps.
#[derive(Debug, Clone, Copy)]
pub struct Hysteresis {
    pub margin: Duration,
    pub cooldown: Duration,
}

impl Default for Hysteresis {
    fn default() -> Self {
        Self {
            margin: Duration::from_secs(15),
            cooldown: Duration::from_secs(30),
        }
    }
}

/// The house's motion ledger and its current hero.
#[derive(Debug, Default)]
pub struct MotionOrder {
    last: HashMap<String, Instant>,
    hero: Option<String>,
    switched_at: Option<Instant>,
}

impl MotionOrder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a camera's motion. Times are arrival times on this server:
    /// the cameras' own clocks are not comparable with each other.
    pub fn motion(&mut self, device_id: &str, at: Instant) {
        self.last.insert(device_id.to_owned(), at);
    }

    pub fn last_motion(&self, device_id: &str) -> Option<Instant> {
        self.last.get(device_id).copied()
    }

    /// The camera that holds the big tile, re-elected only when the
    /// hysteresis allows it.
    pub fn hero(&mut self, now: Instant, hysteresis: Hysteresis) -> Option<String> {
        let Some((newest, newest_at)) = self.newest() else {
            return self.hero.clone();
        };
        if self.hero.as_deref() == Some(newest.as_str()) {
            return self.hero.clone();
        }
        let incumbent_at = self
            .hero
            .as_deref()
            .and_then(|hero| self.last.get(hero).copied());
        let outranked = incumbent_at
            .is_none_or(|at| newest_at.saturating_duration_since(at) > hysteresis.margin);
        let cooled = self
            .switched_at
            .is_none_or(|at| now.saturating_duration_since(at) >= hysteresis.cooldown);
        if outranked && cooled {
            self.hero = Some(newest);
            self.switched_at = Some(now);
        }
        self.hero.clone()
    }

    /// The most recent mover, ties broken by id so the answer is stable.
    fn newest(&self) -> Option<(String, Instant)> {
        self.last
            .iter()
            .max_by(|a, b| a.1.cmp(b.1).then_with(|| a.0.cmp(b.0)))
            .map(|(device, at)| (device.clone(), *at))
    }
}
