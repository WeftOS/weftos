//! `ui://canvas` — freeform 2D painter primitive (ADR-013, ADR-001 row 19).
//!
//! Allocates a typed `egui::Rect` via `ui.allocate_painter(size, sense)`
//! and passes a `Painter` + `CanvasTransform` to the caller's draw
//! closure. Pan/zoom state is persisted via `memory.data` keyed on
//! the canvas id, so successive frames resume with the same viewport.

use std::borrow::Cow;

use egui;

use super::CanonWidget;
use super::response::CanonResponse;
use super::types::{Affordance, Confidence, IdentityUri, MutationAxis, Tooltip, VariantId};

const IDENTITY: &str = "ui://canvas";

static AFFORDANCES_ACTIVE: &[Affordance] = &[
    Affordance {
        name: Cow::Borrowed("paint"),
        verb: Cow::Borrowed("wsp.invoke"),
        actors: &[],
        args_schema: None,
        reorderable: false,
    },
    Affordance {
        name: Cow::Borrowed("erase"),
        verb: Cow::Borrowed("wsp.invoke"),
        actors: &[],
        args_schema: None,
        reorderable: false,
    },
    Affordance {
        name: Cow::Borrowed("select"),
        verb: Cow::Borrowed("wsp.invoke"),
        actors: &[],
        args_schema: None,
        reorderable: false,
    },
    Affordance {
        name: Cow::Borrowed("pan"),
        verb: Cow::Borrowed("wsp.invoke"),
        actors: &[],
        args_schema: None,
        reorderable: false,
    },
    Affordance {
        name: Cow::Borrowed("zoom"),
        verb: Cow::Borrowed("wsp.invoke"),
        actors: &[],
        args_schema: None,
        reorderable: false,
    },
];

static MUTATION_AXES: &[MutationAxis] = &[
    MutationAxis::new("zoom-step"),
    MutationAxis::new("hit-test-radius"),
];

/// Persisted pan/zoom state. Written through `memory.data` so it
/// survives across frames keyed on the canvas id.
#[derive(Copy, Clone, Debug)]
pub struct CanvasTransform {
    pub offset: egui::Vec2,
    pub scale: f32,
}

impl Default for CanvasTransform {
    fn default() -> Self {
        Self {
            offset: egui::Vec2::ZERO,
            scale: 1.0,
        }
    }
}

/// Freeform 2D painter. Pass a draw closure that receives a `Painter`,
/// the canvas rect, and the current `CanvasTransform`.
pub struct Canvas<F>
where
    F: FnOnce(&egui::Painter, egui::Rect, CanvasTransform),
{
    id_source: egui::Id,
    size: egui::Vec2,
    zoom_step: f32,
    allow_pan: bool,
    allow_zoom: bool,
    tooltip: Option<Tooltip>,
    variant: VariantId,
    paint: F,
}

impl<F> Canvas<F>
where
    F: FnOnce(&egui::Painter, egui::Rect, CanvasTransform),
{
    /// Create a canvas with a stable `id_source`, the allocated `size`,
    /// and the `paint` closure. Defaults: `zoom_step` 0.1, pan and zoom
    /// enabled, no tooltip, variant 0.
    pub fn new(id_source: impl std::hash::Hash, size: egui::Vec2, paint: F) -> Self {
        Self {
            id_source: egui::Id::new(("canon.canvas", id_source)),
            size,
            zoom_step: 0.1,
            allow_pan: true,
            allow_zoom: true,
            tooltip: None,
            variant: 0,
            paint,
        }
    }

    /// Set the fractional scale change per scroll event. Each scroll
    /// multiplies `CanvasTransform::scale` by `1.0 ± step` (sign follows
    /// scroll direction); the result is clamped to `0.05..=50.0`.
    pub fn zoom_step(mut self, step: f32) -> Self {
        self.zoom_step = step;
        self
    }

    /// Enable or disable drag-to-pan, which updates
    /// `CanvasTransform::offset` (default: enabled).
    pub fn allow_pan(mut self, allow: bool) -> Self {
        self.allow_pan = allow;
        self
    }

    /// Enable or disable scroll-to-zoom while the pointer hovers the
    /// canvas, which updates `CanvasTransform::scale` (default: enabled).
    pub fn allow_zoom(mut self, allow: bool) -> Self {
        self.allow_zoom = allow;
        self
    }

    /// Set hover text shown over the canvas (default: none).
    pub fn tooltip(mut self, text: impl Into<Tooltip>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Set the variant id reported on the response (default: 0).
    pub fn variant(mut self, variant: VariantId) -> Self {
        self.variant = variant;
        self
    }
}

impl<F> CanonWidget for Canvas<F>
where
    F: FnOnce(&egui::Painter, egui::Rect, CanvasTransform),
{
    fn id(&self) -> egui::Id {
        self.id_source
    }

    fn identity_uri(&self) -> IdentityUri {
        Cow::Borrowed(IDENTITY)
    }

    fn affordances(&self) -> &[Affordance] {
        AFFORDANCES_ACTIVE
    }

    fn confidence(&self) -> Confidence {
        Confidence::input()
    }

    fn variant_id(&self) -> VariantId {
        self.variant
    }

    fn mutation_axes(&self) -> &[MutationAxis] {
        MUTATION_AXES
    }

    fn tooltip(&self) -> Option<&Tooltip> {
        self.tooltip.as_ref()
    }

    fn show(self, ui: &mut egui::Ui) -> CanonResponse {
        let id = self.id_source;
        let variant = self.variant;
        let tooltip = self.tooltip.clone();

        // Pull the persisted transform. We keep two in-memory copies
        // (Copy + Default) so serde-persistence is not required.
        let transform_key = egui::Id::new(("canon.canvas.xform", id));
        let mut transform: CanvasTransform = ui
            .memory_mut(|m| {
                *m.data
                    .get_temp_mut_or_default::<CanvasTransformCell>(transform_key)
            })
            .0;

        let (resp, painter) = ui.allocate_painter(self.size, egui::Sense::click_and_drag());
        let rect = resp.rect;

        // Apply pan from drag delta.
        let mut acted = false;
        if self.allow_pan && resp.dragged() {
            transform.offset += resp.drag_delta();
            acted = true;
        }

        // Apply zoom from pointer scroll when the cursor is over us.
        if self.allow_zoom && resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll.abs() > f32::EPSILON {
                let factor = 1.0 + (scroll.signum() * self.zoom_step);
                transform.scale = (transform.scale * factor).clamp(0.05, 50.0);
                acted = true;
            }
        }

        if acted {
            ui.memory_mut(|m| {
                *m.data
                    .get_temp_mut_or_default::<CanvasTransformCell>(transform_key) =
                    CanvasTransformCell(transform);
            });
        }

        // Hand the painter + rect + transform to the caller.
        (self.paint)(&painter, rect, transform);

        let mut resp = resp;
        if let Some(tt) = &tooltip {
            resp = resp.on_hover_text(tt.as_ref());
        }

        let chosen: Option<&'static str> = if resp.dragged() {
            Some("pan")
        } else if acted {
            Some("zoom")
        } else if resp.clicked() {
            Some("paint")
        } else {
            None
        };

        CanonResponse::from_egui(resp, Cow::Borrowed(IDENTITY), variant, chosen).with_id_hint(id)
    }
}

/// Wrapper so `CanvasTransform` can live in `memory.data` without
/// needing `SerializableAny`. `get_temp_mut_or_default` requires
/// `'static + Any + Send + Sync + Default + Clone`, which a newtype
/// over the `Copy` transform satisfies trivially.
#[derive(Copy, Clone, Default, Debug)]
struct CanvasTransformCell(CanvasTransform);
