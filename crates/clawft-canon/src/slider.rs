//! `ui://slider` — continuous range binding primitive (ADR-001 row 6).
//!
//! `f64` binding by default; callers needing integer or alternate-type
//! sliders can wrap `Slider` in a small adapter for now. Matches the
//! `Confidence::input()` source discipline since the value originates
//! with the user or substrate.

use std::borrow::Cow;

use egui;

use super::CanonWidget;
use super::response::CanonResponse;
use super::types::{Affordance, Confidence, IdentityUri, MutationAxis, Tooltip, VariantId};

const IDENTITY: &str = "ui://slider";

static AFFORDANCES_ACTIVE: &[Affordance] = &[Affordance {
    name: Cow::Borrowed("adjust"),
    verb: Cow::Borrowed("wsp.set"),
    actors: &[],
    args_schema: None,
    reorderable: false,
}];
static AFFORDANCES_DISABLED: &[Affordance] = &[];

static MUTATION_AXES: &[MutationAxis] = &[
    MutationAxis::new("tick-density"),
    MutationAxis::new("snap-behavior"),
];

/// Continuous range binding.
pub struct Slider<'b> {
    id_source: egui::Id,
    label: Cow<'static, str>,
    value: &'b mut f64,
    min: f64,
    max: f64,
    step: Option<f64>,
    suffix: Option<Cow<'static, str>>,
    enabled: bool,
    tooltip: Option<Tooltip>,
    variant: VariantId,
}

impl<'b> Slider<'b> {
    /// Creates a slider that edits `value` within `min..=max`, labelled `label`.
    pub fn new(
        id_source: impl std::hash::Hash,
        label: impl Into<Cow<'static, str>>,
        value: &'b mut f64,
        min: f64,
        max: f64,
    ) -> Self {
        Self {
            id_source: egui::Id::new(("canon.slider", id_source)),
            label: label.into(),
            value,
            min,
            max,
            step: None,
            suffix: None,
            enabled: true,
            tooltip: None,
            variant: 0,
        }
    }

    /// Snaps the value to multiples of `step`, in the same units as the value itself.
    pub fn step(mut self, step: f64) -> Self {
        self.step = Some(step);
        self
    }

    /// Appends `suffix` (such as a unit) to the value text shown on the slider.
    pub fn suffix(mut self, suffix: impl Into<Cow<'static, str>>) -> Self {
        self.suffix = Some(suffix.into());
        self
    }

    /// Enables or disables the slider; a disabled slider cannot be edited.
    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    /// Sets the hover tooltip shown for the slider.
    pub fn tooltip(mut self, text: impl Into<Tooltip>) -> Self {
        self.tooltip = Some(text.into());
        self
    }

    /// Selects the canon variant reported in the slider's response.
    pub fn variant(mut self, variant: VariantId) -> Self {
        self.variant = variant;
        self
    }
}

impl CanonWidget for Slider<'_> {
    fn id(&self) -> egui::Id {
        self.id_source
    }

    fn identity_uri(&self) -> IdentityUri {
        Cow::Borrowed(IDENTITY)
    }

    fn affordances(&self) -> &[Affordance] {
        if self.enabled {
            AFFORDANCES_ACTIVE
        } else {
            AFFORDANCES_DISABLED
        }
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
        let enabled = self.enabled;
        let tooltip = self.tooltip.clone();
        let label = self.label;
        let min = self.min;
        let max = self.max;
        let step = self.step;
        let suffix = self.suffix;

        let mut resp = ui
            .scope(|ui| {
                if !enabled {
                    ui.disable();
                }
                let mut s = egui::Slider::new(self.value, min..=max).text(label.as_ref());
                if let Some(st) = step {
                    s = s.step_by(st);
                }
                if let Some(sfx) = suffix {
                    s = s.suffix(sfx.into_owned());
                }
                ui.add(s)
            })
            .inner;

        if let Some(tt) = &tooltip {
            resp = resp.on_hover_text(tt.as_ref());
        }

        let chosen: Option<&'static str> = if enabled && resp.changed() {
            Some("adjust")
        } else {
            None
        };

        CanonResponse::from_egui(resp, Cow::Borrowed(IDENTITY), variant, chosen).with_id_hint(id)
    }
}
