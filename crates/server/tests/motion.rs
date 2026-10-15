//! Hero selection: which camera holds the big tile, and how motion bursts
//! are kept from flapping it.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::time::{Duration, Instant};

use server::motion::{Hysteresis, MotionOrder};

const HALL: &str = "hall";
const GATE: &str = "gate";

fn at(base: Instant, secs: u64) -> Instant {
    base + Duration::from_secs(secs)
}

#[test]
fn the_default_hysteresis_is_the_planned_one() {
    let hysteresis = Hysteresis::default();
    assert_eq!(
        hysteresis.margin,
        Duration::from_secs(15),
        "a challenger must be 15s more recent than the incumbent"
    );
    assert_eq!(
        hysteresis.cooldown,
        Duration::from_secs(30),
        "and the hero cannot switch twice inside 30s"
    );
}

#[test]
fn a_house_with_no_motion_has_no_hero() {
    let mut order = MotionOrder::new();
    assert_eq!(
        order.hero(Instant::now(), Hysteresis::default()),
        None,
        "nothing has moved, so no camera leads"
    );
}

#[test]
fn a_burst_does_not_flap_the_hero() {
    let base = Instant::now();
    let mut order = MotionOrder::new();
    order.motion(HALL, base);
    assert_eq!(
        order.hero(base, Hysteresis::default()),
        Some(HALL.to_owned()),
        "the first mover takes the hero"
    );

    for secs in [2, 3, 4] {
        order.motion(GATE, at(base, secs));
        assert_eq!(
            order.hero(at(base, secs), Hysteresis::default()),
            Some(HALL.to_owned()),
            "a challenger inside the margin does not take the tile"
        );
    }
    assert_eq!(
        order.last_motion(GATE),
        Some(at(base, 4)),
        "the challenger's own motion is still on record"
    );
}

#[test]
fn a_newer_mover_waits_out_the_cooldown_then_takes_the_hero() {
    let base = Instant::now();
    let mut order = MotionOrder::new();
    order.motion(HALL, base);
    order.hero(base, Hysteresis::default());

    order.motion(GATE, at(base, 20));
    assert_eq!(
        order.hero(at(base, 20), Hysteresis::default()),
        Some(HALL.to_owned()),
        "20s newer clears the margin, but the cooldown still holds the hero"
    );

    order.motion(GATE, at(base, 40));
    assert_eq!(
        order.hero(at(base, 40), Hysteresis::default()),
        Some(GATE.to_owned()),
        "past the cooldown the newer mover takes the hero"
    );
    assert_eq!(
        order.hero(at(base, 41), Hysteresis::default()),
        Some(GATE.to_owned()),
        "and the decision sticks until the hysteresis allows another"
    );
}

#[test]
fn a_challenger_exactly_at_the_margin_is_not_enough() {
    let base = Instant::now();
    let mut order = MotionOrder::new();
    order.motion(HALL, base);
    order.hero(base, Hysteresis::default());

    order.motion(GATE, at(base, 15));
    assert_eq!(
        order.hero(at(base, 30), Hysteresis::default()),
        Some(HALL.to_owned()),
        "the margin asks for strictly more recent motion, so 15s ties with the incumbent"
    );
}

#[test]
fn the_cooldown_ends_exactly_at_its_interval() {
    let base = Instant::now();
    let mut order = MotionOrder::new();
    order.motion(HALL, base);
    order.hero(base, Hysteresis::default());

    order.motion(GATE, at(base, 16));
    assert_eq!(
        order.hero(at(base, 29), Hysteresis::default()),
        Some(HALL.to_owned()),
        "a second short of the cooldown the hero holds"
    );
    assert_eq!(
        order.hero(at(base, 30), Hysteresis::default()),
        Some(GATE.to_owned()),
        "at 30s the hold has expired"
    );
}

#[test]
fn simultaneous_motion_picks_the_same_camera_every_time() {
    let base = Instant::now();
    let mut order = MotionOrder::new();
    order.motion(GATE, base);
    order.motion(HALL, base);

    assert_eq!(
        order.hero(base, Hysteresis::default()),
        Some(HALL.to_owned()),
        "a tie breaks by id"
    );
    assert_eq!(
        order.hero(base, Hysteresis::default()),
        Some(HALL.to_owned()),
        "and the pick cannot flap: the same camera answers every read"
    );
}
