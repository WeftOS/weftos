//! An envelope (and a `Batch`) applies every op, even after its damage
//! has already escalated to a full repaint. Found live: a console batch
//! that cleared and redrew the screen lost every op after the first
//! few, so removes were ignored and half the inserts never landed.

use weftos_leaf_scene::{
    px, Layer, Node, NodeId, Primitive, Rect, Rgba, SceneEnvelope, SceneOp, SceneStore, Style,
    Transform,
};

fn big_rect(n: u32) -> Node {
    Node {
        id: NodeId::from_parts(0, n),
        layer: Layer::Widget,
        transform: Transform::translate(px(0), px(0)),
        primitive: Primitive::Rect {
            w: px(800),
            h: px(480),
            radius_q8: 0,
        },
        style: Style::filled(Rgba::RED),
        input: None,
    }
}

fn store() -> SceneStore {
    let mut s = SceneStore::new();
    s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
    s
}

#[test]
fn envelope_applies_every_op_after_damage_goes_full() {
    let mut s = store();
    // Two full-screen rects push the damage past the 50% threshold;
    // the hundred small nodes after them must still land.
    let mut ops = vec![SceneOp::Insert(big_rect(1)), SceneOp::Insert(big_rect(2))];
    for n in 10..110 {
        let mut small = big_rect(n);
        small.primitive = Primitive::Rect {
            w: px(4),
            h: px(4),
            radius_q8: 0,
        };
        ops.push(SceneOp::Insert(small));
    }
    ops.push(SceneOp::Remove(NodeId::from_parts(0, 1)));
    let damage = s.apply(&SceneEnvelope::new(0, ops));
    assert!(damage.is_full());
    assert!(s.node(NodeId::from_parts(0, 1)).is_none(), "the trailing remove applied");
    assert!(s.node(NodeId::from_parts(0, 109)).is_some(), "the trailing insert applied");
    assert_eq!(s.display(0).unwrap().nodes.len(), 101);
}

#[test]
fn batch_applies_every_op_after_damage_goes_full() {
    let mut s = store();
    let ops = vec![
        SceneOp::Insert(big_rect(1)),
        SceneOp::Insert(big_rect(2)),
        SceneOp::Insert(big_rect(3)),
        SceneOp::Remove(NodeId::from_parts(0, 2)),
    ];
    let damage = s.apply_op(0, &SceneOp::Batch(ops));
    assert!(damage.is_full());
    assert!(s.node(NodeId::from_parts(0, 3)).is_some());
    assert!(s.node(NodeId::from_parts(0, 2)).is_none());
}
