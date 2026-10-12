//! The leaf's retained scene: one `SceneStore` (Phase A,
//! `weftos-leaf-scene`) rendered through `weftos-leaf-renderer` onto the
//! `DpiDisplay` `SceneSurface`. Replaces the deprecated
//! `weftos-leaf-display::Compositor` (goal 6 of the 2026-10-10 bring-up).
//!
//! Two inputs reach it:
//! - vector `SceneEnvelope`s (`weaver leaf scene …`, the supported path),
//!   applied with `SceneStore::apply` and rendered by damage;
//! - the older raster `LeafPush` payloads (`weaver leaf push text|clear|
//!   image`), translated into scene nodes so the compositor's behaviour
//!   is kept: one node per `DisplayText`, `DisplayClear` removes every
//!   raster node in that layer, `DisplayImage` becomes a full-screen
//!   Raw565 bitmap. Brightness, layer effects and audio have no
//!   backend on this board and are ignored as before.
//!
//! Raster nodes live in a reserved id range (`0xF0_0000..`) so they never
//! collide with host-authored scene ids.

use log::{info, warn};
use weftos_leaf_renderer::render_damage;
use weftos_leaf_scene::{
    px, BitmapFormat, BuiltinFont, DamageSet, DisplayId, FontFace, KerningHint, Layer, Node, NodeId, Primitive,
    Rect, Rgba, SceneEnvelope, SceneOp, SceneStore, Style, Transform,
};
use weftos_leaf_types::{LayerSlot, LeafPush};

use crate::board::{SCREEN_HEIGHT, SCREEN_WIDTH};
use crate::display::DpiDisplay;

/// The one physical display.
pub const DISPLAY_ID: DisplayId = 0;
/// Full-panel viewport (Q24.8), used to clip merged damage.
const VIEWPORT: Rect = Rect::from_px(0, 0, SCREEN_WIDTH as i32, SCREEN_HEIGHT as i32);

fn merge(into: &mut DamageSet, other: DamageSet) {
    into.merge(&other, VIEWPORT);
}
/// Raster-shim node ids start here (24-bit path hash space).
const RASTER_ID_BASE: u32 = 0xF0_0000;
/// Boot-screen node ids.
pub(crate) const BOOT_TITLE_ID: u32 = 0xFE_0001;
pub(crate) const BOOT_LINE_ID: u32 = 0xFE_0002;

pub struct LeafScene {
    store: SceneStore,
    /// Raster-shim nodes currently in the store, with their layer.
    raster: Vec<(LayerSlot, NodeId)>,
    next_raster: u32,
    /// Damage kept back by a fragmented session batch until its last
    /// fragment arrives.
    pending: DamageSet,
}

fn layer_of(slot: LayerSlot) -> Layer {
    match slot {
        LayerSlot::Bg => Layer::Bg,
        LayerSlot::Widget => Layer::Widget,
        LayerSlot::Text => Layer::Text,
        LayerSlot::Alert => Layer::Alert,
    }
}

fn text_node(id: u32, layer: Layer, text: &str, x: i32, y: i32, color: Rgba) -> Node {
    Node {
        id: NodeId::from_parts(DISPLAY_ID, id),
        layer,
        // The compositor placed `Text::new` on its baseline; the renderer
        // uses Baseline::Top, so step up by the FONT_10X20 ascent to keep
        // the same pixel position.
        transform: Transform::translate(px(x), px(y - 16)),
        primitive: Primitive::Text {
            content: String::from(text),
            face: FontFace::Builtin(BuiltinFont::Mono10x20),
            size_q8: 20 << 8,
            weight: 400,
            kerning: KerningHint::Auto,
        },
        style: Style::filled(color),
        input: None,
    }
}

impl LeafScene {
    pub fn new() -> Self {
        let mut store = SceneStore::new();
        store.set_viewport(DISPLAY_ID, Rect::from_px(0, 0, SCREEN_WIDTH as i32, SCREEN_HEIGHT as i32));
        Self { store, raster: Vec::new(), next_raster: RASTER_ID_BASE, pending: DamageSet::none() }
    }

    /// Render the pending damage to the panel.
    fn render(&self, display: &mut DpiDisplay, damage: &DamageSet) {
        match render_damage(&self.store, DISPLAY_ID, damage, display) {
            Ok(stats) => info!(
                "[scene] rendered drawn={} full={} rects={}",
                stats.drawn,
                damage.is_full(),
                damage.rects().len()
            ),
            Err(e) => warn!("[scene] render failed: {e:?}"),
        }
    }

    /// Two-line status screen: white title, cyan `msg`. Same positions as
    /// the compositor-era boot screen (40,50) / (40,90).
    pub fn boot_screen(&mut self, display: &mut DpiDisplay, msg: &str) {
        let title = text_node(BOOT_TITLE_ID, Layer::Text, "clawft-edge-pad-idf :: mesh terminal", 40, 50, Rgba::WHITE);
        let line = text_node(BOOT_LINE_ID, Layer::Text, msg, 40, 90, Rgba::new(0, 255, 255, 255));
        let mut damage = DamageSet::full();
        merge(&mut damage, self.store.apply_op(DISPLAY_ID, &SceneOp::Update(title)));
        merge(&mut damage, self.store.apply_op(DISPLAY_ID, &SceneOp::Update(line)));
        self.render(display, &damage);
    }

    /// Apply a vector scene envelope from the host and render its damage.
    pub fn apply_envelope(&mut self, display: &mut DpiDisplay, env: &SceneEnvelope) {
        let ops = env.ops.len();
        let damage = self.store.apply(env);
        info!("[scene] APPLY display={} ops={ops}", env.display_id);
        self.render(display, &damage);
    }

    /// Apply a session's ops (display-session frames). With `render`
    /// false the damage is kept for the batch's last fragment, so a big
    /// scene draws once instead of once per fragment.
    pub fn apply_session_ops(&mut self, display: &mut DpiDisplay, env: &SceneEnvelope, render: bool) {
        let damage = self.store.apply(env);
        merge(&mut self.pending, damage);
        info!("[scene] SESSION ops={} render={render}", env.ops.len());
        if render && !self.pending.is_empty() {
            let damage = core::mem::replace(&mut self.pending, DamageSet::none());
            self.render(display, &damage);
        }
    }

    /// Translate a raster `LeafPush` into scene ops and render.
    pub fn apply_raster(&mut self, display: &mut DpiDisplay, push: LeafPush) {
        let mut damage = DamageSet::none();
        match push {
            LeafPush::DisplayText(t) => {
                if t.clear_first {
                    merge(&mut damage, self.clear_layer(t.z));
                }
                let id = self.alloc_raster(t.z);
                let node = text_node(id.0 & 0xFF_FFFF, layer_of(t.z), &t.text, t.x, t.y, Rgba::opaque(t.color[0], t.color[1], t.color[2]));
                merge(&mut damage, self.store.apply_op(DISPLAY_ID, &SceneOp::Insert(node)));
            }
            LeafPush::DisplayClear(c) => merge(&mut damage, self.clear_layer(c.z)),
            LeafPush::DisplayImage(img) => {
                let (w, h) = (SCREEN_WIDTH as usize, SCREEN_HEIGHT as usize);
                if img.rgb.len() != w * h * 3 {
                    warn!("[scene] DisplayImage has {} bytes, expected {} (full-screen RGB888) — dropped", img.rgb.len(), w * h * 3);
                    return;
                }
                let mut data = Vec::with_capacity(w * h * 2);
                for p in img.rgb.chunks_exact(3) {
                    let v: u16 = ((p[0] as u16 >> 3) << 11) | ((p[1] as u16 >> 2) << 5) | (p[2] as u16 >> 3);
                    data.extend_from_slice(&v.to_le_bytes());
                }
                merge(&mut damage, self.clear_layer(img.z));
                let id = self.alloc_raster(img.z);
                let node = Node {
                    id,
                    layer: layer_of(img.z),
                    transform: Transform::IDENTITY,
                    primitive: Primitive::Bitmap { w: px(w as i32), h: px(h as i32), format: BitmapFormat::Raw565, data },
                    style: Style::default(),
                    input: None,
                };
                merge(&mut damage, self.store.apply_op(DISPLAY_ID, &SceneOp::Insert(node)));
            }
            LeafPush::DisplayBrightness { on_us } => {
                info!("[scene] DisplayBrightness on_us={on_us} ignored (no PWM backlight on this board)");
                return;
            }
            LeafPush::LayerEffect(_) | LeafPush::Audio(_) => {
                info!("[scene] LayerEffect/Audio push ignored (no backend)");
                return;
            }
            _ => {
                warn!("[scene] unknown LeafPush variant ignored");
                return;
            }
        }
        self.render(display, &damage);
    }

    fn alloc_raster(&mut self, slot: LayerSlot) -> NodeId {
        let id = NodeId::from_parts(DISPLAY_ID, self.next_raster & 0xFF_FFFF);
        self.next_raster = RASTER_ID_BASE + ((self.next_raster + 1 - RASTER_ID_BASE) % 0x0D_FFFF);
        self.raster.push((slot, id));
        id
    }

    fn clear_layer(&mut self, slot: LayerSlot) -> DamageSet {
        let mut damage = DamageSet::none();
        let (gone, keep): (Vec<_>, Vec<_>) = self.raster.drain(..).partition(|(s, _)| *s == slot);
        self.raster = keep;
        for (_, id) in gone {
            merge(&mut damage, self.store.apply_op(DISPLAY_ID, &SceneOp::Remove(id)));
        }
        damage
    }

    /// Hit-test a touch against the retained scene (Q24.8 coords); the
    /// resolved node rides in the session's `Input` frame.
    pub fn hit_test(&self, x_q8: i32, y_q8: i32) -> Option<NodeId> {
        self.store.hit_test(DISPLAY_ID, x_q8, y_q8)
    }
}

impl Default for LeafScene {
    fn default() -> Self {
        Self::new()
    }
}

impl LeafScene {
    /// Partial-update micro-bench for the boot self-test: rewrite the
    /// boot line text as fast as the renderer presents it for `secs`.
    /// Each update damages only the old and new text rects, so this
    /// measures the partial-damage path (front→back copy + small clear
    /// + one text node), not a full repaint.
    pub fn bench_partial_updates(&mut self, display: &mut DpiDisplay, secs: u64) {
        use std::time::{Duration, Instant};
        let start = Instant::now();
        let mut n = 0u32;
        let _ = crate::display::take_flip_stats();
        while start.elapsed() < Duration::from_secs(secs) {
            let line = text_node(BOOT_LINE_ID, Layer::Text, &format!("partial update bench {n:05}"), 40, 90, Rgba::new(0, 255, 255, 255));
            let damage = self.store.apply_op(DISPLAY_ID, &SceneOp::Update(line));
            if let Err(e) = render_damage(&self.store, DISPLAY_ID, &damage, display) {
                warn!("[scene] bench render failed: {e:?}");
                break;
            }
            n += 1;
        }
        let (flips, wait_us) = crate::display::take_flip_stats();
        info!(
            "[selftest] partial updates: {n} in {secs} s ({:.1} fps); {flips} flips, mean flip wait {:.1} ms",
            n as f32 / secs as f32,
            wait_us as f32 / 1000.0
        );
    }
}
