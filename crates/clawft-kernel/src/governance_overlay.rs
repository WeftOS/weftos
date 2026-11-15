//! Project governance overlay, kernel side (ADR-103 A6, D8, Phase 2 package E).
//!
//! Package A owns the file format and the tighten-only rule
//! ([`clawft_types::config::overlay`]); this module adds what only the
//! kernel can do:
//!
//! * [`load_overlay`]: read `<root>/.weftos/overlay.toml`. A missing file is
//!   the empty overlay (nothing is added); every other failure (unreadable,
//!   not a regular file, too large, not UTF-8, does not parse) is an error,
//!   never an empty overlay. A broken overlay must not become fewer rules.
//! * [`merge`]: the user's [`ParentPolicy`] plus the overlay become one
//!   [`Effective`] rule set, limits and three hashes. The merge itself is
//!   package A's [`overlay_merge`]; nothing here relaxes it and nothing is
//!   clamped silently.
//! * [`Effective::into_rules`]: the rules for `GovernanceEngine::add_rule`.
//!
//! # Rule mapping
//!
//! The parent's rules are carried over verbatim, `active` and severity
//! included. Each overlay `deny` glob becomes one blocking rule that fires on
//! the action alone (`force_on_match`) and is tagged
//! [`OVERLAY_DENY_TAG`], so the engine answers `Deny` even when the
//! engine-wide human-approval flag is on. Each `require_approval` glob is the
//! same shape tagged [`OVERLAY_APPROVAL_TAG`] and answers `EscalateToHuman`.
//! Globs are an exact action or a prefix ending in one `*`: `workload.place*`
//! also matches `workload.placement.x` (documented in package A).
//!
//! An entry's optional `principal` becomes the rule's `principal_selector`
//! (exact id or one trailing `*`, matched against the request's `agent_id`),
//! so the rule applies to that principal only. A parent rule scoped to a
//! principal still counts as a parent deny when checking that an approval
//! does not overlap one (conservative: it can only refuse more).
//!
//! # Hashes
//!
//! All three are SHA-256 over a domain tag plus canonical JSON
//! ([`canonical_json`]); floats never appear (a risk threshold is hashed as
//! the hex of its IEEE-754 bits).
//!
//! * `parent_hash`: `"weftos-parent-rules-v1\n"` + `{limits, rules}`, rules
//!   sorted by id. Recomputed from the rules, never read from the file.
//! * `overlay_hash`: `"weftos-overlay-v1\n"` + the parsed overlay, so
//!   comments and whitespace do not change it.
//! * `effective_hash`: `"weftos-effective-rules-v1\n"` + `{limits,
//!   overlay_hash, parent_hash, rules}` with the merged rules sorted by id.
//!   Each rule is its full serialized form, so `active`, severity and
//!   selectors are covered.

use std::path::Path;

use clawft_types::config::overlay::{
    self as overlay_merge, EffectiveOverlay, Limits, OverlayFile, ParentView,
};
use clawft_types::project::canon::{canonical_json, hex_encode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::governance::{
    GovernanceBranch, GovernanceRule, GovernanceRuleType, OVERLAY_APPROVAL_TAG, OVERLAY_DENY_TAG,
    RuleSeverity,
};
use crate::parent_policy::{ParentPolicy, ParentPolicyError, parent_rules_hash};

/// Largest overlay or parent-policy file read, in bytes.
pub const MAX_FILE_BYTES: u64 = 1024 * 1024;
/// Domain tag of the overlay hash.
pub const OVERLAY_HASH_DOMAIN: &str = "weftos-overlay-v1\n";
/// Domain tag of the effective-rules hash.
pub const EFFECTIVE_HASH_DOMAIN: &str = "weftos-effective-rules-v1\n";

/// Why an overlay, a parent policy or their merge was refused.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum OverlayError {
    /// A file could not be read.
    #[error("{path}: {reason}")]
    Io { path: String, reason: String },
    /// A file is over [`MAX_FILE_BYTES`].
    #[error("{path}: {bytes} bytes is over the {MAX_FILE_BYTES} byte limit")]
    TooLarge { path: String, bytes: u64 },
    /// The overlay broke the tighten-only rule (package A's error).
    #[error("{0}")]
    Overlay(#[from] overlay_merge::OverlayError),
    /// The parent policy is not acceptable.
    #[error("parent policy: {0}")]
    Parent(#[from] ParentPolicyError),
    /// Two rules the merge would emit share an id.
    #[error("`{key}`: rule id {id:?} collides with another rule")]
    IdCollision { key: String, id: String },
    /// `profile = "project"` but the kernel is not a child kernel.
    #[error("profile `project` needs a child runtime root (RootSource::Child)")]
    NotAChild,
    /// The project certificate is missing or does not verify.
    #[error("project certificate: {0}")]
    Cert(String),
    /// An overlay approval glob overlaps a glob the parent denies; an
    /// approval must never turn a deny into a question.
    #[error("`{key}`: approval glob {glob:?} overlaps the parent deny {parent_glob:?}")]
    ApprovalOverlapsDeny {
        key: String,
        glob: String,
        parent_glob: String,
    },
    /// `overlay.toml` is gone although a non-empty overlay is in force.
    /// Clear an overlay with an empty file, never by deleting it.
    #[error("overlay.toml is missing but a non-empty overlay was applied; write an empty file to clear it")]
    OverlayMissing,
    /// The rollback pin exists but cannot be read.
    #[error("{path}: {reason}")]
    PinCorrupt { path: String, reason: String },
    /// No rollback pin, yet the chain records an applied overlay.
    #[error("rollback pin is missing although the chain records an applied governance overlay")]
    PinMissing,
    /// The project was revoked by its user daemon.
    #[error("project revoked ({0})")]
    Revoked(String),
}

impl OverlayError {
    /// The offending key (what `weaver doctor` prints and `BootRefused`
    /// names).
    pub fn key(&self) -> String {
        use overlay_merge::OverlayError as E;
        match self {
            Self::Io { path, .. } | Self::TooLarge { path, .. } => path.clone(),
            Self::Overlay(e) => match e {
                E::Parse(_) => "overlay.toml".into(),
                E::Schema(_) => "schema".into(),
                E::Forbidden(k) | E::UnknownKey(k) => k.clone(),
                E::MalformedId { key, .. }
                | E::DuplicateId { key, .. }
                | E::ShadowsParent { key, .. }
                | E::EmptyActions { key }
                | E::MalformedGlob { key, .. }
                | E::MalformedPrincipal { key, .. }
                | E::InvalidLimit { key }
                | E::Relaxes { key, .. } => key.clone(),
            },
            Self::Parent(e) => e.key(),
            Self::IdCollision { key, .. } => key.clone(),
            Self::NotAChild => "kernel.profile".into(),
            Self::Cert(_) => "project.cert.json".into(),
            Self::ApprovalOverlapsDeny { key, .. } => key.clone(),
            Self::OverlayMissing => "overlay.toml".into(),
            Self::PinCorrupt { path, .. } => path.clone(),
            Self::PinMissing => format!("state/{}", crate::overlay_trust::VERSION_PIN_FILE),
            Self::Revoked(_) => "revoked".into(),
        }
    }

    /// The message `BootRefused` carries.
    pub fn boot_message(&self) -> String {
        format!(
            "governance overlay refused at `{}`: {self}; fix it and start again (a project never starts with fewer rules than its parent)",
            self.key()
        )
    }
}

/// A parsed overlay plus its hash.
#[derive(Debug, Clone, PartialEq)]
pub struct Overlay {
    /// The parsed file.
    pub file: OverlayFile,
    /// `overlay_hash`.
    pub hash: [u8; 32],
}

impl Overlay {
    /// The overlay that adds nothing (what a missing file means).
    pub fn empty() -> Self {
        Self::from_file(OverlayFile::default())
    }

    /// Wrap a parsed file and hash it.
    pub fn from_file(file: OverlayFile) -> Self {
        let hash = overlay_hash(&file);
        Self { file, hash }
    }

    /// Parse overlay TOML text.
    pub fn from_toml(s: &str) -> Result<Self, OverlayError> {
        Ok(Self::from_file(OverlayFile::from_toml(s)?))
    }
}

/// Read a size-capped regular file; `Ok(None)` when it does not exist.
pub(crate) fn read_capped(path: &Path) -> Result<Option<String>, OverlayError> {
    let io = |reason: String| OverlayError::Io {
        path: path.display().to_string(),
        reason,
    };
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(e.to_string())),
    };
    if !meta.is_file() {
        return Err(io("not a regular file".into()));
    }
    if meta.len() > MAX_FILE_BYTES {
        return Err(OverlayError::TooLarge {
            path: path.display().to_string(),
            bytes: meta.len(),
        });
    }
    // Read through a bounded reader: the file may grow after `metadata`.
    use std::io::Read;
    let mut buf = String::new();
    std::fs::File::open(path)
        .and_then(|f| f.take(MAX_FILE_BYTES + 1).read_to_string(&mut buf))
        .map_err(|e| io(e.to_string()))?;
    if buf.len() as u64 > MAX_FILE_BYTES {
        return Err(OverlayError::TooLarge {
            path: path.display().to_string(),
            bytes: buf.len() as u64,
        });
    }
    Ok(Some(buf))
}

/// Load `overlay.toml`. A missing file is [`Overlay::empty`]; any other
/// failure is an error (fail closed, ADR-103 A6 decision 2).
pub fn load_overlay(path: &Path) -> Result<Overlay, OverlayError> {
    match read_capped(path)? {
        None => Ok(Overlay::empty()),
        Some(text) => Overlay::from_toml(&text),
    }
}

/// `limits` as a canonical JSON value: no floats.
pub(crate) fn limits_value(l: &Limits) -> Value {
    json!({
        "risk_threshold_bits": l.risk_threshold.map(|r| format!("{:016x}", r.to_bits())),
        "max_processes": l.max_processes,
        "spawn_budget": l.spawn_budget,
        "human_approval_required": l.human_approval_required,
    })
}

pub(crate) fn norm_id(id: &str) -> String {
    id.trim().to_ascii_lowercase()
}

/// Rules as canonical JSON values, sorted by normalized id then by content
/// so equal sets hash equally whatever their order.
pub(crate) fn sorted_rule_values(rules: &[GovernanceRule]) -> Vec<Value> {
    let mut keyed: Vec<(String, String, Value)> = rules
        .iter()
        .map(|r| {
            let v = serde_json::to_value(r).unwrap_or(Value::Null);
            (norm_id(&r.id), canonical_json(&v), v)
        })
        .collect();
    keyed.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    keyed.into_iter().map(|(_, _, v)| v).collect()
}

pub(crate) fn sha256_domain(domain: &str, body: &Value) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(domain.as_bytes());
    h.update(canonical_json(body).as_bytes());
    h.finalize().into()
}

fn overlay_hash(f: &OverlayFile) -> [u8; 32] {
    let mut deny: Vec<Value> = f
        .deny
        .iter()
        .map(|d| {
            let mut actions = d.actions.clone();
            actions.sort();
            let mut v = json!({"id": norm_id(&d.id), "actions": actions, "reason": d.reason});
            // Only when set, so an overlay without it keeps the hash it had.
            if let Some(p) = &d.principal {
                v["principal"] = json!(p);
            }
            v
        })
        .collect();
    deny.sort_by_key(canonical_json);
    let mut approval: Vec<Value> = f
        .require_approval
        .iter()
        .map(|a| {
            let mut actions = a.actions.clone();
            actions.sort();
            let mut v = json!({"id": a.id.as_deref().map(norm_id), "actions": actions, "reason": a.reason});
            if let Some(p) = &a.principal {
                v["principal"] = json!(p);
            }
            v
        })
        .collect();
    approval.sort_by_key(canonical_json);
    sha256_domain(
        OVERLAY_HASH_DOMAIN,
        &json!({
            "schema": f.schema,
            "deny": deny,
            "require_approval": approval,
            "limits": limits_value(&f.limits),
        }),
    )
}

/// The merged result a project kernel runs with.
#[derive(Debug, Clone)]
pub struct Effective {
    /// Parent rules (verbatim) then the overlay's rules.
    pub rules: Vec<GovernanceRule>,
    /// Effective limits: `min` for numbers, `OR` for flags.
    pub limits: Limits,
    /// Every action glob denied by a parent or overlay rule.
    pub deny_actions: Vec<String>,
    /// Every action glob forced to human approval.
    pub require_approval_actions: Vec<String>,
    /// Hash of the parent's rules and limits.
    pub parent_hash: [u8; 32],
    /// Hash of the overlay.
    pub overlay_hash: [u8; 32],
    /// Hash of the merged rule set; the chain's `rule_hash`.
    pub effective_hash: [u8; 32],
    /// The parent policy version this was merged from.
    pub parent_version: u64,
}

impl Effective {
    /// The rules, to feed `GovernanceEngine::add_rule` one by one.
    pub fn into_rules(self) -> Vec<GovernanceRule> {
        self.rules
    }

    /// Engine threshold: the effective limit, else `default`.
    pub fn risk_threshold(&self, default: f64) -> f64 {
        self.limits.risk_threshold.unwrap_or(default)
    }

    /// Engine human-approval flag: the effective limit, else `default`.
    pub fn human_approval(&self, default: bool) -> bool {
        self.limits.human_approval_required.unwrap_or(default)
    }

    /// `effective_hash` as lowercase hex.
    pub fn effective_hash_hex(&self) -> String {
        hex_encode(&self.effective_hash)
    }
}

fn selector_rule(
    id: String,
    glob: &str,
    reason: &str,
    severity: RuleSeverity,
    tag: &str,
    principal: Option<&str>,
) -> GovernanceRule {
    GovernanceRule {
        id,
        description: reason.to_owned(),
        branch: GovernanceBranch::Judicial,
        severity,
        active: true,
        reference_url: None,
        sop_category: Some(tag.to_owned()),
        rule_type: GovernanceRuleType::General,
        action_selector: Some(glob.to_owned()),
        tool_selector: None,
        force_on_match: true,
        principal_selector: principal.map(str::to_owned),
    }
}

/// One rule per action glob; `id` for one glob, `id[n]` for several.
fn expand(
    base: &str,
    actions: &[String],
    reason: &str,
    tag: &str,
    principal: Option<&str>,
    out: &mut Vec<GovernanceRule>,
) {
    for (n, glob) in actions.iter().enumerate() {
        let id = if actions.len() == 1 {
            base.to_owned()
        } else {
            format!("{base}[{n}]")
        };
        out.push(selector_rule(id, glob, reason, RuleSeverity::Blocking, tag, principal));
    }
}

/// What the merge needs to know about the parent's rules.
pub(crate) fn parent_view(p: &ParentPolicy) -> ParentView {
    let selector = |r: &GovernanceRule| {
        r.active
            && matches!(r.severity, RuleSeverity::Blocking | RuleSeverity::Critical)
            && r.force_on_match
            && r.rule_type == GovernanceRuleType::General
            && r.tool_selector.is_none()
    };
    let mut deny = Vec::new();
    let mut approval = Vec::new();
    for r in p.rules.iter().filter(|r| selector(r)) {
        let Some(g) = r.action_selector.clone() else {
            continue;
        };
        if r.sop_category.as_deref() == Some(OVERLAY_APPROVAL_TAG) {
            approval.push(g);
        } else {
            deny.push(g);
        }
    }
    ParentView {
        rule_ids: p.rules.iter().map(|r| r.id.clone()).collect(),
        deny_actions: deny,
        require_approval_actions: approval,
        limits: p.limits,
    }
}

/// True when some action matches both globs (exact, or a prefix ending `*`).
pub(crate) fn globs_overlap(a: &str, b: &str) -> bool {
    match (a.strip_suffix('*'), b.strip_suffix('*')) {
        (Some(pa), Some(pb)) => pa.starts_with(pb) || pb.starts_with(pa),
        (Some(pa), None) => b.starts_with(pa),
        (None, Some(pb)) => a.starts_with(pb),
        (None, None) => a == b,
    }
}

/// Merge `overlay` onto `parent` (tighten-only). The parent's signature is
/// NOT checked here (see [`crate::parent_policy::verify_parent_policy`]);
/// its rule hash is recomputed rather than trusted.
pub fn merge(parent: &ParentPolicy, overlay: &Overlay) -> Result<Effective, OverlayError> {
    // A parent with no threshold runs the engine default, so that default is
    // the ceiling an overlay may not raise (otherwise 0.9 over "none" would
    // loosen a 0.7 engine).
    let mut view = parent_view(parent);
    view.limits
        .risk_threshold
        .get_or_insert(crate::overlay_runtime::DEFAULT_RISK_THRESHOLD);
    let EffectiveOverlay {
        deny_actions,
        require_approval_actions,
        deny_rules,
        approval_rules,
        limits,
    } = overlay_merge::merge(&view, &overlay.file)?;

    // `max_processes = 0` would leave no slot for the kernel's own process.
    if limits.max_processes == Some(0) {
        return Err(OverlayError::Overlay(overlay_merge::OverlayError::InvalidLimit {
            key: "limits.max_processes".into(),
        }));
    }
    for (i, a) in approval_rules.iter().enumerate() {
        for (j, g) in a.actions.iter().enumerate() {
            if let Some(pg) = view.deny_actions.iter().find(|pg| globs_overlap(g, pg)) {
                return Err(OverlayError::ApprovalOverlapsDeny {
                    key: format!("require_approval[{i}].actions[{j}]"),
                    glob: g.clone(),
                    parent_glob: pg.clone(),
                });
            }
        }
    }
    let mut rules = parent.rules.clone();
    // Review M1: the engine-wide `human_approval_required` escalates every
    // blocking verdict that is not a hard deny. Raised by the overlay (not by
    // the parent), it would turn the parent's denies into approvable
    // prompts. Keep it tighten-only: the parent's blocking rules become hard
    // denies (`OVERLAY_DENY_TAG`, which the engine never escalates), so only
    // actions the parent permits ask for approval. Parent approval rules keep
    // their tag. The tag replaces the rule's `sop_category` in the child.
    if limits.human_approval_required == Some(true) && parent.limits.human_approval_required != Some(true) {
        for r in rules.iter_mut().filter(|r| {
            matches!(r.severity, RuleSeverity::Blocking | RuleSeverity::Critical)
                && r.sop_category.as_deref() != Some(OVERLAY_APPROVAL_TAG)
        }) {
            r.sop_category = Some(OVERLAY_DENY_TAG.to_owned());
        }
    }
    let parent_ids: std::collections::BTreeSet<String> =
        parent.rules.iter().map(|r| norm_id(&r.id)).collect();
    let mut added: Vec<GovernanceRule> = Vec::new();
    for d in &deny_rules {
        expand(
            &d.id,
            &d.actions,
            d.reason.as_deref().unwrap_or("denied by the project overlay"),
            OVERLAY_DENY_TAG,
            d.principal.as_deref(),
            &mut added,
        );
    }
    for (i, a) in approval_rules.iter().enumerate() {
        let base = a
            .id
            .clone()
            .unwrap_or_else(|| format!("overlay.approval[{i}]"));
        expand(
            &base,
            &a.actions,
            a.reason
                .as_deref()
                .unwrap_or("human approval required by the project overlay"),
            OVERLAY_APPROVAL_TAG,
            a.principal.as_deref(),
            &mut added,
        );
    }
    let mut seen = std::collections::BTreeSet::new();
    for r in &added {
        let n = norm_id(&r.id);
        if parent_ids.contains(&n) || !seen.insert(n) {
            return Err(OverlayError::IdCollision {
                key: "overlay".into(),
                id: r.id.clone(),
            });
        }
    }
    rules.extend(added);

    let parent_hash = parent_rules_hash(&parent.rules, &parent.limits);
    let effective_hash = sha256_domain(
        EFFECTIVE_HASH_DOMAIN,
        &json!({
            "parent_hash": hex_encode(&parent_hash),
            "overlay_hash": hex_encode(&overlay.hash),
            "rules": sorted_rule_values(&rules),
            "limits": limits_value(&limits),
        }),
    );
    Ok(Effective {
        rules,
        limits,
        deny_actions,
        require_approval_actions,
        parent_hash,
        overlay_hash: overlay.hash,
        effective_hash,
        parent_version: parent.version,
    })
}
