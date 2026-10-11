//! Shared builders for the integration tests.
#![allow(dead_code)]

use weftos_display_client::scene::{
    px, BitmapFormat, CursorHint, HitShape, InputRegion, Layer, Node, NodeId, Primitive, Rgba,
    SceneOp, Style, Transform,
};
use weftos_display_client::{DisplayGrant, DisplaySession, DisplayTarget, LeafSim};

/// A filled rectangle at whole-pixel `(x, y)` with an AABB input region.
pub fn button(id: u32, x: i32, y: i32, w: i32, h: i32) -> Node {
    let mut n = plain_rect(id, x, y, w, h);
    n.input = Some(InputRegion {
        shape: HitShape::Aabb { w: px(w), h: px(h) },
        cursor_hint: CursorHint::Pointer,
        capture: true,
    });
    n
}

/// A filled rectangle with no input region.
pub fn plain_rect(id: u32, x: i32, y: i32, w: i32, h: i32) -> Node {
    let mut n = Node::new(
        NodeId::from_raw(id),
        Layer::Widget,
        Primitive::Rect {
            w: px(w),
            h: px(h),
            radius_q8: 0,
        },
    );
    n.transform = Transform::translate(px(x), px(y));
    n.style = Style::filled(Rgba::opaque(0x40, 0x80, 0xC0));
    n
}

/// A 1-pixel-wide Raw565 bitmap node holding exactly `bytes` bytes of
/// pixel data (`bytes` must be even so the decoder accepts it).
pub fn bitmap(id: u32, bytes: usize) -> Node {
    assert_eq!(bytes % 2, 0, "Raw565 needs two bytes per pixel");
    Node::new(
        NodeId::from_raw(id),
        Layer::Widget,
        Primitive::Bitmap {
            w: px(1),
            h: px((bytes / 2) as i32),
            format: BitmapFormat::Raw565,
            data: vec![0u8; bytes],
        },
    )
}

pub fn insert(n: Node) -> SceneOp {
    SceneOp::Insert(n)
}

pub fn open_sim() -> (DisplaySession, LeafSim) {
    let (target, sim) = DisplayTarget::sim();
    let s = DisplaySession::open(target, DisplayGrant::dev("test")).expect("open sim session");
    (s, sim)
}

pub fn drain(s: &mut DisplaySession) -> Vec<weftos_display_client::ObjectEvent> {
    let mut out = Vec::new();
    while let Some(ev) = s.poll_event() {
        out.push(ev);
    }
    out
}
