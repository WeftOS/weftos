//! Closing a session clears its objects on the leaf and ends input.

mod common;

use common::*;
use weftos_display_client::DisplayError;

#[test]
fn close_clears_the_leaf_and_refuses_further_ops() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(1, 0, 0, 100, 100)), insert(plain_rect(2, 0, 0, 10, 10))]).unwrap();
    assert_eq!(sim.node_count(), 2);
    s.close().unwrap();
    assert!(s.is_closed());
    assert_eq!(sim.node_count(), 0);
    assert_eq!(s.node_count(), 0);
    assert_eq!(s.apply(&[insert(plain_rect(3, 0, 0, 1, 1))]), Err(DisplayError::Closed));
    assert_eq!(sim.node_count(), 0);
    // Closing twice is fine.
    s.close().unwrap();
}

#[test]
fn drop_tears_down_like_close() {
    let (target, sim) = weftos_display_client::DisplayTarget::sim();
    {
        let mut s = weftos_display_client::DisplaySession::open(target, weftos_display_client::DisplayGrant::dev("t")).unwrap();
        s.apply(&[insert(plain_rect(1, 0, 0, 1, 1))]).unwrap();
        assert_eq!(sim.node_count(), 1);
    }
    assert_eq!(sim.node_count(), 0);
}

#[test]
fn input_after_close_is_dropped() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(1, 0, 0, 100, 100))]).unwrap();
    sim.tap(10, 10);
    s.close().unwrap();
    sim.tap(10, 10);
    assert_eq!(s.poll_event(), None);
}

#[test]
fn leaf_going_away_closes_the_session() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(button(1, 0, 0, 100, 100))]).unwrap();
    sim.tap(10, 10);
    sim.disconnect();
    // The tap that happened before the drop is still delivered …
    assert!(matches!(s.poll_event(), Some(weftos_display_client::ObjectEvent::Tap { .. })));
    // … then the session notices the leaf is gone.
    assert_eq!(s.poll_event(), None);
    assert!(s.is_closed());
    assert_eq!(s.apply(&[insert(plain_rect(2, 0, 0, 1, 1))]), Err(DisplayError::Closed));
}
