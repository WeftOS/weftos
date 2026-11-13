//! Project governance overlay and the tighten-only merge (ADR-103 A6, D8).
//!
//! `<root>/.weftos/overlay.toml` lets a project add rules on top of the rules
//! the user daemon pushes down. It can only tighten:
//!
//! - `deny` and `require_approval` entries are unioned with the parent's;
//! - every numeric limit becomes `min(parent, overlay)`;
//! - a boolean limit becomes `parent OR overlay`;
//! - an overlay may not contain `permit` or `deactivate`, a rule id that
//!   exists in the parent (compared trimmed and case-insensitively), or a
//!   limit above the parent's (`false` over a parent `true` counts).
//!
//! Every violation is a hard [`OverlayError`] naming the offending key; the
//! merge never clamps silently. This module is pure types and the merge
//! function: loading the file at boot, the signed parent policy and the
//! effective-rules hash belong to the kernel (package E).
//!
//! Action globs are an exact action name or a prefix ending in one trailing
//! `*`. `workload.place*` therefore also matches `workload.placement.x`;
//! that is the documented semantics, not a bug.
//!
//! An entry may carry `principal`: who the rule applies to. It uses the same
//! exact-or-trailing-`*` form over the caller-supplied principal id (the
//! `agent_id` of a gate or `governance.check` call), see
//! [`valid_principal_selector`]. Without it the rule applies to every
//! principal. A scoped rule only adds a restriction for the principals it
//! names, so the tighten-only rule is unchanged.
//!
//! ```toml
//! schema = 1
//! [[deny]]
//! id = "project.no-shell"
//! actions = ["tool.shell_exec", "workload.place*"]
//! reason = "this project never runs shell tools"
//! [[require_approval]]
//! actions = ["workload.start*"]
//! [[deny]]
//! id = "project.bot-no-write"
//! actions = ["tool.write"]
//! principal = "mesh-agent-7"   # only this asker; omit to apply to everyone
//! [limits]
//! risk_threshold = 0.5
//! max_processes = 32
//! spawn_budget = 4
//! human_approval_required = true
//! ```

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

/// Overlay file format version.
pub const OVERLAY_SCHEMA: u32 = 1;

/// Numeric limits may only go down, flags only up.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    /// Risk threshold in `[0, 1]`; effective = min(parent, overlay).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_threshold: Option<f64>,
    /// Process cap; effective = min.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_processes: Option<u64>,
    /// Spawn budget; effective = min.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn_budget: Option<u64>,
    /// Force human approval; effective = parent OR overlay.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub human_approval_required: Option<bool>,
}

/// One `[[deny]]` entry: a new deny rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayDeny {
    /// Rule id; must not exist in the parent.
    pub id: String,
    /// Action globs to deny.
    pub actions: Vec<String>,
    /// Why (shown in the denial).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Apply only to this principal (exact id, or a prefix ending in one `*`).
    /// `None`: every principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

/// One `[[require_approval]]` entry: force human approval for these actions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OverlayApproval {
    /// Optional rule id; when present it must not exist in the parent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// Action globs that need approval.
    pub actions: Vec<String>,
    /// Why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// Apply only to this principal (exact id, or a prefix ending in one `*`).
    /// `None`: every principal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub principal: Option<String>,
}

/// `<root>/.weftos/overlay.toml`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OverlayFile {
    /// Format version ([`OVERLAY_SCHEMA`]).
    #[serde(default = "schema_one")]
    pub schema: u32,
    /// New deny rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deny: Vec<OverlayDeny>,
    /// Actions forced to human approval.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub require_approval: Vec<OverlayApproval>,
    /// Tightened limits.
    #[serde(default)]
    pub limits: Limits,
    /// Any other top-level key; always rejected by [`merge`] (`permit` and
    /// `deactivate` get their own error).
    #[serde(flatten, skip_serializing)]
    pub extra: BTreeMap<String, toml::Value>,
}

fn schema_one() -> u32 {
    OVERLAY_SCHEMA
}

impl Default for OverlayFile {
    fn default() -> Self {
        Self {
            schema: OVERLAY_SCHEMA,
            deny: Vec::new(),
            require_approval: Vec::new(),
            limits: Limits::default(),
            extra: BTreeMap::new(),
        }
    }
}

impl OverlayFile {
    /// Parse overlay TOML. Shape errors surface here; relaxations surface in
    /// [`merge`].
    pub fn from_toml(s: &str) -> Result<Self, OverlayError> {
        toml::from_str(s).map_err(|e| OverlayError::Parse(e.to_string()))
    }
}

/// What the parent (user) policy contributes to a merge.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ParentView {
    /// Every parent rule id (for the no-shadowing check).
    pub rule_ids: Vec<String>,
    /// Action globs the parent denies.
    pub deny_actions: Vec<String>,
    /// Action globs the parent forces to approval.
    pub require_approval_actions: Vec<String>,
    /// Parent limits; `None` means unlimited / off.
    pub limits: Limits,
}

/// The tightened result.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EffectiveOverlay {
    /// Parent and overlay deny globs, sorted and de-duplicated.
    pub deny_actions: Vec<String>,
    /// Parent and overlay approval globs, sorted and de-duplicated.
    pub require_approval_actions: Vec<String>,
    /// The overlay's own deny rules (to append to the parent's rules).
    pub deny_rules: Vec<OverlayDeny>,
    /// The overlay's own approval rules.
    pub approval_rules: Vec<OverlayApproval>,
    /// Effective limits.
    pub limits: Limits,
}

/// An overlay was refused. Every variant names the offending key.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OverlayError {
    /// The TOML did not parse into [`OverlayFile`].
    #[error("overlay does not parse: {0}")]
    Parse(String),
    /// `schema` is not [`OVERLAY_SCHEMA`].
    #[error("overlay `schema` is {0}, expected {OVERLAY_SCHEMA}")]
    Schema(u32),
    /// `permit` or `deactivate`: an overlay may only tighten.
    #[error("overlay key `{0}` is not allowed: an overlay can only tighten")]
    Forbidden(String),
    /// A top-level key this format does not define.
    #[error("overlay has unknown key `{0}`")]
    UnknownKey(String),
    /// A rule id that is empty, padded with whitespace or has odd characters.
    #[error("`{key}`: malformed rule id {id:?}")]
    MalformedId { key: String, id: String },
    /// Two overlay rules share an id (compared case-insensitively).
    #[error("`{key}`: duplicate rule id {id:?}")]
    DuplicateId { key: String, id: String },
    /// An overlay rule id exists in the parent: no shadowing.
    #[error("`{key}`: rule id {id:?} shadows a parent rule")]
    ShadowsParent { key: String, id: String },
    /// An action list is empty.
    #[error("`{key}`: no actions")]
    EmptyActions { key: String },
    /// An action glob is empty, has whitespace or a `*` that is not last.
    #[error("`{key}`: malformed action glob {pattern:?}")]
    MalformedGlob { key: String, pattern: String },
    /// A principal selector is empty, too long, has characters an id cannot
    /// have, or a `*` that is not last.
    #[error("`{key}`: malformed principal selector {pattern:?}")]
    MalformedPrincipal { key: String, pattern: String },
    /// A limit is outside its valid range (NaN, negative, above 1).
    #[error("`{key}`: invalid value")]
    InvalidLimit { key: String },
    /// A limit that loosens the parent's.
    #[error("`{key}`: {overlay} relaxes the parent value {parent}; set it to {parent} or lower in overlay.toml")]
    Relaxes {
        key: String,
        parent: String,
        overlay: String,
    },
}

/// True when `pattern` is a well-formed action glob.
pub fn valid_glob(pattern: &str) -> bool {
    let body = pattern.strip_suffix('*').unwrap_or(pattern);
    !pattern.is_empty() && !body.contains('*') && !pattern.chars().any(char::is_whitespace)
}

/// Longest principal id or selector, in bytes.
pub const MAX_PRINCIPAL_LEN: usize = 128;

/// True when `pattern` is a well-formed principal selector: an id (ASCII
/// letters, digits and `. _ : @ -`, at most [`MAX_PRINCIPAL_LEN`] bytes), or
/// such a prefix followed by one trailing `*`. The id alphabet is the one the
/// Tapestry client allows for a directory id, so no valid id is unmatchable.
pub fn valid_principal_selector(pattern: &str) -> bool {
    let body = pattern.strip_suffix('*').unwrap_or(pattern);
    !pattern.is_empty()
        && pattern.len() <= MAX_PRINCIPAL_LEN
        && body
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | ':' | '@' | '-'))
}

/// Whether `action` matches `pattern` (exact, or prefix when it ends `*`).
pub fn glob_matches(pattern: &str, action: &str) -> bool {
    match pattern.strip_suffix('*') {
        Some(prefix) => action.starts_with(prefix),
        None => pattern == action,
    }
}

fn check_id(key: String, id: &str) -> Result<String, OverlayError> {
    let ok = !id.is_empty()
        && id == id.trim()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':'));
    if ok {
        Ok(id.to_ascii_lowercase())
    } else {
        Err(OverlayError::MalformedId { key, id: id.into() })
    }
}

fn check_actions(key: &str, actions: &[String]) -> Result<(), OverlayError> {
    if actions.is_empty() {
        return Err(OverlayError::EmptyActions { key: key.into() });
    }
    for (i, a) in actions.iter().enumerate() {
        if !valid_glob(a) {
            return Err(OverlayError::MalformedGlob {
                key: format!("{key}[{i}]"),
                pattern: a.clone(),
            });
        }
    }
    Ok(())
}

fn check_principal(key: String, principal: Option<&str>) -> Result<(), OverlayError> {
    match principal {
        Some(p) if !valid_principal_selector(p) => Err(OverlayError::MalformedPrincipal {
            key,
            pattern: p.into(),
        }),
        _ => Ok(()),
    }
}

fn tighten_rule_id(
    key: &str,
    id: &str,
    parent: &BTreeSet<String>,
    seen: &mut BTreeSet<String>,
) -> Result<(), OverlayError> {
    let norm = check_id(format!("{key}.id"), id)?;
    if parent.contains(&norm) {
        return Err(OverlayError::ShadowsParent {
            key: format!("{key}.id"),
            id: id.into(),
        });
    }
    if !seen.insert(norm) {
        return Err(OverlayError::DuplicateId {
            key: format!("{key}.id"),
            id: id.into(),
        });
    }
    Ok(())
}

fn tighten_num<T: Copy + PartialOrd + std::fmt::Display>(
    key: &str,
    parent: Option<T>,
    overlay: Option<T>,
) -> Result<Option<T>, OverlayError> {
    match (parent, overlay) {
        (Some(p), Some(o)) if o > p => Err(OverlayError::Relaxes {
            key: key.into(),
            parent: p.to_string(),
            overlay: o.to_string(),
        }),
        (_, Some(o)) => Ok(Some(o)),
        (p, None) => Ok(p),
    }
}

fn merge_limits(parent: &Limits, o: &Limits) -> Result<Limits, OverlayError> {
    if let Some(r) = o.risk_threshold
        && !(r.is_finite() && (0.0..=1.0).contains(&r))
    {
        return Err(OverlayError::InvalidLimit {
            key: "limits.risk_threshold".into(),
        });
    }
    let human = match (parent.human_approval_required, o.human_approval_required) {
        (Some(true), Some(false)) => {
            return Err(OverlayError::Relaxes {
                key: "limits.human_approval_required".into(),
                parent: "true".into(),
                overlay: "false".into(),
            });
        }
        (None, None) => None,
        (p, o) => Some(p.unwrap_or(false) || o.unwrap_or(false)),
    };
    Ok(Limits {
        risk_threshold: tighten_num(
            "limits.risk_threshold",
            parent.risk_threshold,
            o.risk_threshold,
        )?,
        max_processes: tighten_num("limits.max_processes", parent.max_processes, o.max_processes)?,
        spawn_budget: tighten_num("limits.spawn_budget", parent.spawn_budget, o.spawn_budget)?,
        human_approval_required: human,
    })
}

fn union(a: &[String], b: impl Iterator<Item = String>) -> Vec<String> {
    let set: BTreeSet<String> = a.iter().cloned().chain(b).collect();
    set.into_iter().collect()
}

/// Merge `overlay` onto `parent`, tighten-only. The first violation (in the
/// order: parent limits, schema, forbidden keys, deny rules, approval rules, limits) is
/// returned and nothing is clamped.
pub fn merge(parent: &ParentView, overlay: &OverlayFile) -> Result<EffectiveOverlay, OverlayError> {
    if overlay.schema != OVERLAY_SCHEMA {
        return Err(OverlayError::Schema(overlay.schema));
    }
    if let Some(key) = overlay.extra.keys().next() {
        return Err(match key.as_str() {
            "permit" | "deactivate" => OverlayError::Forbidden(key.clone()),
            _ => OverlayError::UnknownKey(key.clone()),
        });
    }
    if let Some(r) = parent.limits.risk_threshold
        && !(r.is_finite() && (0.0..=1.0).contains(&r))
    {
        return Err(OverlayError::InvalidLimit {
            key: "parent.limits.risk_threshold".into(),
        });
    }
    let parent_ids: BTreeSet<String> = parent
        .rule_ids
        .iter()
        .map(|i| i.trim().to_ascii_lowercase())
        .collect();
    let mut seen = BTreeSet::new();
    for (i, d) in overlay.deny.iter().enumerate() {
        let key = format!("deny[{i}]");
        tighten_rule_id(&key, &d.id, &parent_ids, &mut seen)?;
        check_actions(&format!("{key}.actions"), &d.actions)?;
        check_principal(format!("{key}.principal"), d.principal.as_deref())?;
    }
    for (i, a) in overlay.require_approval.iter().enumerate() {
        let key = format!("require_approval[{i}]");
        if let Some(id) = &a.id {
            tighten_rule_id(&key, id, &parent_ids, &mut seen)?;
        }
        check_actions(&format!("{key}.actions"), &a.actions)?;
        check_principal(format!("{key}.principal"), a.principal.as_deref())?;
    }
    let limits = merge_limits(&parent.limits, &overlay.limits)?;
    Ok(EffectiveOverlay {
        deny_actions: union(
            &parent.deny_actions,
            overlay.deny.iter().flat_map(|d| d.actions.iter().cloned()),
        ),
        require_approval_actions: union(
            &parent.require_approval_actions,
            overlay
                .require_approval
                .iter()
                .flat_map(|a| a.actions.iter().cloned()),
        ),
        deny_rules: overlay.deny.clone(),
        approval_rules: overlay.require_approval.clone(),
        limits,
    })
}
