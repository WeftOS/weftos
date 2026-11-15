//! The signed parent policy (ADR-103 A6, D8, Phase 2 package E).
//!
//! The user daemon exports its governance rules and limits as a
//! [`ParentPolicy`], signs it with the user key and writes it to
//! `<run>/<id>/parent-policy.json`. A project kernel reads it at boot (and on
//! `governance.reload` / `governance.parent.update`), checks the signature
//! against the `user_pubkey` of its certificate and merges its overlay onto it
//! ([`crate::governance_overlay::merge`]).
//!
//! Signed bytes: [`PARENT_POLICY_DOMAIN`] + canonical JSON of every field but
//! `sig` (rules sorted by id, the risk threshold as the hex of its bits). The
//! tag is distinct from the certificate, anchor and proof-of-possession tags
//! of package A, so a signature made for one can never verify as another.
//!
//! `version` only moves forward: a kernel pins the highest version it has
//! accepted and refuses an older signed policy ([`ParentPolicyError::Rollback`]),
//! so putting an old, looser, validly signed file back does nothing.

use std::path::Path;

use chrono::{DateTime, SecondsFormat, Utc};
use clawft_types::config::overlay::Limits;
use clawft_types::project::canon::{canonical_json, hex_decode, hex_encode};
use clawft_types::project::cert::key_id;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::governance::{GovernanceEngine, GovernanceRule};
use crate::governance_overlay::{
    OverlayError, limits_value, read_capped, sha256_domain, sorted_rule_values,
};

/// Domain tag prepended to the signed bytes of a [`ParentPolicy`].
pub const PARENT_POLICY_DOMAIN: &str = "weftos-parent-policy-v1\n";
/// Domain tag of [`parent_rules_hash`].
pub const PARENT_RULES_DOMAIN: &str = "weftos-parent-rules-v1\n";
/// How far past the clock a previous version may be and still be believed.
pub const VERSION_SLACK_SECS: u64 = 86_400;
/// Parent policy format version.
pub const PARENT_SCHEMA: u32 = 1;

/// Why a parent policy was refused. Every variant names its key.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ParentPolicyError {
    /// `schema` is not [`PARENT_SCHEMA`].
    #[error("`schema` is {0}, expected {PARENT_SCHEMA}")]
    Schema(u32),
    /// `user_key_id` is not the id of the trusted user key.
    #[error("`user_key_id` does not match the trusted user key")]
    UserKey,
    /// `issued_at` is not `YYYY-MM-DDTHH:MM:SSZ`.
    #[error("`issued_at` is not a canonical UTC timestamp")]
    IssuedAt,
    /// A rule id is empty or has surrounding whitespace.
    #[error("`rules[{0}].id` is malformed")]
    RuleId(usize),
    /// A rule's `principal_selector` is malformed.
    #[error("`rules[{0}].principal_selector` is a malformed principal selector")]
    PrincipalSelector(usize),
    /// A limit is out of range.
    #[error("`limits.{0}` is invalid")]
    Limit(&'static str),
    /// `rule_hash` does not match the rules and limits.
    #[error("`rule_hash` does not match the rules and limits")]
    RuleHash,
    /// `sig` is not 128 lowercase hex characters.
    #[error("`sig` is malformed")]
    SigFormat,
    /// The signature does not verify.
    #[error("`sig` does not verify")]
    BadSignature,
    /// The file could not be read or parsed.
    #[error("{0}")]
    File(String),
    /// A signed policy older than the newest one this kernel accepted.
    #[error("`version` {have} is older than the accepted version {pinned}")]
    Rollback { have: u64, pinned: u64 },
}

impl ParentPolicyError {
    /// The offending key.
    pub fn key(&self) -> String {
        match self {
            Self::Schema(_) => "parent-policy.schema",
            Self::UserKey => "parent-policy.user_key_id",
            Self::IssuedAt => "parent-policy.issued_at",
            Self::RuleId(i) => return format!("parent-policy.rules[{i}].id"),
            Self::PrincipalSelector(i) => {
                return format!("parent-policy.rules[{i}].principal_selector");
            }
            Self::Limit(k) => return format!("parent-policy.limits.{k}"),
            Self::RuleHash => "parent-policy.rule_hash",
            Self::SigFormat | Self::BadSignature => "parent-policy.sig",
            Self::File(_) => "parent-policy.json",
            Self::Rollback { .. } => "parent-policy.version",
        }
        .to_owned()
    }
}

/// `<run>/<id>/parent-policy.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ParentPolicy {
    /// [`PARENT_SCHEMA`].
    pub schema: u32,
    /// `key_id` of the signing user key.
    pub user_key_id: String,
    /// Strictly increasing across exports.
    pub version: u64,
    /// `YYYY-MM-DDTHH:MM:SSZ`.
    pub issued_at: String,
    /// The user's rules, active or not.
    pub rules: Vec<GovernanceRule>,
    /// The user's limits.
    pub limits: Limits,
    /// Hex of [`parent_rules_hash`].
    pub rule_hash: String,
    /// Hex Ed25519 signature by the user key.
    #[serde(default)]
    pub sig: String,
}

/// Hash of a rule set and its limits (the `parent_hash` of an effective
/// merge). Covers `active`, severity and selectors through each rule's full
/// serialized form.
pub fn parent_rules_hash(rules: &[GovernanceRule], limits: &Limits) -> [u8; 32] {
    sha256_domain(
        PARENT_RULES_DOMAIN,
        &json!({"rules": sorted_rule_values(rules), "limits": limits_value(limits)}),
    )
}

fn check_limits(l: &Limits) -> Result<(), ParentPolicyError> {
    if let Some(r) = l.risk_threshold
        && !(r.is_finite() && (0.0..=1.0).contains(&r))
    {
        return Err(ParentPolicyError::Limit("risk_threshold"));
    }
    if l.max_processes == Some(0) {
        return Err(ParentPolicyError::Limit("max_processes"));
    }
    Ok(())
}

impl ParentPolicy {
    fn body(&self) -> Value {
        json!({
            "schema": self.schema,
            "user_key_id": self.user_key_id,
            "version": self.version,
            "issued_at": self.issued_at,
            "rules": sorted_rule_values(&self.rules),
            "limits": limits_value(&self.limits),
            "rule_hash": self.rule_hash,
        })
    }

    /// The bytes the user key signs.
    pub fn signed_bytes(&self) -> Vec<u8> {
        let mut out = PARENT_POLICY_DOMAIN.as_bytes().to_vec();
        out.extend_from_slice(canonical_json(&self.body()).as_bytes());
        out
    }

    /// Parse JSON text.
    pub fn from_json(s: &str) -> Result<Self, ParentPolicyError> {
        serde_json::from_str(s).map_err(|e| ParentPolicyError::File(format!("does not parse: {e}")))
    }
}

/// Check shape, hash and signature against the trusted user key.
pub fn verify_parent_policy(
    p: &ParentPolicy,
    user_pubkey: &[u8; 32],
) -> Result<(), ParentPolicyError> {
    if p.schema != PARENT_SCHEMA {
        return Err(ParentPolicyError::Schema(p.schema));
    }
    if p.user_key_id != key_id(user_pubkey) {
        return Err(ParentPolicyError::UserKey);
    }
    let ts_ok = DateTime::parse_from_rfc3339(&p.issued_at)
        .map(|t| t.with_timezone(&Utc).to_rfc3339_opts(SecondsFormat::Secs, true) == p.issued_at)
        .unwrap_or(false);
    if !ts_ok {
        return Err(ParentPolicyError::IssuedAt);
    }
    for (i, r) in p.rules.iter().enumerate() {
        if r.id.is_empty() || r.id != r.id.trim() {
            return Err(ParentPolicyError::RuleId(i));
        }
        if let Some(sel) = r.principal_selector.as_deref()
            && !clawft_types::config::overlay::valid_principal_selector(sel)
        {
            return Err(ParentPolicyError::PrincipalSelector(i));
        }
    }
    check_limits(&p.limits)?;
    if p.rule_hash != hex_encode(&parent_rules_hash(&p.rules, &p.limits)) {
        return Err(ParentPolicyError::RuleHash);
    }
    let sig: [u8; 64] = hex_decode(&p.sig).ok_or(ParentPolicyError::SigFormat)?;
    VerifyingKey::from_bytes(user_pubkey)
        .map_err(|_| ParentPolicyError::UserKey)?
        .verify_strict(&p.signed_bytes(), &Signature::from_bytes(&sig))
        .map_err(|_| ParentPolicyError::BadSignature)
}

/// Read and parse a parent policy file (size-capped; missing is an error:
/// a project kernel with no parent policy must not start).
pub fn load_parent_policy(path: &Path) -> Result<ParentPolicy, OverlayError> {
    let text = read_capped(path)?.ok_or_else(|| {
        OverlayError::Parent(ParentPolicyError::File(format!(
            "{} does not exist",
            path.display()
        )))
    })?;
    Ok(ParentPolicy::from_json(&text)?)
}

/// User side: snapshot `engine`, tighten with `limits`, sign.
///
/// `limits` carries what the engine does not hold (`max_processes`,
/// `spawn_budget`) and may only lower the engine's own threshold or raise its
/// human-approval flag. Refuses to sign an out-of-range threshold.
pub fn export(
    engine: &GovernanceEngine,
    limits: &Limits,
    signing_key: &SigningKey,
    version: u64,
    issued_at: DateTime<Utc>,
) -> Result<ParentPolicy, ParentPolicyError> {
    export_rules(
        engine.rules().to_vec(),
        engine.risk_threshold(),
        engine.human_approval_required(),
        limits,
        signing_key,
        version,
        issued_at,
    )
}

/// [`export`] from an already-taken snapshot (`rules`, engine threshold and
/// human-approval flag).
pub fn export_rules(
    rules: Vec<GovernanceRule>,
    engine_threshold: f64,
    engine_human_approval: bool,
    limits: &Limits,
    signing_key: &SigningKey,
    version: u64,
    issued_at: DateTime<Utc>,
) -> Result<ParentPolicy, ParentPolicyError> {
    let threshold = limits
        .risk_threshold
        .map_or(engine_threshold, |r| r.min(engine_threshold));
    let limits = Limits {
        risk_threshold: Some(threshold),
        max_processes: limits.max_processes,
        spawn_budget: limits.spawn_budget,
        human_approval_required: Some(
            engine_human_approval || limits.human_approval_required.unwrap_or(false),
        ),
    };
    check_limits(&limits)?;
    let pubkey = signing_key.verifying_key().to_bytes();
    let mut p = ParentPolicy {
        schema: PARENT_SCHEMA,
        user_key_id: key_id(&pubkey),
        version,
        issued_at: issued_at.to_rfc3339_opts(SecondsFormat::Secs, true),
        rule_hash: hex_encode(&parent_rules_hash(&rules, &limits)),
        rules,
        limits,
        sig: String::new(),
    };
    p.sig = hex_encode(&signing_key.sign(&p.signed_bytes()).to_bytes());
    Ok(p)
}

/// The next version after `prev`: past `prev` and past the clock, so a lost
/// counter file cannot send the version backwards behind a kernel's pin.
pub fn next_version(prev: u64, now: DateTime<Utc>) -> u64 {
    prev.saturating_add(1).max(u64::try_from(now.timestamp()).unwrap_or(0))
}

/// Atomic write, mode 0600 on unix: a private temp file in the same
/// directory, fsynced, then renamed over `path`.
pub fn write_atomic_0600(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let result = (|| {
        let mut f = opts.open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// User side: export and atomically write `path` (normally
/// `<run>/<id>/parent-policy.json`). The version continues from whatever
/// valid policy the file already holds.
pub fn export_to(
    path: &Path,
    engine: &GovernanceEngine,
    limits: &Limits,
    signing_key: &SigningKey,
) -> Result<ParentPolicy, ParentPolicyError> {
    export_rules_to(
        path,
        engine.rules().to_vec(),
        engine.risk_threshold(),
        engine.human_approval_required(),
        limits,
        signing_key,
    )
}

/// [`export_to`] from an already-taken snapshot.
pub fn export_rules_to(
    path: &Path,
    rules: Vec<GovernanceRule>,
    engine_threshold: f64,
    engine_human_approval: bool,
    limits: &Limits,
    signing_key: &SigningKey,
) -> Result<ParentPolicy, ParentPolicyError> {
    let now = Utc::now();
    // Only a policy this key signed may set the version, and a version far
    // ahead of the clock is not believed: a bad file cannot ratchet it up.
    let pubkey = signing_key.verifying_key().to_bytes();
    let slack = u64::try_from(now.timestamp()).unwrap_or(0) + VERSION_SLACK_SECS;
    let prev = load_parent_policy(path)
        .ok()
        .filter(|p| verify_parent_policy(p, &pubkey).is_ok())
        .map_or(0, |p| p.version)
        .min(slack);
    let p = export_rules(
        rules,
        engine_threshold,
        engine_human_approval,
        limits,
        signing_key,
        next_version(prev, now),
        now,
    )?;
    let text = serde_json::to_vec_pretty(&p)
        .map_err(|e| ParentPolicyError::File(format!("serialize: {e}")))?;
    write_atomic_0600(path, &text)
        .map_err(|e| ParentPolicyError::File(format!("{}: {e}", path.display())))?;
    Ok(p)
}
