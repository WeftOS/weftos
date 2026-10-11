//! Leaf-side scene store — see
//! [vector-leaf-display.md §5 Scene Graph Model](../../../docs/design/vector-leaf-display.md).
//!
//! `SceneStore` is the leaf's authoritative state for one or more
//! displays. Hosts apply [`SceneOp`](crate::op::SceneOp)s; the store
//! returns a [`DamageSet`](crate::damage::DamageSet) so the renderer
//! knows which rects to repaint.
//!
//! ## Threading
//!
//! `SceneStore` is `Send + !Sync` by virtue of `BTreeMap` interior;
//! wrap in a `Mutex` for shared access between the mesh ingest task
//! and the renderer. See design doc Appendix B.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use crate::color::Rgba;
use crate::damage::DamageSet;
use crate::envelope::SceneEnvelope;
use crate::geometry::Rect;
use crate::id::{DisplayId, NodeId};
use crate::node::Node;
use crate::op::SceneOp;
use crate::primitive::{BlendMode, HitShape, Layer, Primitive};
use crate::scene::Scene;
use crate::tween::{ActiveTween, AnimatableProperty, PropertyValue, TweenTable};

/// Per-display state. Internal to `SceneStore` but `pub` so consumers
/// can introspect via [`SceneStore::display`].
#[derive(Debug, Clone)]
pub struct DisplayState {
    pub bg: Rgba,
    pub viewport: Rect,
    pub layer_blend: [BlendMode; 4],
    /// Nodes keyed by id. Iteration order is `BTreeMap`'s id order; we
    /// keep `node_order` to preserve z-order within a layer.
    pub nodes: BTreeMap<NodeId, Node>,
    /// Z-order witness: the order nodes were inserted in. Producers
    /// resend in declaration order; the renderer walks this vector.
    pub node_order: Vec<NodeId>,
    pub tweens: TweenTable,
}

impl DisplayState {
    fn new() -> Self {
        Self {
            bg: Rgba::TRANSPARENT,
            viewport: Rect::ZERO,
            layer_blend: [BlendMode::Normal; 4],
            nodes: BTreeMap::new(),
            node_order: Vec::new(),
            tweens: TweenTable::new(),
        }
    }

    /// AABB of every node in a given layer. Used by
    /// `SceneOp::SetLayerBlend` damage. Returns `Rect::ZERO` for empty layers.
    fn layer_aabb(&self, layer: Layer) -> Rect {
        let mut acc = Rect::ZERO;
        for id in &self.node_order {
            if let Some(n) = self.nodes.get(id) {
                if n.layer == layer {
                    if let Some(a) = n.aabb() {
                        acc = acc.union(&a);
                    }
                }
            }
        }
        acc
    }
}

/// The leaf-side authoritative scene state.
///
/// One store per leaf instance. Each display is keyed by `DisplayId`;
/// the store lazy-creates a [`DisplayState`] when the first op targets
/// a new id.
#[derive(Debug, Clone, Default)]
pub struct SceneStore {
    displays: BTreeMap<DisplayId, DisplayState>,
}

impl SceneStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Read-only access to one display's state, if it exists.
    pub fn display(&self, id: DisplayId) -> Option<&DisplayState> {
        self.displays.get(&id)
    }

    /// True if this store has any state for `id`.
    pub fn has_display(&self, id: DisplayId) -> bool {
        self.displays.contains_key(&id)
    }

    /// Convenience: lookup a single node by id (across all displays —
    /// the NodeId's high byte disambiguates).
    pub fn node(&self, id: NodeId) -> Option<&Node> {
        self.displays
            .get(&id.display_id())
            .and_then(|d| d.nodes.get(&id))
    }

    /// Apply an envelope's worth of ops; returns the union of their damage.
    ///
    /// All ops in an envelope target the envelope's `display_id`; ops
    /// inside the envelope MAY override (via `Replace(Scene)` which
    /// carries its own display id), in which case the inner scene's id
    /// wins for that specific op.
    pub fn apply(&mut self, env: &SceneEnvelope) -> DamageSet {
        let display = env.display_id;
        let viewport = self
            .displays
            .get(&display)
            .map(|d| d.viewport)
            .unwrap_or(Rect::ZERO);
        let mut damage = DamageSet::none();
        for op in &env.ops {
            // Every op is applied even once the damage is already a
            // full repaint: an envelope is atomic, and a batch whose
            // early ops dirty half the screen must not lose its tail.
            let d = self.apply_op(display, op);
            if damage.is_full() {
                continue;
            }
            let vp = self
                .displays
                .get(&display)
                .map(|d| d.viewport)
                .unwrap_or(viewport);
            damage.merge(&d, vp);
        }
        damage
    }

    /// Apply a single op to one display. Public so renderer tests can
    /// drive the store without round-tripping an envelope.
    pub fn apply_op(&mut self, display: DisplayId, op: &SceneOp) -> DamageSet {
        match op {
            SceneOp::Insert(node) | SceneOp::Update(node) => self.upsert(display, node.clone()),
            SceneOp::Remove(id) => self.remove(*id),
            SceneOp::SetLayerBlend { layer, mode } => self.set_layer_blend(display, *layer, *mode),
            SceneOp::Tween {
                id,
                property,
                from,
                to,
                duration_ms,
                start_at,
                curve,
            } => self.start_tween(
                *id,
                *property,
                from.clone(),
                to.clone(),
                *duration_ms,
                *start_at,
                *curve,
            ),
            SceneOp::CancelTween { id, property } => {
                self.cancel_tween(*id, *property);
                DamageSet::none()
            }
            SceneOp::Clear => self.clear(display),
            SceneOp::Replace(scene) => self.replace(scene.clone()),
            SceneOp::Batch(ops) => {
                let vp = self
                    .displays
                    .get(&display)
                    .map(|d| d.viewport)
                    .unwrap_or(Rect::ZERO);
                let mut acc = DamageSet::none();
                for inner in ops {
                    let d = self.apply_op(display, inner);
                    if !acc.is_full() {
                        acc.merge(&d, vp);
                    }
                }
                acc
            }
        }
    }

    fn ensure_display(&mut self, id: DisplayId) -> &mut DisplayState {
        self.displays.entry(id).or_insert_with(DisplayState::new)
    }

    fn upsert(&mut self, display: DisplayId, mut node: Node) -> DamageSet {
        // Normalize: the node's DisplayId byte MUST match the
        // envelope's display id. If a misaligned id is upserted, we
        // rewrite it so the store stays internally consistent.
        if node.id.display_id() != display {
            node.id = NodeId::from_parts(display, node.id.path_hash());
        }
        let new_aabb = node.aabb();
        let d = self.ensure_display(display);
        let viewport = d.viewport;
        let old_aabb = d.nodes.get(&node.id).and_then(|n| n.aabb());
        let was_present = d.nodes.contains_key(&node.id);
        d.nodes.insert(node.id, node.clone());
        if !was_present {
            d.node_order.push(node.id);
        }
        let mut damage = DamageSet::none();
        if let Some(r) = old_aabb {
            damage.add(r, viewport);
        }
        if let Some(r) = new_aabb {
            damage.add(r, viewport);
        }
        damage
    }

    fn remove(&mut self, id: NodeId) -> DamageSet {
        let display = id.display_id();
        let Some(d) = self.displays.get_mut(&display) else {
            return DamageSet::none();
        };
        let viewport = d.viewport;
        let aabb = d.nodes.get(&id).and_then(|n| n.aabb());
        if d.nodes.remove(&id).is_some() {
            d.node_order.retain(|other| *other != id);
            // Also drop any tweens for this node — cleanup, no damage
            // beyond the AABB.
            d.tweens.cancel(id, None);
        }
        if let Some(r) = aabb {
            DamageSet::from_rect(clip(r, viewport))
        } else {
            DamageSet::none()
        }
    }

    fn set_layer_blend(&mut self, display: DisplayId, layer: Layer, mode: BlendMode) -> DamageSet {
        let d = self.ensure_display(display);
        d.layer_blend[layer.index()] = mode;
        let viewport = d.viewport;
        let aabb = d.layer_aabb(layer);
        DamageSet::from_rect(clip(aabb, viewport))
    }

    fn clear(&mut self, display: DisplayId) -> DamageSet {
        let Some(d) = self.displays.get_mut(&display) else {
            return DamageSet::none();
        };
        d.nodes.clear();
        d.node_order.clear();
        d.tweens = TweenTable::new();
        // Conservative: full repaint, since everything just vanished.
        DamageSet::full()
    }

    fn replace(&mut self, scene: Scene) -> DamageSet {
        let display = scene.display_id;
        let d = self.ensure_display(display);
        d.bg = scene.bg;
        d.viewport = scene.viewport;
        d.layer_blend = scene.layer_blend;
        d.nodes.clear();
        d.node_order.clear();
        // Preserve scene's declaration order in `node_order`.
        for node in scene.nodes {
            d.node_order.push(node.id);
            d.nodes.insert(node.id, node);
        }
        // Tweens are reset on Replace; the producer can re-emit
        // mid-flight tweens after the snapshot if needed.
        d.tweens = TweenTable::new();
        DamageSet::full()
    }

    #[allow(clippy::too_many_arguments)] // internal helper; arg list mirrors SceneOp::Tween's wire fields verbatim
    fn start_tween(
        &mut self,
        id: NodeId,
        property: AnimatableProperty,
        from: PropertyValue,
        to: PropertyValue,
        duration_ms: u32,
        start_at: Option<u32>,
        curve: crate::primitive::EaseCurve,
    ) -> DamageSet {
        let display = id.display_id();
        let d = self.ensure_display(display);
        let viewport = d.viewport;
        let aabb_before = d.nodes.get(&id).and_then(|n| n.aabb());
        let start_ms = start_at.unwrap_or(0);
        let tween = ActiveTween {
            id,
            property,
            from,
            to,
            start_ms,
            duration_ms,
            curve,
        };
        d.tweens.insert(tween);
        // Damage covers the node's pre-animation AABB; the renderer
        // ticks before drawing, so any post-animation expansion shows
        // up on the next frame via `tick`.
        if let Some(r) = aabb_before {
            DamageSet::from_rect(clip(r, viewport))
        } else {
            DamageSet::none()
        }
    }

    fn cancel_tween(&mut self, id: NodeId, property: Option<AnimatableProperty>) {
        let display = id.display_id();
        if let Some(d) = self.displays.get_mut(&display) {
            d.tweens.cancel(id, property);
        }
    }

    /// Advance the per-display tween tables. v1 behaviour: every
    /// active tween snaps to its `to` value, the affected node's
    /// property is overwritten, and the tween is removed.
    ///
    /// v1.1 will replace this with eased interpolation per frame.
    /// Producers don't need to change anything; the damage rules are
    /// already correct.
    pub fn tick(&mut self, now_ms: u32) -> DamageSet {
        let mut accumulated = DamageSet::none();
        // Collect display ids first to satisfy borrow checker.
        let ids: Vec<DisplayId> = self.displays.keys().copied().collect();
        for d_id in ids {
            let viewport = self
                .displays
                .get(&d_id)
                .map(|d| d.viewport)
                .unwrap_or(Rect::ZERO);
            let drained = {
                let d = self.displays.get_mut(&d_id).expect("present above");
                d.tweens.tick_v1_snap(now_ms)
            };
            for tw in drained {
                let local = self.snap_tween_to_end(tw);
                accumulated.merge(&local, viewport);
                if accumulated.is_full() {
                    break;
                }
            }
            if accumulated.is_full() {
                break;
            }
        }
        accumulated
    }

    /// v1: apply the `to` value to the affected node, return the
    /// union of pre- and post-snap AABBs as damage.
    ///
    /// v1.1: interpolate to the current frame's value and emit damage
    /// covering both the old and new AABBs. The signature stays the
    /// same; only the body changes.
    fn snap_tween_to_end(&mut self, tw: ActiveTween) -> DamageSet {
        let display = tw.id.display_id();
        let Some(d) = self.displays.get_mut(&display) else {
            return DamageSet::none();
        };
        let viewport = d.viewport;
        let Some(node) = d.nodes.get_mut(&tw.id) else {
            return DamageSet::none();
        };
        let old_aabb = node.aabb();
        apply_property(node, &tw.property, &tw.to);
        let new_aabb = node.aabb();
        let mut dmg = DamageSet::none();
        if let Some(r) = old_aabb {
            dmg.add(r, viewport);
        }
        if let Some(r) = new_aabb {
            dmg.add(r, viewport);
        }
        dmg
    }

    /// Set this display's viewport. Producers should call once at
    /// startup; the v1 protocol doesn't require this to be on the wire.
    pub fn set_viewport(&mut self, display: DisplayId, viewport: Rect) {
        self.ensure_display(display).viewport = viewport;
    }

    /// Set the display background colour. Producers usually set this
    /// via `Replace(Scene)`; this is the imperative escape hatch.
    pub fn set_bg(&mut self, display: DisplayId, bg: Rgba) -> DamageSet {
        let d = self.ensure_display(display);
        d.bg = bg;
        DamageSet::full()
    }

    /// Materialize one display's current state into a [`Scene`]. The
    /// resulting Scene, applied via `SceneOp::Replace`, reconstructs
    /// this display verbatim (modulo in-flight tweens, which Replace
    /// always resets — see `replace`).
    ///
    /// This is the snapshot-cadence hook: producers call this every 5
    /// seconds and on mesh-connect to send the leaf an authoritative
    /// view of state for self-healing.
    pub fn to_snapshot(&self, display: DisplayId) -> Scene {
        let Some(d) = self.displays.get(&display) else {
            return Scene::empty(display);
        };
        let mut nodes = Vec::with_capacity(d.node_order.len());
        for id in &d.node_order {
            if let Some(n) = d.nodes.get(id) {
                nodes.push(n.clone());
            }
        }
        Scene {
            display_id: display,
            bg: d.bg,
            viewport: d.viewport,
            layer_blend: d.layer_blend,
            nodes,
        }
    }

    /// Walk every node in `display` in z-order, top layer first
    /// (`Alert → Text → Widget → Bg`). Useful for renderers that draw
    /// top-down; most v1 renderers reverse-iterate (bottom-up).
    pub fn walk_top_down(&self, display: DisplayId, mut f: impl FnMut(&Node)) {
        let Some(d) = self.displays.get(&display) else {
            return;
        };
        for layer in Layer::TOP_DOWN {
            for id in &d.node_order {
                if let Some(n) = d.nodes.get(id) {
                    if n.layer == layer {
                        f(n);
                    }
                }
            }
        }
    }

    /// Walk every node in draw order (`Bg → Widget → Text → Alert`).
    /// Renderers walk this on each frame.
    pub fn walk_draw_order(&self, display: DisplayId, mut f: impl FnMut(&Node)) {
        let Some(d) = self.displays.get(&display) else {
            return;
        };
        for layer in Layer::DRAW_ORDER {
            for id in &d.node_order {
                if let Some(n) = d.nodes.get(id) {
                    if n.layer == layer {
                        f(n);
                    }
                }
            }
        }
    }

    /// Hit-test: walk top-down, return the first interactive node
    /// whose [`InputRegion`](crate::primitive::InputRegion) contains
    /// `(x_q8, y_q8)`. Coordinates are Q24.8 display pixels.
    ///
    /// `HitShape::Aabb` and `HitShape::Circle` ship in v1;
    /// `HitShape::Path` returns "no hit" (the renderer's responsibility
    /// to rasterize-and-test arrives in v1.1).
    pub fn hit_test(&self, display: DisplayId, x_q8: i32, y_q8: i32) -> Option<NodeId> {
        let d = self.displays.get(&display)?;
        for layer in Layer::TOP_DOWN {
            // Iterate node_order in REVERSE so latest-declared sibling
            // (top of its layer's z-stack) wins.
            for id in d.node_order.iter().rev() {
                let Some(node) = d.nodes.get(id) else {
                    continue;
                };
                if node.layer != layer {
                    continue;
                }
                let Some(region) = &node.input else { continue };
                if node.style.is_invisible() {
                    continue;
                }
                if hit_test_shape(node, &region.shape, x_q8, y_q8) {
                    return Some(*id);
                }
            }
        }
        None
    }
}

/// Hit-test one shape, accounting for the node's transform.
fn hit_test_shape(node: &Node, shape: &HitShape, x_q8: i32, y_q8: i32) -> bool {
    let local_x = x_q8 - node.transform.x;
    let local_y = y_q8 - node.transform.y;
    match shape {
        HitShape::Aabb { w, h } => local_x >= 0 && local_x < *w && local_y >= 0 && local_y < *h,
        HitShape::Circle { radius_q16 } => {
            // local is Q24.8; radius is Q16.16. Compare squared
            // distances in Q16.16-ish space.
            let r_q8 = (*radius_q16 >> 8) as i64;
            let dx = local_x as i64;
            let dy = local_y as i64;
            dx * dx + dy * dy <= r_q8 * r_q8
        }
        // v1.1: rasterize the path and test.
        HitShape::Path(_) => false,
    }
}

/// Apply a property snap to a node. Mirrors `AnimatableProperty`
/// semantics: v1 just overwrites the relevant field. v1.1 interpolates.
fn apply_property(node: &mut Node, property: &AnimatableProperty, value: &PropertyValue) {
    match (property, value) {
        (AnimatableProperty::Position, PropertyValue::Position(p)) => {
            node.transform.x = p.x;
            node.transform.y = p.y;
        }
        (AnimatableProperty::Opacity, PropertyValue::Opacity(v)) => {
            node.style.opacity = *v;
        }
        (AnimatableProperty::Fill, PropertyValue::Color(c)) => {
            // Only overwrite if a fill exists; otherwise materialize one.
            node.style.fill = Some(*c);
        }
        (AnimatableProperty::Stroke, PropertyValue::Color(c)) => {
            node.style.stroke = Some(*c);
        }
        (AnimatableProperty::Scale, PropertyValue::ScaleQ16(s)) => {
            node.transform.scale_q16 = *s;
        }
        (AnimatableProperty::Rotation, PropertyValue::RotationQ8(r)) => {
            node.transform.rotation_deg_q8 = *r;
        }
        (AnimatableProperty::TextContent, PropertyValue::Text(t)) => {
            if let Primitive::Text { content, .. } = &mut node.primitive {
                *content = t.clone();
            }
        }
        // Type mismatch — silently drop. v1.1 may want to log here.
        _ => {}
    }
}

/// Clip a rect to a viewport. Empty viewport disables clipping (keeps
/// the rect intact); this lets producers that haven't called
/// `set_viewport` still get useful damage.
fn clip(rect: Rect, viewport: Rect) -> Rect {
    if viewport.is_empty() {
        rect
    } else {
        rect.clip_to(&viewport)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::geometry::{px, Transform};
    use crate::primitive::{Primitive, Style};

    fn node_id(d: u8, n: u32) -> NodeId {
        NodeId::from_parts(d, n)
    }

    fn rect_node(id: NodeId, x: i32, y: i32, w: i32, h: i32) -> Node {
        Node {
            id,
            layer: Layer::Widget,
            transform: Transform::translate(px(x), px(y)),
            primitive: Primitive::Rect {
                w: px(w),
                h: px(h),
                radius_q8: 0,
            },
            style: Style::filled(Rgba::RED),
            input: None,
        }
    }

    #[test]
    fn upsert_emits_damage_for_new_node() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let d = s.apply_op(
            0,
            &SceneOp::Insert(rect_node(node_id(0, 1), 10, 10, 50, 50)),
        );
        assert!(!d.is_full());
        assert_eq!(d.len(), 1);
        assert_eq!(d.rects()[0], Rect::from_px(10, 10, 50, 50));
    }

    #[test]
    fn upsert_existing_unions_old_and_new_aabbs() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        s.apply_op(
            0,
            &SceneOp::Insert(rect_node(node_id(0, 1), 10, 10, 50, 50)),
        );
        // Move the node — damage should cover both old and new positions.
        let d = s.apply_op(
            0,
            &SceneOp::Update(rect_node(node_id(0, 1), 100, 10, 50, 50)),
        );
        assert!(!d.is_full());
        // Two disjoint rects (assuming they don't touch).
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn remove_emits_aabb_damage() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        s.apply_op(
            0,
            &SceneOp::Insert(rect_node(node_id(0, 7), 100, 100, 20, 20)),
        );
        let d = s.apply_op(0, &SceneOp::Remove(node_id(0, 7)));
        assert_eq!(d.rects()[0], Rect::from_px(100, 100, 20, 20));
        assert!(s.node(node_id(0, 7)).is_none());
    }

    #[test]
    fn clear_returns_full_damage() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        s.apply_op(
            0,
            &SceneOp::Insert(rect_node(node_id(0, 1), 10, 10, 10, 10)),
        );
        let d = s.apply_op(0, &SceneOp::Clear);
        assert!(d.is_full());
        assert_eq!(s.display(0).unwrap().nodes.len(), 0);
    }

    #[test]
    fn replace_resets_and_returns_full_damage() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        s.apply_op(
            0,
            &SceneOp::Insert(rect_node(node_id(0, 1), 10, 10, 10, 10)),
        );
        let mut scene = Scene::empty(0);
        scene.viewport = Rect::from_px(0, 0, 800, 480);
        scene.nodes.push(rect_node(node_id(0, 99), 0, 0, 5, 5));
        let d = s.apply_op(0, &SceneOp::Replace(scene));
        assert!(d.is_full());
        assert_eq!(s.display(0).unwrap().nodes.len(), 1);
        assert!(s.node(node_id(0, 99)).is_some());
        assert!(s.node(node_id(0, 1)).is_none());
    }

    #[test]
    fn batch_unions_damage() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let batch = SceneOp::Batch(alloc::vec![
            SceneOp::Insert(rect_node(node_id(0, 1), 0, 0, 10, 10)),
            SceneOp::Insert(rect_node(node_id(0, 2), 100, 100, 10, 10)),
        ]);
        let d = s.apply_op(0, &batch);
        assert!(!d.is_full());
        assert_eq!(d.len(), 2);
    }

    #[test]
    fn set_layer_blend_damages_only_that_layer() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        // One Widget, one Text node.
        s.apply_op(0, &SceneOp::Insert(rect_node(node_id(0, 1), 0, 0, 10, 10)));
        let mut text = rect_node(node_id(0, 2), 100, 100, 10, 10);
        text.layer = Layer::Text;
        s.apply_op(0, &SceneOp::Insert(text));
        let d = s.apply_op(
            0,
            &SceneOp::SetLayerBlend {
                layer: Layer::Widget,
                mode: BlendMode::Multiply,
            },
        );
        // Damage covers the Widget node only.
        assert_eq!(d.rects()[0], Rect::from_px(0, 0, 10, 10));
    }

    #[test]
    fn tick_snaps_opacity_tween_to_end() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let mut n = rect_node(node_id(0, 1), 0, 0, 10, 10);
        n.style.opacity = 0;
        s.apply_op(0, &SceneOp::Insert(n));
        s.apply_op(
            0,
            &SceneOp::Tween {
                id: node_id(0, 1),
                property: AnimatableProperty::Opacity,
                from: PropertyValue::Opacity(0),
                to: PropertyValue::Opacity(255),
                duration_ms: 200,
                start_at: None,
                curve: crate::primitive::EaseCurve::Linear,
            },
        );
        let _ = s.tick(0);
        assert_eq!(s.node(node_id(0, 1)).unwrap().style.opacity, 255);
    }

    #[test]
    fn hit_test_aabb_picks_top_layer_first() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        // Two overlapping interactive nodes, one Widget, one Alert.
        let mut widget = rect_node(node_id(0, 1), 0, 0, 100, 100);
        widget.input = Some(crate::primitive::InputRegion {
            shape: HitShape::Aabb {
                w: px(100),
                h: px(100),
            },
            cursor_hint: crate::primitive::CursorHint::None,
            capture: false,
        });
        s.apply_op(0, &SceneOp::Insert(widget));

        let mut alert = rect_node(node_id(0, 2), 0, 0, 50, 50);
        alert.layer = Layer::Alert;
        alert.input = Some(crate::primitive::InputRegion {
            shape: HitShape::Aabb {
                w: px(50),
                h: px(50),
            },
            cursor_hint: crate::primitive::CursorHint::None,
            capture: false,
        });
        s.apply_op(0, &SceneOp::Insert(alert));

        // (10, 10) hits both — Alert wins.
        assert_eq!(s.hit_test(0, px(10), px(10)), Some(node_id(0, 2)));
        // (60, 60) hits only the Widget.
        assert_eq!(s.hit_test(0, px(60), px(60)), Some(node_id(0, 1)));
        // (200, 200) hits nothing.
        assert_eq!(s.hit_test(0, px(200), px(200)), None);
    }

    #[test]
    fn hit_test_circle() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        let mut n = rect_node(node_id(0, 1), 100, 100, 0, 0);
        n.input = Some(crate::primitive::InputRegion {
            // 50 px radius. Q16.16: 50 << 16.
            shape: HitShape::Circle {
                radius_q16: 50u32 << 16,
            },
            cursor_hint: crate::primitive::CursorHint::None,
            capture: false,
        });
        s.apply_op(0, &SceneOp::Insert(n));
        // Inside circle.
        assert_eq!(s.hit_test(0, px(120), px(120)), Some(node_id(0, 1)));
        // Outside circle.
        assert_eq!(s.hit_test(0, px(200), px(200)), None);
    }

    #[test]
    fn hit_test_skips_invisible() {
        let mut s = SceneStore::new();
        let mut n = rect_node(node_id(0, 1), 0, 0, 10, 10);
        n.input = Some(crate::primitive::InputRegion {
            shape: HitShape::Aabb {
                w: px(10),
                h: px(10),
            },
            cursor_hint: crate::primitive::CursorHint::None,
            capture: false,
        });
        n.style.opacity = 0;
        s.apply_op(0, &SceneOp::Insert(n));
        assert_eq!(s.hit_test(0, px(5), px(5)), None);
    }

    #[test]
    fn to_snapshot_roundtrips_via_replace() {
        let mut s = SceneStore::new();
        s.set_viewport(0, Rect::from_px(0, 0, 800, 480));
        s.set_bg(0, Rgba::opaque(0x10, 0x20, 0x30));
        s.apply_op(0, &SceneOp::Insert(rect_node(node_id(0, 1), 0, 0, 10, 10)));
        s.apply_op(
            0,
            &SceneOp::Insert(rect_node(node_id(0, 2), 50, 50, 20, 20)),
        );

        let snap = s.to_snapshot(0);

        // Apply to a fresh store via Replace.
        let mut t = SceneStore::new();
        let dmg = t.apply_op(0, &SceneOp::Replace(snap.clone()));
        assert!(dmg.is_full());

        // Both stores should see equivalent nodes (order + content).
        let t_snap = t.to_snapshot(0);
        assert_eq!(t_snap, snap);
    }
}
