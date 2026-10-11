//! ADR-117 §2 bounds: node cap, op rate, image budget. A rejected batch
//! never reaches the leaf.

mod common;

use common::*;
use weftos_display_client::scene::{NodeId, SceneOp};
use weftos_display_client::{
    AdmitAll, DisplayError, DisplayGrant, DisplaySession, DisplayTarget, GrantGate, SessionLimits,
    SessionOptions,
};

fn open_with(limits: SessionLimits) -> (DisplaySession, weftos_display_client::LeafSim) {
    let (target, sim) = DisplayTarget::sim();
    let opts = SessionOptions { limits, gate: Box::new(AdmitAll) };
    (DisplaySession::open_with(target, DisplayGrant::dev("t"), opts).unwrap(), sim)
}

#[test]
fn node_cap_is_4096_and_the_4097th_is_refused_whole() {
    let limits = SessionLimits { max_ops_per_sec: 10_000, ..SessionLimits::d1() };
    let (mut s, sim) = open_with(limits);
    let full: Vec<SceneOp> = (1..=4096).map(|i| insert(plain_rect(i, 0, 0, 1, 1))).collect();
    s.apply(&full).unwrap();
    assert_eq!(sim.node_count(), 4096);

    let err = s.apply(&[insert(plain_rect(5000, 0, 0, 1, 1)), insert(plain_rect(5001, 0, 0, 1, 1))]).unwrap_err();
    assert_eq!(err, DisplayError::NodeBudget { max: 4096, would_be: 4098 });
    assert_eq!(sim.node_count(), 4096, "nothing from the refused batch may land");
    assert!(!sim.has_node(NodeId::from_raw(5000)));

    // Updating an existing node does not count twice.
    s.apply(&[SceneOp::Update(plain_rect(1, 5, 5, 1, 1))]).unwrap();
    // Freeing one makes room for one.
    s.apply(&[SceneOp::Remove(NodeId::from_raw(1)), insert(plain_rect(5000, 0, 0, 1, 1))]).unwrap();
    assert_eq!(sim.node_count(), 4096);
}

#[test]
fn op_rate_counts_batch_children_and_refuses_overflow() {
    let limits = SessionLimits { max_ops_per_sec: 5, ..SessionLimits::d1() };
    let (mut s, sim) = open_with(limits);
    s.apply(&[SceneOp::Batch((1..=4).map(|i| insert(plain_rect(i, 0, 0, 1, 1))).collect())]).unwrap();
    let err = s.apply(&[insert(plain_rect(10, 0, 0, 1, 1)), insert(plain_rect(11, 0, 0, 1, 1))]).unwrap_err();
    assert_eq!(err, DisplayError::OpRate { max_per_sec: 5, attempted: 6 });
    assert_eq!(sim.node_count(), 4);
    // One more fits exactly.
    s.apply(&[insert(plain_rect(10, 0, 0, 1, 1))]).unwrap();
    assert_eq!(sim.node_count(), 5);
}

#[test]
fn image_budget_counts_live_bitmap_bytes() {
    let limits = SessionLimits { image_budget_bytes: 1_000, ..SessionLimits::d1() };
    let (mut s, sim) = open_with(limits);
    s.apply(&[insert(bitmap(1, 600))]).unwrap();
    assert_eq!(s.image_bytes(), 600);
    let err = s.apply(&[insert(bitmap(2, 600))]).unwrap_err();
    assert_eq!(err, DisplayError::ImageBudget { max_bytes: 1_000, would_be: 1_200 });
    assert_eq!(sim.node_count(), 1);
    // Replacing the image frees the old bytes first.
    s.apply(&[SceneOp::Update(bitmap(1, 900))]).unwrap();
    assert_eq!(s.image_bytes(), 900);
    s.apply(&[SceneOp::Remove(NodeId::from_raw(1)), insert(bitmap(2, 1_000))]).unwrap();
    assert_eq!(s.image_bytes(), 1_000);
    s.apply(&[SceneOp::Clear]).unwrap();
    assert_eq!(s.image_bytes(), 0);
}

#[test]
fn defaults_are_the_d1_numbers() {
    let (s, _sim) = open_sim();
    assert_eq!(s.limits(), SessionLimits::d1());
    assert_eq!(SessionLimits::d1().max_nodes, 4096);
}

/// The D2 seam: a gate that refuses at open, and one that revokes
/// after the session is up.
struct Deny;
impl GrantGate for Deny {
    fn admit(&mut self, _: &DisplayGrant) -> Result<(), DisplayError> {
        Err(DisplayError::Denied("test gate"))
    }
}
struct RevokeAfter(u32);
impl GrantGate for RevokeAfter {
    fn admit(&mut self, _: &DisplayGrant) -> Result<(), DisplayError> {
        Ok(())
    }
    fn still_valid(&mut self, _: &DisplayGrant) -> Result<(), DisplayError> {
        if self.0 == 0 {
            return Err(DisplayError::Denied("revoked"));
        }
        self.0 -= 1;
        Ok(())
    }
}

#[test]
fn grant_gate_hooks_are_consulted() {
    let (target, _sim) = DisplayTarget::sim();
    let opts = SessionOptions { limits: SessionLimits::d1(), gate: Box::new(Deny) };
    assert_eq!(
        DisplaySession::open_with(target, DisplayGrant::dev("t"), opts).err(),
        Some(DisplayError::Denied("test gate"))
    );

    let (target, sim) = DisplayTarget::sim();
    let opts = SessionOptions { limits: SessionLimits::d1(), gate: Box::new(RevokeAfter(1)) };
    let mut s = DisplaySession::open_with(target, DisplayGrant::dev("t"), opts).unwrap();
    s.apply(&[insert(plain_rect(1, 0, 0, 1, 1))]).unwrap();
    assert_eq!(s.apply(&[insert(plain_rect(2, 0, 0, 1, 1))]), Err(DisplayError::Denied("revoked")));
    assert!(s.is_closed());
    assert_eq!(sim.node_count(), 1, "D1 leaves the leaf's clear to D2 revocation");
}
