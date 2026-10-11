//! Ops sent through a session land on the simulated leaf and render.

mod common;

use common::*;
use weftos_display_client::scene::{NodeId, Scene, SceneOp};
use weftos_display_client::{DisplayError, DisplayGrant, DisplaySession, DisplaySize, DisplayTarget, LeafEndpoint, WindowConfig};

#[test]
fn insert_reaches_leaf_and_renders() {
    let (mut s, sim) = open_sim();
    assert_eq!(s.size(), DisplaySize::PAD);
    s.apply(&[insert(plain_rect(1, 10, 10, 50, 50))]).unwrap();
    assert_eq!(sim.node_count(), 1);
    assert!(sim.has_node(NodeId::from_raw(1)));
    assert_eq!(s.node_count(), 1);
    // The sim rendered the frame: inside the rect is the fill, outside is bg.
    assert_eq!(sim.pixel(20, 20), (0x40, 0x80, 0xC0));
    assert_eq!(sim.pixel(200, 200), (0, 0, 0));
    assert_eq!(sim.frames_rendered(), 1);
}

#[test]
fn update_remove_clear_and_replace_track_nodes() {
    let (mut s, sim) = open_sim();
    s.apply(&[insert(plain_rect(1, 0, 0, 10, 10)), insert(plain_rect(2, 20, 0, 10, 10))]).unwrap();
    s.apply(&[SceneOp::Update(plain_rect(1, 5, 5, 10, 10))]).unwrap();
    assert_eq!(sim.node_count(), 2);
    s.apply(&[SceneOp::Remove(NodeId::from_raw(2))]).unwrap();
    assert_eq!(sim.node_count(), 1);
    assert_eq!(s.node_count(), 1);

    let mut scene = Scene::empty(0);
    scene.nodes = vec![plain_rect(7, 0, 0, 1, 1), plain_rect(8, 0, 0, 1, 1), plain_rect(9, 0, 0, 1, 1)];
    s.apply(&[SceneOp::Replace(scene)]).unwrap();
    assert_eq!(sim.node_count(), 3);
    assert_eq!(s.node_count(), 3);

    s.apply(&[SceneOp::Clear]).unwrap();
    assert_eq!(sim.node_count(), 0);
    assert_eq!(s.node_count(), 0);
}

#[test]
fn batch_applies_atomically_and_counts_children() {
    let (mut s, sim) = open_sim();
    s.apply(&[SceneOp::Batch(vec![insert(plain_rect(1, 0, 0, 1, 1)), insert(plain_rect(2, 0, 0, 1, 1))])])
        .unwrap();
    assert_eq!(sim.node_count(), 2);
    assert_eq!(s.node_count(), 2);
}

#[test]
fn node_ids_are_normalised_to_the_session_display() {
    let (mut s, sim) = open_sim();
    // A node tagged for display 3 lands on display 0 (the store rewrites
    // the id, and the ledger must agree so the cap stays honest).
    let mut n = plain_rect(1, 0, 0, 1, 1);
    n.id = NodeId::from_parts(3, 1);
    s.apply(&[insert(n)]).unwrap();
    assert!(sim.has_node(NodeId::from_parts(0, 1)));
    assert_eq!(s.node_count(), 1);
}

#[test]
fn capabilities_report_touch_and_ptt() {
    let (s, _sim) = open_sim();
    let caps = s.capabilities();
    assert!(caps.input.touch);
    assert_eq!(caps.input.buttons, vec!["ptt".to_string()]);
    assert!(caps.video.is_none());
    assert!(caps.render.has_alpha());
}

#[test]
fn unbuilt_targets_say_not_yet() {
    let leaf = DisplayTarget::Leaf(LeafEndpoint {
        name: "weftos://mesh/nodes/pad/services/display".into(),
        leaf_id: "00".into(),
        display: 0,
    });
    assert!(matches!(
        DisplaySession::open(leaf, DisplayGrant::dev("t")),
        Err(DisplayError::NotYet(_))
    ));
    if !cfg!(feature = "window") {
        let win = DisplayTarget::Window(WindowConfig::default());
        assert!(matches!(
            DisplaySession::open(win, DisplayGrant::dev("t")),
            Err(DisplayError::NotYet(_))
        ));
    }
}

#[test]
fn partial_damage_clears_pixels_a_node_left_behind() {
    let (mut s, sim) = open_sim();
    let fill = (0x40, 0x80, 0xC0);
    s.apply(&[insert(plain_rect(1, 10, 10, 50, 50))]).unwrap();
    assert_eq!(sim.pixel(20, 20), fill);

    // Moved: the old spot is background again, the new spot is filled.
    s.apply(&[SceneOp::Update(plain_rect(1, 200, 100, 50, 50))]).unwrap();
    assert_eq!(sim.pixel(20, 20), (0, 0, 0), "old position must be cleared");
    assert_eq!(sim.pixel(210, 110), fill);

    // Shrunk: the part no longer covered is background.
    s.apply(&[SceneOp::Update(plain_rect(1, 200, 100, 10, 10))]).unwrap();
    assert_eq!(sim.pixel(240, 140), (0, 0, 0), "shrunk area must be cleared");
    assert_eq!(sim.pixel(205, 105), fill);

    // Removed: nothing of it remains.
    s.apply(&[SceneOp::Remove(NodeId::from_raw(1))]).unwrap();
    assert_eq!(sim.pixel(205, 105), (0, 0, 0));
}
