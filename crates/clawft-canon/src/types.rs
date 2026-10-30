//! Canon head types shared by every primitive.
//!
//! Carries the six mandatory fields from ADR-006 (`primitive-head`):
//! `type`, `state`, `affordances`, `confidence`, `variant`, `mutation-axes`,
//! plus the two optional fields from the 2026-04-19 amendment (`tooltip`,
//! per-affordance `reorderable`).

use std::borrow::Cow;

/// Opaque, composer-assigned id stamped on every `surface.update`.
/// Echoed by the renderer on every return-signal so ECC can attribute
/// the echo to the exact pulse that emitted it (ADR-007, session-5:385).
pub type VariantId = u64;

/// Ontology IRI under `ui://` (e.g. `ui://pressable`, `ui://modal`).
/// Stored as `Cow` so static primitives use zero-cost &'static str and
/// dynamic primitives (composition) can own their IRI.
pub type IdentityUri = Cow<'static, str>;

/// Who may invoke an affordance. Default interpretation of absent = `Any`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ActorKind {
    Human,
    Agent,
    Any,
}

/// One entry in `primitive-head.affordances` — what the caller may do
/// *right now* on this primitive, already intersected with governance.
#[derive(Clone, Debug)]
pub struct Affordance {
    /// Symbolic name (stable across renderers).
    pub name: Cow<'static, str>,
    /// WSP verb to invoke (ADR-005).
    pub verb: Cow<'static, str>,
    /// Who may invoke — empty slice = Any.
    pub actors: &'static [ActorKind],
    /// Optional ontology IRI for the args schema.
    pub args_schema: Option<Cow<'static, str>>,
    /// 2026-04-19 amendment — container primitives that permit
    /// drag-to-reorder children declare this on the `reorder` affordance.
    pub reorderable: bool,
}

impl Affordance {
    /// Affordance invoking WSP verb `verb` under symbolic `name`; open to
    /// any actor, no args schema, not reorderable.
    pub const fn new(name: &'static str, verb: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            verb: Cow::Borrowed(verb),
            actors: &[],
            args_schema: None,
            reorderable: false,
        }
    }
}

/// ADR-006 §3 — how the state value was produced. An inference-sourced
/// value without confidence is malformed at the kernel boundary
/// (Cedar-OS negative lesson, Session 9 #2).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ConfidenceSource {
    Deterministic,
    Inference,
    Cache,
    Input,
}

/// `primitive-head.confidence` (ADR-006 §3) — how trustworthy the state
/// value is, and where it came from.
#[derive(Copy, Clone, Debug)]
pub struct Confidence {
    pub source: ConfidenceSource,
    /// 0.0–1.0 point estimate; `None` = not expressible.
    pub value: Option<f32>,
    /// Optional (lo, hi) interval; `None` = not expressible.
    pub interval: Option<(f32, f32)>,
}

impl Confidence {
    /// Confidence for a deterministically computed value: certain (`1.0`),
    /// no interval.
    pub const fn deterministic() -> Self {
        Self {
            source: ConfidenceSource::Deterministic,
            value: Some(1.0),
            interval: None,
        }
    }
    /// Confidence for a value taken directly from user input: certain
    /// (`1.0`), no interval.
    pub const fn input() -> Self {
        Self {
            source: ConfidenceSource::Input,
            value: Some(1.0),
            interval: None,
        }
    }
}

/// One legal GEPA mutation axis plus the reason it can be frozen.
/// Empty axis list = no mutation legal on this primitive (consent flows,
/// `Modal.affordances`, safety affordances — foundations §active-radar).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum FrozenBy {
    Brand,
    Safety,
    Consent,
    UserRequest,
}

/// One entry in `primitive-head.mutation-axes` — a GEPA-mutable axis of the
/// primitive, together with the reasons (if any) it is currently frozen.
#[derive(Clone, Debug)]
pub struct MutationAxis {
    pub name: Cow<'static, str>,
    pub frozen_by: &'static [FrozenBy],
}

impl MutationAxis {
    /// Axis called `name` with no freeze reasons, so it is open to mutation.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name: Cow::Borrowed(name),
            frozen_by: &[],
        }
    }
}

/// ADR-014 modality split — `ui://modal` carries this as a typed field
/// rather than fanning into four primitives.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Modality {
    /// Blocks focus until dismissed — consent, destructive-confirm.
    Modal,
    /// Non-blocking floating surface — palette, inspector.
    Floating,
    /// Transient — tooltip, long-press description (ADR-006 §7).
    Tool,
    /// Auto-dismissing notification.
    Toast,
}

/// Optional head field — hover / accessibility / long-press help text.
/// Future i18n wrapper; for now a plain string.
pub type Tooltip = Cow<'static, str>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confidence_deterministic_is_certain() {
        let c = Confidence::deterministic();
        assert_eq!(c.source, ConfidenceSource::Deterministic);
        assert_eq!(c.value, Some(1.0));
        assert_eq!(c.interval, None);
    }

    #[test]
    fn confidence_input_is_certain() {
        let c = Confidence::input();
        assert_eq!(c.source, ConfidenceSource::Input);
        assert_eq!(c.value, Some(1.0));
        assert_eq!(c.interval, None);
    }

    #[test]
    fn affordance_new_has_defaults() {
        let a = Affordance::new("a", "b");
        assert_eq!(a.name, "a");
        assert_eq!(a.verb, "b");
        assert!(a.actors.is_empty());
        assert!(a.args_schema.is_none());
        assert!(!a.reorderable);
    }

    #[test]
    fn mutation_axis_new_has_no_frozen_by() {
        let m = MutationAxis::new("x");
        assert_eq!(m.name, "x");
        assert!(m.frozen_by.is_empty());
    }
}
