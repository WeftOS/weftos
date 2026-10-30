//! `ui://grid` — two-axis regular layout primitive (ADR-001 row 8).
//!
//! Pure container: no affordances fire off the grid itself (its
//! children carry their own). `mutation-axes` cover `num-columns` and
//! `gap`.

use std::borrow::Cow;

use egui;

use super::CanonWidget;
use super::response::CanonResponse;
use super::types::{Affordance, Confidence, IdentityUri, MutationAxis, Tooltip, VariantId};

const IDENTITY: &str = "ui://grid";

static AFFORDANCES_NONE: &[Affordance] = &[];

static MUTATION_AXES: &[MutationAxis] =
    &[MutationAxis::new("num-columns"), MutationAxis::new("gap")];

/// Two-axis regular layout. Takes a child-builder closure that is
/// invoked inside `egui::Grid::new(id).show(...)`. The closure is the
/// caller's responsibility for emitting `ui.end_row()` between rows.
pub struct Grid<F>
where
    F: FnOnce(&mut egui::Ui),
{
    id_source: egui::Id,
    num_columns: usize,
    min_col_width: Option<f32>,
    spacing: Option<egui::Vec2>,
    striped: bool,
    tooltip: Option<Tooltip>,
    variant: VariantId,
    build: F,
}

impl<F> Grid<F>
where
    F: FnOnce(&mut egui::Ui),
{
    /// Construct a `Grid` with `num_columns` columns; `build` emits the cells.
    pub fn new(id_source: impl std::hash::Hash, num_columns: usize, build: F) -> Self {
        Self {
            id_source: egui::Id::new(("canon.grid", id_source)),
            num_columns,
            min_col_width: None,
            spacing: None,
            striped: false,
            tooltip: None,
            variant: 0,
            build,
        }
    }

    /// Set the minimum width of each column.
    pub fn min_col_width(mut self, w: f32) -> Self {
        self.min_col_width = Some(w);
        self
    }

    /// Override the horizontal and vertical spacing between cells.
    pub fn spacing(mut self, spacing: egui::Vec2) -> Self {
        self.spacing = Some(spacing);
        self
    }

    /// Shade alternating rows when `striped` is true.
    pub fn striped(mut self, striped: bool) -> Self {
        self.striped = striped;
        self
    }

    /// Attach a tooltip shown when hovering the grid.
    pub fn tooltip(mut self, text: impl Into<Tooltip>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Select the visual variant for this grid.
    pub fn variant(mut self, variant: VariantId) -> Self {
        self.variant = variant;
        self
    }
}

impl<F> CanonWidget for Grid<F>
where
    F: FnOnce(&mut egui::Ui),
{
    fn id(&self) -> egui::Id {
        self.id_source
    }

    fn identity_uri(&self) -> IdentityUri {
        Cow::Borrowed(IDENTITY)
    }

    fn affordances(&self) -> &[Affordance] {
        AFFORDANCES_NONE
    }

    fn confidence(&self) -> Confidence {
        Confidence::deterministic()
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

        let mut grid = egui::Grid::new(id)
            .num_columns(self.num_columns)
            .striped(self.striped);
        if let Some(w) = self.min_col_width {
            grid = grid.min_col_width(w);
        }
        if let Some(s) = self.spacing {
            grid = grid.spacing(s);
        }

        let inner = grid.show(ui, self.build);
        let mut resp = inner.response;
        if let Some(tt) = &tooltip {
            resp = resp.on_hover_text(tt.as_ref());
        }

        CanonResponse::from_egui(resp, Cow::Borrowed(IDENTITY), variant, None).with_id_hint(id)
    }
}
