//! Raw pointer events → object events.
//!
//! The leaf resolves every pointer event to a node id with its own
//! hit-test and ships `InputEnvelope`s. This mapper turns those into
//! the two gestures D1 promises: a `Tap` when the pointer goes down and
//! up on a node without travelling, and incremental `Drag` deltas while
//! it is held and moving. The node is captured at pointer-down, so a
//! drag that wanders off the object keeps reporting against it.

use std::collections::VecDeque;

use weftos_leaf_scene::{from_px_q8, InputEnvelope, InputEvent, NodeId};

use crate::event::ObjectEvent;

/// Pixels the pointer must travel before a press becomes a drag.
pub const DRAG_THRESHOLD_PX: i32 = 4;

#[derive(Debug, Clone, Copy)]
struct Active {
    pointer: u8,
    node: Option<NodeId>,
    start: (i32, i32),
    last: (i32, i32),
    dragging: bool,
}

/// Tracks one pointer at a time (the first one down); other slots are
/// ignored until it lifts.
#[derive(Debug, Default)]
pub struct GestureMapper {
    active: Option<Active>,
}

impl GestureMapper {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one raw event; any resulting object events are appended to
    /// `out` in order.
    pub fn feed(&mut self, env: &InputEnvelope, out: &mut VecDeque<ObjectEvent>) {
        match env.event {
            InputEvent::PointerDown { pointer_id, x, y, .. } => {
                if self.active.is_some() {
                    return;
                }
                let at = (from_px_q8(x), from_px_q8(y));
                self.active = Some(Active {
                    pointer: pointer_id,
                    node: env.node_id,
                    start: at,
                    last: at,
                    dragging: false,
                });
            }
            InputEvent::PointerMove { pointer_id, x, y, .. } => {
                let Some(a) = self.active.as_mut() else { return };
                if a.pointer != pointer_id {
                    return;
                }
                let now = (from_px_q8(x), from_px_q8(y));
                if !a.dragging {
                    let travelled = (now.0 - a.start.0).abs().max((now.1 - a.start.1).abs());
                    if travelled < DRAG_THRESHOLD_PX {
                        return;
                    }
                    a.dragging = true;
                }
                let (dx, dy) = (now.0 - a.last.0, now.1 - a.last.1);
                a.last = now;
                if let (Some(node), true) = (a.node, dx != 0 || dy != 0) {
                    out.push_back(ObjectEvent::Drag { node, dx, dy });
                }
            }
            InputEvent::PointerUp { pointer_id, .. } => {
                let Some(a) = self.active else { return };
                if a.pointer != pointer_id {
                    return;
                }
                self.active = None;
                if let (Some(node), false) = (a.node, a.dragging) {
                    out.push_back(ObjectEvent::Tap { node });
                }
            }
            InputEvent::PointerCancel { pointer_id } => {
                if matches!(self.active, Some(a) if a.pointer == pointer_id) {
                    self.active = None;
                }
            }
        }
    }

    /// True while a pointer is held.
    pub fn is_pressed(&self) -> bool {
        self.active.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_leaf_scene::px;

    fn env(node: Option<u32>, ev: InputEvent) -> InputEnvelope {
        InputEnvelope::new(0, node.map(NodeId::from_raw), ev)
    }
    fn down(node: Option<u32>, x: i32, y: i32) -> InputEnvelope {
        env(
            node,
            InputEvent::PointerDown {
                pointer_id: 0,
                x: px(x),
                y: px(y),
                pressure_q8: 0,
            },
        )
    }
    fn mv(x: i32, y: i32) -> InputEnvelope {
        env(
            None,
            InputEvent::PointerMove {
                pointer_id: 0,
                x: px(x),
                y: px(y),
                pressure_q8: 0,
            },
        )
    }
    fn up(x: i32, y: i32) -> InputEnvelope {
        env(
            None,
            InputEvent::PointerUp {
                pointer_id: 0,
                x: px(x),
                y: px(y),
            },
        )
    }

    #[test]
    fn down_up_on_node_is_tap() {
        let mut m = GestureMapper::new();
        let mut out = VecDeque::new();
        m.feed(&down(Some(7), 10, 10), &mut out);
        m.feed(&up(11, 10), &mut out);
        assert_eq!(out.pop_front(), Some(ObjectEvent::Tap { node: NodeId::from_raw(7) }));
        assert!(out.is_empty());
    }

    #[test]
    fn jitter_under_threshold_stays_a_tap() {
        let mut m = GestureMapper::new();
        let mut out = VecDeque::new();
        m.feed(&down(Some(7), 10, 10), &mut out);
        m.feed(&mv(12, 11), &mut out);
        m.feed(&up(12, 11), &mut out);
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], ObjectEvent::Tap { .. }));
    }

    #[test]
    fn travel_becomes_drag_and_no_tap() {
        let mut m = GestureMapper::new();
        let mut out = VecDeque::new();
        m.feed(&down(Some(7), 10, 10), &mut out);
        m.feed(&mv(20, 10), &mut out);
        m.feed(&mv(25, 13), &mut out);
        m.feed(&up(25, 13), &mut out);
        let node = NodeId::from_raw(7);
        assert_eq!(
            out.into_iter().collect::<Vec<_>>(),
            vec![
                ObjectEvent::Drag { node, dx: 10, dy: 0 },
                ObjectEvent::Drag { node, dx: 5, dy: 3 },
            ]
        );
    }

    #[test]
    fn press_on_nothing_is_silent() {
        let mut m = GestureMapper::new();
        let mut out = VecDeque::new();
        m.feed(&down(None, 10, 10), &mut out);
        m.feed(&mv(40, 40), &mut out);
        m.feed(&up(40, 40), &mut out);
        assert!(out.is_empty());
    }
}
