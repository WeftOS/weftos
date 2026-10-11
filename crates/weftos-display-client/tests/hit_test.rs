//! Touch lands on the object under it; the leaf does the hit-test.

mod common;

use common::*;
use weftos_display_client::scene::{NodeId, SceneOp};
use weftos_display_client::ObjectEvent;

#[test]
fn tap_inside_region_names_that_node() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(10, 100, 100, 80, 40)), insert(button(11, 300, 100, 80, 40))]).unwrap();
    sim.tap(120, 110);
    sim.tap(379, 139);
    assert_eq!(
        drain(&mut s),
        vec![
            ObjectEvent::Tap { node: NodeId::from_raw(10) },
            ObjectEvent::Tap { node: NodeId::from_raw(11) },
        ]
    );
}

#[test]
fn tap_outside_every_region_is_silent() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(10, 100, 100, 80, 40)), insert(plain_rect(20, 400, 100, 80, 40))]).unwrap();
    sim.tap(50, 50);
    sim.tap(180, 140); // one past the right/bottom edge
    sim.tap(420, 120); // a rect with no input region
    assert!(drain(&mut s).is_empty());
    assert_eq!(sim.hit_test(420, 120), None);
}

#[test]
fn topmost_overlapping_node_wins() {
    let (mut s, sim) = open_sim();
    // Later-declared sibling in the same layer sits on top.
    s.apply(&[insert(button(1, 100, 100, 100, 100)), insert(button(2, 150, 150, 100, 100))]).unwrap();
    sim.tap(175, 175);
    sim.tap(110, 110);
    assert_eq!(
        drain(&mut s),
        vec![
            ObjectEvent::Tap { node: NodeId::from_raw(2) },
            ObjectEvent::Tap { node: NodeId::from_raw(1) },
        ]
    );
}

#[test]
fn invisible_node_is_not_hit() {
    let (mut s, sim) = open_sim();
    let mut hidden = button(5, 0, 0, 800, 480);
    hidden.style.visible = false;
    s.apply(&[insert(button(1, 100, 100, 50, 50)), insert(hidden)]).unwrap();
    sim.tap(120, 120);
    assert_eq!(drain(&mut s), vec![ObjectEvent::Tap { node: NodeId::from_raw(1) }]);
}

#[test]
fn removed_node_stops_receiving_taps() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(1, 100, 100, 50, 50))]).unwrap();
    s.apply(&[SceneOp::Remove(NodeId::from_raw(1))]).unwrap();
    sim.tap(120, 120);
    assert!(drain(&mut s).is_empty());
}
