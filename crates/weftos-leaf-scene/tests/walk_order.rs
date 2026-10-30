//! Layer ordering of `walk_draw_order` and `walk_top_down`.

use weftos_leaf_scene::{
    geometry::{px, Transform},
    primitive::{Layer, Primitive, Style},
    Node, NodeId, Rgba, SceneOp, SceneStore,
};

fn id(n: u32) -> NodeId {
    NodeId::from_parts(0, n)
}

fn node(node_id: NodeId, layer: Layer) -> Node {
    Node {
        id: node_id,
        layer,
        transform: Transform::translate(px(0), px(0)),
        primitive: Primitive::Rect {
            w: px(10),
            h: px(10),
            radius_q8: 0,
        },
        style: Style::filled(Rgba::WHITE),
        input: None,
    }
}

fn store_with(nodes: &[(u32, Layer)]) -> SceneStore {
    let mut s = SceneStore::new();
    for (n, layer) in nodes {
        s.apply_op(0, &SceneOp::Insert(node(id(*n), *layer)));
    }
    s
}

fn draw_order_ids(s: &SceneStore, display: u8) -> Vec<NodeId> {
    let mut out = Vec::new();
    s.walk_draw_order(display, |n| out.push(n.id));
    out
}

fn top_down_ids(s: &SceneStore, display: u8) -> Vec<NodeId> {
    let mut out = Vec::new();
    s.walk_top_down(display, |n| out.push(n.id));
    out
}

#[test]
fn walk_draw_order_visits_bg_widget_text_alert() {
    let s = store_with(&[
        (1, Layer::Text),
        (2, Layer::Bg),
        (3, Layer::Alert),
        (4, Layer::Widget),
    ]);
    assert_eq!(draw_order_ids(&s, 0), vec![id(2), id(4), id(1), id(3)]);
}

#[test]
fn walk_top_down_visits_alert_text_widget_bg() {
    let s = store_with(&[
        (1, Layer::Text),
        (2, Layer::Bg),
        (3, Layer::Alert),
        (4, Layer::Widget),
    ]);
    assert_eq!(top_down_ids(&s, 0), vec![id(3), id(1), id(4), id(2)]);
}

#[test]
fn walk_order_keeps_insertion_order_within_a_layer() {
    let s = store_with(&[
        (1, Layer::Widget),
        (2, Layer::Bg),
        (3, Layer::Widget),
        (4, Layer::Bg),
    ]);
    assert_eq!(draw_order_ids(&s, 0), vec![id(2), id(4), id(1), id(3)]);
    assert_eq!(top_down_ids(&s, 0), vec![id(1), id(3), id(2), id(4)]);
}

#[test]
fn walk_order_is_noop_on_unknown_display() {
    let s = store_with(&[(1, Layer::Widget)]);
    assert!(draw_order_ids(&s, 7).is_empty());
    assert!(top_down_ids(&s, 7).is_empty());
}
