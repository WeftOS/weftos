//! Permission engine — K1 ACL (WEFT-130 scaffold + WEFT-554 Did / delegation).
//!
//! ## Status
//!
//! WEFT-130 shipped the scaffold: typed [`CapabilityChecker`],
//! [`EffectiveAclCache`], tree-walk with default-deny.
//!
//! WEFT-554 upgrades the foundation:
//! - [`Principal`] is DID-backed ([`Did`]) while remaining string-compatible
//! - Delegation cert shortcut via WEFT-131 [`DelegationCert`]
//! - Multi-node chain validation through [`validate_chain`]
//! - Local [`crate::consent`] evaluate (deny-by-policy + risk) until
//!   a standalone `exo_consent` crate lands
//!
//! ## Residual (document on close)
//!
//! - True LRU eviction in [`EffectiveAclCache`] (still bulk-clear)
//! - Wire kernel `CapabilityChecker` to `tree.check()`
//! - Full BailmentPolicy / signed ConsentProof
//! - Daemon-persisted ACL state for `weaver resource {grant,revoke,check}`
//!
//! ## Default policy
//!
//! `CapabilityChecker::new()` builds a checker whose root policy is
//! **deny-all**. Callers explicitly grant subtrees with
//! [`CapabilityChecker::grant`]. Evaluation order (SPARC §5.1):
//!
//! 1. Cache hit
//! 2. Walk resource → root: valid delegation cert for principal → Allow
//! 3. First matching ACL policy (most-specific-first)
//! 4. Consent-layer evaluate (if consent policies configured)
//! 5. Default-deny

use std::collections::HashMap;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};

use crate::consent::{
    self, ConsentDecision, ConsentPolicy, ConsentProof, RiskScore, create_consent_proof,
};
use crate::delegation::{DelegationCert, DelegationError, validate_chain};
use crate::did::Did;
use crate::model::{Action, ResourceId, Role};

// ── Decision (back-compat with K0) ─────────────────────────────────

/// Permission decision returned by the legacy free function
/// [`check`] and the new [`CapabilityChecker::check_permission`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Decision {
    Allow,
    Deny,
    /// Delegation path matched (alias of Allow for audit clarity).
    Delegate,
}

impl Decision {
    /// Whether this decision permits the action.
    pub fn is_allowed(&self) -> bool {
        matches!(self, Decision::Allow | Decision::Delegate)
    }
}

/// Check whether an agent with the given role may perform an action on a resource.
///
/// **K0 stub**: Always returns `Allow`. Preserved so existing K0
/// call sites compile unchanged. New code must use
/// [`CapabilityChecker::check_permission`] instead.
#[deprecated(
    note = "always returns Allow; use PermissionEngine (or the governance gate). Card eb1d6a2d"
)]
pub fn check(_agent_id: &str, _role: &Role, _action: &Action, _resource: &ResourceId) -> Decision {
    Decision::Allow
}

// ── Principal (Did-backed) ─────────────────────────────────────────

/// Identity of an actor making a permission request.
///
/// Backed by [`Did`]. Opaque strings remain valid for migration;
/// prefer `did:weft:…` / `did:key:…` forms.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct Principal(pub Did);

impl Principal {
    /// Construct from any string-like value (opaque or DID URI).
    ///
    /// Empty input becomes the sentinel principal `"anonymous"`.
    pub fn new(id: impl Into<String>) -> Self {
        let s = id.into();
        let did = Did::new(s.as_str()).unwrap_or_else(|_| Did::from_str_unchecked("anonymous"));
        Self(did)
    }

    /// Construct from a validated [`Did`].
    pub fn from_did(did: Did) -> Self {
        Self(did)
    }

    /// Parse a DID URI or opaque id; errors on empty.
    pub fn parse(id: impl AsRef<str>) -> Result<Self, crate::did::DidError> {
        Ok(Self(Did::parse(id)?))
    }

    /// The principal's identifier string.
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }

    /// Borrow the underlying [`Did`].
    pub fn did(&self) -> &Did {
        &self.0
    }

    /// Consume into the underlying [`Did`].
    pub fn into_did(self) -> Did {
        self.0
    }
}

impl From<&str> for Principal {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<Did> for Principal {
    fn from(d: Did) -> Self {
        Self(d)
    }
}

impl From<Principal> for Did {
    fn from(p: Principal) -> Self {
        p.0
    }
}

// ── ACL policy ─────────────────────────────────────────────────────

/// A single ACL entry attached to a resource.
///
/// Policies are evaluated from the leaf up. The first explicit
/// grant or deny that matches the principal/role for the requested
/// action wins. Children inherit unless overridden.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AclPolicy {
    /// Who this policy applies to. `None` matches any principal
    /// (used for role-based grants).
    pub principal: Option<Principal>,
    /// Required role. `None` matches any role.
    pub role: Option<Role>,
    /// Action this policy gates.
    pub action: Action,
    /// Whether to allow or deny.
    pub effect: Effect,
}

/// Effect of an ACL policy match.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Effect {
    Allow,
    Deny,
}

impl AclPolicy {
    /// Convenience constructor for an Allow policy.
    pub fn allow(principal: Principal, action: Action) -> Self {
        Self {
            principal: Some(principal),
            role: None,
            action,
            effect: Effect::Allow,
        }
    }

    /// Convenience constructor for a Deny policy.
    pub fn deny(principal: Principal, action: Action) -> Self {
        Self {
            principal: Some(principal),
            role: None,
            action,
            effect: Effect::Deny,
        }
    }

    /// Whether this policy matches the given (principal, role, action).
    fn matches(&self, principal: &Principal, role: &Role, action: &Action) -> bool {
        if &self.action != action {
            return false;
        }
        if let Some(ref p) = self.principal
            && p != principal
        {
            return false;
        }
        if let Some(ref r) = self.role
            && r != role
        {
            return false;
        }
        true
    }
}

// ── Resource scope helpers ─────────────────────────────────────────

/// Whether `requested` is the same as, or a descendant of, `scope`.
pub fn resource_in_scope(requested: &ResourceId, scope: &ResourceId) -> bool {
    if scope.is_root() {
        return true;
    }
    if requested == scope {
        return true;
    }
    let prefix = if scope.0.ends_with('/') {
        scope.0.clone()
    } else {
        format!("{}/", scope.0)
    };
    requested.0.starts_with(&prefix)
}

// ── EffectiveAclCache ──────────────────────────────────────────────

/// LRU-style cache of evaluated permission decisions.
///
/// Per the K1 spec (§5.2) the cache is keyed by
/// `(principal, resource_id, action)`. The scaffold uses the
/// "clear when full" eviction strategy from the spec (true LRU is
/// residual). Hits and misses are tracked for tuning.
#[derive(Debug)]
pub struct EffectiveAclCache {
    entries: Mutex<HashMap<CacheKey, Decision>>,
    max_size: usize,
    hits: std::sync::atomic::AtomicU64,
    misses: std::sync::atomic::AtomicU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct CacheKey {
    principal: Principal,
    resource: ResourceId,
    action: Action,
}

impl EffectiveAclCache {
    /// Build a new cache with the given capacity.
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            max_size: max_size.max(1),
            hits: std::sync::atomic::AtomicU64::new(0),
            misses: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Look up a cached decision.
    pub fn get(
        &self,
        principal: &Principal,
        resource: &ResourceId,
        action: &Action,
    ) -> Option<Decision> {
        let key = CacheKey {
            principal: principal.clone(),
            resource: resource.clone(),
            action: action.clone(),
        };
        let entries = self.entries.lock().ok()?;
        if let Some(d) = entries.get(&key) {
            self.hits.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Some(d.clone())
        } else {
            self.misses
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            None
        }
    }

    /// Insert a decision. If the cache is full, the entire table is
    /// cleared (per spec §5.2 K1 scaffold). True LRU is residual.
    pub fn put(
        &self,
        principal: Principal,
        resource: ResourceId,
        action: Action,
        decision: Decision,
    ) {
        let Ok(mut entries) = self.entries.lock() else {
            return;
        };
        if entries.len() >= self.max_size {
            entries.clear();
        }
        entries.insert(
            CacheKey {
                principal,
                resource,
                action,
            },
            decision,
        );
    }

    /// Drop every entry whose resource matches `resource`.
    pub fn invalidate_resource(&self, resource: &ResourceId) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|k, _| &k.resource != resource);
        }
    }

    /// Drop every entry for `principal`.
    pub fn invalidate_principal(&self, principal: &Principal) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.retain(|k, _| &k.principal != principal);
        }
    }

    /// Drop every entry.
    pub fn clear(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            entries.clear();
        }
    }

    /// Cumulative hit count since construction.
    pub fn hits(&self) -> u64 {
        self.hits.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Cumulative miss count since construction.
    pub fn misses(&self) -> u64 {
        self.misses.load(std::sync::atomic::Ordering::Relaxed)
    }
}

impl Default for EffectiveAclCache {
    fn default() -> Self {
        Self::new(1024)
    }
}

// ── CapabilityChecker ──────────────────────────────────────────────

/// Tree-walking permission checker (WEFT-130 + WEFT-554).
///
/// Stores per-resource ACL policies and delegation certificates.
/// Walks from the requested resource up to root:
///
/// 1. **Delegation shortcut** — active, non-expired, non-revoked cert
///    whose grantee matches the principal and whose resource scope
///    covers the request → [`Decision::Delegate`] (allowed).
/// 2. **ACL** — first matching policy (most-specific-first).
/// 3. **Consent** — if consent policies are configured, evaluate with
///    risk score; Deny-by-policy and high-risk deny.
/// 4. **Default-deny**.
#[derive(Debug)]
pub struct CapabilityChecker {
    /// Per-resource ACLs. Missing entry = no policy at that level.
    policies: Mutex<HashMap<ResourceId, Vec<AclPolicy>>>,
    /// Per-resource active delegation certificates (WEFT-131).
    delegations: Mutex<HashMap<ResourceId, Vec<DelegationCert>>>,
    /// Consent-layer policies (WEFT-554 foundation / exo_consent stand-in).
    consent_policies: Mutex<Vec<ConsentPolicy>>,
    /// Principal DID string → risk score (0–100).
    risk_scores: Mutex<HashMap<String, RiskScore>>,
    /// Risk ≥ this → Deny via consent path.
    risk_deny_threshold: u8,
    /// Risk ≥ this (and < deny) → Defer (surfaced as Deny at ACL boundary
    /// unless `map_defer_to_allow` — currently maps to Deny for fail-closed).
    risk_defer_threshold: u8,
    /// When true, fall through to consent evaluate after ACL miss.
    consent_enabled: bool,
    /// Decision cache for repeated lookups.
    cache: EffectiveAclCache,
}

impl CapabilityChecker {
    /// Build a checker whose default policy is **deny-all** at root.
    pub fn new() -> Self {
        Self {
            policies: Mutex::new(HashMap::new()),
            delegations: Mutex::new(HashMap::new()),
            consent_policies: Mutex::new(Vec::new()),
            risk_scores: Mutex::new(HashMap::new()),
            risk_deny_threshold: 90,
            risk_defer_threshold: 70,
            // Off by default so scaffold ACL tests keep default-deny
            // without requiring consent allows. Enable for consent path.
            consent_enabled: false,
            cache: EffectiveAclCache::new(1024),
        }
    }

    /// Enable consent-layer evaluation after ACL miss.
    pub fn with_consent(mut self, enabled: bool) -> Self {
        self.consent_enabled = enabled;
        self
    }

    /// Override risk thresholds (deny, defer).
    pub fn with_risk_thresholds(mut self, deny: u8, defer: u8) -> Self {
        self.risk_deny_threshold = deny.min(100);
        self.risk_defer_threshold = defer.min(self.risk_deny_threshold);
        self
    }

    /// Direct access to the underlying decision cache.
    pub fn cache(&self) -> &EffectiveAclCache {
        &self.cache
    }

    /// Attach an ACL policy to a resource.
    pub fn add_policy(&self, resource: ResourceId, policy: AclPolicy) {
        if let Ok(mut policies) = self.policies.lock() {
            policies.entry(resource.clone()).or_default().push(policy);
        }
        self.cache.invalidate_resource(&resource);
    }

    /// Convenience: grant `action` on `resource` to `principal`.
    pub fn grant(&self, resource: ResourceId, principal: Principal, action: Action) {
        self.add_policy(resource, AclPolicy::allow(principal, action));
    }

    /// Convenience: deny `action` on `resource` for `principal`.
    pub fn deny(&self, resource: ResourceId, principal: Principal, action: Action) {
        self.add_policy(resource, AclPolicy::deny(principal, action));
    }

    /// Register a delegation certificate on a resource node.
    ///
    /// Does not re-verify signature here (caller may validate first).
    /// Invalidates the grantee's cache entries.
    pub fn add_delegation(&self, resource: ResourceId, cert: DelegationCert) {
        let grantee = Principal::new(&cert.grantee);
        if let Ok(mut map) = self.delegations.lock() {
            map.entry(resource.clone()).or_default().push(cert);
        }
        self.cache.invalidate_principal(&grantee);
        self.cache.invalidate_resource(&resource);
    }

    /// Revoke all delegations for `grantee` on `resource`.
    /// Returns the number of certs removed.
    pub fn revoke_delegation(&self, resource: &ResourceId, grantee: &Principal) -> usize {
        let mut removed = 0;
        if let Ok(mut map) = self.delegations.lock()
            && let Some(certs) = map.get_mut(resource) {
                let before = certs.len();
                certs.retain(|c| c.grantee != grantee.as_str());
                removed = before - certs.len();
            }
        if removed > 0 {
            self.cache.invalidate_principal(grantee);
            self.cache.invalidate_resource(resource);
        }
        removed
    }

    /// Mark matching certs revoked by grantee (status bit) across all resources.
    pub fn revoke_delegation_everywhere(&self, grantee: &Principal) -> usize {
        let mut removed = 0;
        if let Ok(mut map) = self.delegations.lock() {
            for certs in map.values_mut() {
                for c in certs.iter_mut() {
                    if c.grantee == grantee.as_str() && !c.revoked {
                        c.revoke();
                        removed += 1;
                    }
                }
            }
        }
        if removed > 0 {
            self.cache.invalidate_principal(grantee);
            self.cache.clear();
        }
        removed
    }

    /// Prune expired / revoked certs. Returns count removed.
    pub fn prune_delegations(&self, now: DateTime<Utc>) -> usize {
        let mut pruned = 0;
        if let Ok(mut map) = self.delegations.lock() {
            for certs in map.values_mut() {
                let before = certs.len();
                certs.retain(|c| {
                    if c.revoked {
                        return false;
                    }
                    if let Some(exp) = c.expires_at
                        && now > exp {
                            return false;
                        }
                    true
                });
                pruned += before - certs.len();
            }
        }
        if pruned > 0 {
            self.cache.clear();
        }
        pruned
    }

    /// Add a consent-layer policy.
    pub fn add_consent_policy(&self, policy: ConsentPolicy) {
        if let Ok(mut policies) = self.consent_policies.lock() {
            policies.push(policy);
        }
        self.cache.clear();
    }

    /// Set risk score for a principal DID.
    pub fn set_risk_score(&self, principal: &Principal, risk: RiskScore) {
        if let Ok(mut scores) = self.risk_scores.lock() {
            scores.insert(principal.as_str().to_string(), risk);
        }
        self.cache.invalidate_principal(principal);
    }

    /// Grant with consent proof emission (returns the proof for audit).
    pub fn grant_with_consent(
        &self,
        resource: ResourceId,
        grantor: &Did,
        grantee: Principal,
        action: Action,
        purpose: impl Into<String>,
    ) -> ConsentProof {
        let proof = create_consent_proof(grantor, grantee.did(), purpose);
        self.grant(resource, grantee, action);
        proof
    }

    /// Evaluate whether `principal` (with `role`) may perform
    /// `action` on `resource`.
    pub fn check_permission(
        &self,
        principal: &Principal,
        role: &Role,
        action: &Action,
        resource: &ResourceId,
    ) -> Decision {
        if let Some(d) = self.cache.get(principal, resource, action) {
            return d;
        }

        let decision = self.evaluate_uncached(principal, role, action, resource, Utc::now());
        self.cache.put(
            principal.clone(),
            resource.clone(),
            action.clone(),
            decision.clone(),
        );
        decision
    }

    /// Same as [`Self::check_permission`] but with an explicit clock for tests.
    pub fn check_permission_at(
        &self,
        principal: &Principal,
        role: &Role,
        action: &Action,
        resource: &ResourceId,
        now: DateTime<Utc>,
    ) -> Decision {
        // Bypass cache for time-sensitive checks so expiry is live.
        self.evaluate_uncached(principal, role, action, resource, now)
    }

    /// End-to-end Did-based check using a multi-node delegation chain.
    ///
    /// 1. [`validate_chain`] against `trust_anchor`
    /// 2. Leaf grantee must match `principal`
    /// 3. Leaf resource scope must cover `resource`
    /// 4. On success → [`Decision::Delegate`]; else fall through to ACL.
    // Public API with positional inputs mirroring `check_permission_at` plus chain/anchor; a params struct would break callers.
    #[allow(clippy::too_many_arguments)]
    pub fn check_with_delegation_chain(
        &self,
        principal: &Principal,
        role: &Role,
        action: &Action,
        resource: &ResourceId,
        chain: &[DelegationCert],
        trust_anchor: &VerifyingKey,
        now: DateTime<Utc>,
    ) -> Result<Decision, DelegationError> {
        validate_chain(chain, trust_anchor, now)?;
        let leaf = chain.last().ok_or(DelegationError::EmptyChain)?;
        if leaf.grantee != principal.as_str() {
            // Chain valid but not for this principal — fall through ACL.
            return Ok(self.check_permission_at(principal, role, action, resource, now));
        }
        if !resource_in_scope(resource, &leaf.resource) {
            return Ok(self.check_permission_at(principal, role, action, resource, now));
        }
        Ok(Decision::Delegate)
    }

    fn evaluate_uncached(
        &self,
        principal: &Principal,
        role: &Role,
        action: &Action,
        resource: &ResourceId,
        now: DateTime<Utc>,
    ) -> Decision {
        // ── 1. Delegation shortcut (walk resource → root) ──────────
        // Certs may be indexed under the resource they were attached to
        // and/or declare a scope in `cert.resource`. A cert matches when
        // grantee equals principal, the cert validates, and the declared
        // scope covers the requested resource.
        if let Ok(delegations) = self.delegations.lock() {
            let mut current = Some(resource.clone());
            while let Some(rid) = current {
                if let Some(certs) = delegations.get(&rid) {
                    for cert in certs {
                        if cert.grantee != principal.as_str() {
                            continue;
                        }
                        if !resource_in_scope(resource, &cert.resource) {
                            continue;
                        }
                        if cert.validate(now).is_ok() {
                            return Decision::Delegate;
                        }
                    }
                }
                current = rid.parent();
            }
        }

        // ── 2. ACL first-match walk ────────────────────────────────
        if let Ok(policies) = self.policies.lock() {
            let mut current = Some(resource.clone());
            while let Some(rid) = current {
                if let Some(node_policies) = policies.get(&rid) {
                    for policy in node_policies {
                        if policy.matches(principal, role, action) {
                            return match policy.effect {
                                Effect::Allow => Decision::Allow,
                                Effect::Deny => Decision::Deny,
                            };
                        }
                    }
                }
                current = rid.parent();
            }
        }

        // ── 3. Consent layer (optional) ────────────────────────────
        if self.consent_enabled {
            let risk = self
                .risk_scores
                .lock()
                .ok()
                .and_then(|m| m.get(principal.as_str()).copied())
                .unwrap_or(RiskScore::NEUTRAL);
            let policies = self
                .consent_policies
                .lock()
                .map(|p| p.clone())
                .unwrap_or_default();
            match consent::evaluate(
                &policies,
                principal.did(),
                action,
                &risk,
                self.risk_deny_threshold,
                self.risk_defer_threshold,
            ) {
                ConsentDecision::Allow => return Decision::Allow,
                // Fail-closed: Defer surfaces as Deny at the ACL boundary
                // until cognitum-gate wiring lands.
                ConsentDecision::Deny { .. } | ConsentDecision::Defer { .. } => {
                    return Decision::Deny;
                }
            }
        }

        // ── 4. Default-deny ────────────────────────────────────────
        Decision::Deny
    }
}

impl Default for CapabilityChecker {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;
    use ed25519_dalek::SigningKey;

    fn key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    #[test]
    #[allow(deprecated)]
    fn k0_stub_always_allows() {
        let decision = check(
            "agent-001",
            &Role::Viewer,
            &Action::Admin,
            &ResourceId::new("/kernel"),
        );
        assert_eq!(decision, Decision::Allow);
    }

    #[test]
    fn decision_serde_roundtrip() {
        for decision in [Decision::Allow, Decision::Deny, Decision::Delegate] {
            let json = serde_json::to_string(&decision).unwrap();
            let back: Decision = serde_json::from_str(&json).unwrap();
            assert_eq!(back, decision);
        }
    }

    #[test]
    fn principal_is_did_backed() {
        let p = Principal::from_did(Did::weft("alice").unwrap());
        assert_eq!(p.as_str(), "did:weft:alice");
        assert_eq!(p.did().method(), Some("weft"));
        // Opaque still works (WEFT-130 compat).
        let opaque = Principal::new("bob");
        assert_eq!(opaque.as_str(), "bob");
    }

    #[test]
    fn principal_serde_is_string() {
        let p = Principal::from_did(Did::weft("alice").unwrap());
        let json = serde_json::to_string(&p).unwrap();
        assert_eq!(json, "\"did:weft:alice\"");
        let back: Principal = serde_json::from_str(&json).unwrap();
        assert_eq!(back, p);
    }

    // ── CapabilityChecker scaffold tests (WEFT-130) ───────────────

    #[test]
    fn root_deny_default() {
        let checker = CapabilityChecker::new();
        let decision = checker.check_permission(
            &Principal::new("alice"),
            &Role::Viewer,
            &Action::Read,
            &ResourceId::new("/apps/secret"),
        );
        assert_eq!(decision, Decision::Deny);
    }

    #[test]
    fn principal_grant_allows() {
        let checker = CapabilityChecker::new();
        checker.grant(
            ResourceId::new("/apps/secret"),
            Principal::new("alice"),
            Action::Read,
        );

        let allow = checker.check_permission(
            &Principal::new("alice"),
            &Role::Viewer,
            &Action::Read,
            &ResourceId::new("/apps/secret"),
        );
        assert_eq!(allow, Decision::Allow);

        let deny = checker.check_permission(
            &Principal::new("bob"),
            &Role::Viewer,
            &Action::Read,
            &ResourceId::new("/apps/secret"),
        );
        assert_eq!(deny, Decision::Deny);

        let deny_write = checker.check_permission(
            &Principal::new("alice"),
            &Role::Viewer,
            &Action::Write,
            &ResourceId::new("/apps/secret"),
        );
        assert_eq!(deny_write, Decision::Deny);
    }

    #[test]
    fn parent_inherit_grants() {
        let checker = CapabilityChecker::new();
        checker.grant(
            ResourceId::new("/apps"),
            Principal::new("alice"),
            Action::Read,
        );

        let inherited = checker.check_permission(
            &Principal::new("alice"),
            &Role::Viewer,
            &Action::Read,
            &ResourceId::new("/apps/secret"),
        );
        assert_eq!(inherited, Decision::Allow);
    }

    #[test]
    fn explicit_deny_overrides_inherit() {
        let checker = CapabilityChecker::new();
        checker.grant(
            ResourceId::new("/apps"),
            Principal::new("alice"),
            Action::Read,
        );
        checker.deny(
            ResourceId::new("/apps/secret"),
            Principal::new("alice"),
            Action::Read,
        );

        let decision = checker.check_permission(
            &Principal::new("alice"),
            &Role::Viewer,
            &Action::Read,
            &ResourceId::new("/apps/secret"),
        );
        assert_eq!(decision, Decision::Deny);

        let sibling = checker.check_permission(
            &Principal::new("alice"),
            &Role::Viewer,
            &Action::Read,
            &ResourceId::new("/apps/public"),
        );
        assert_eq!(sibling, Decision::Allow);
    }

    #[test]
    fn cache_records_hits_and_misses() {
        let checker = CapabilityChecker::new();
        checker.grant(
            ResourceId::new("/svc"),
            Principal::new("alice"),
            Action::Execute,
        );

        let p = Principal::new("alice");
        let r = ResourceId::new("/svc");
        let a = Action::Execute;

        assert_eq!(
            checker.check_permission(&p, &Role::Viewer, &a, &r),
            Decision::Allow
        );
        assert_eq!(
            checker.check_permission(&p, &Role::Viewer, &a, &r),
            Decision::Allow
        );

        assert!(
            checker.cache().hits() >= 1,
            "expected at least one cache hit, got {}",
            checker.cache().hits()
        );
        assert!(
            checker.cache().misses() >= 1,
            "expected at least one cache miss, got {}",
            checker.cache().misses()
        );
    }

    #[test]
    fn add_policy_invalidates_cache() {
        let checker = CapabilityChecker::new();
        let p = Principal::new("alice");
        let r = ResourceId::new("/svc");

        assert_eq!(
            checker.check_permission(&p, &Role::Viewer, &Action::Read, &r),
            Decision::Deny
        );

        checker.grant(r.clone(), p.clone(), Action::Read);

        assert_eq!(
            checker.check_permission(&p, &Role::Viewer, &Action::Read, &r),
            Decision::Allow
        );
    }

    // ── WEFT-554: Did + delegation + consent ───────────────────────

    #[test]
    fn did_based_grant() {
        let checker = CapabilityChecker::new();
        let alice = Principal::from_did(Did::weft("alice").unwrap());
        checker.grant(
            ResourceId::new("/apps/secret"),
            alice.clone(),
            Action::Read,
        );
        assert_eq!(
            checker.check_permission(&alice, &Role::Operator, &Action::Read, &ResourceId::new("/apps/secret")),
            Decision::Allow
        );
    }

    #[test]
    fn delegation_shortcut_allows_grantee() {
        let checker = CapabilityChecker::new();
        let root = key(1);
        let admin = Did::weft("admin").unwrap();
        let worker = Principal::from_did(Did::weft("worker").unwrap());
        let scope = ResourceId::new("/apps/secret");

        let cert = DelegationCert::grant(
            admin.as_str(),
            worker.as_str(),
            scope.clone(),
            &root,
            Some(Utc::now() + Duration::hours(1)),
            None,
        );
        checker.add_delegation(scope.clone(), cert);

        // No ACL grant — delegation alone permits.
        assert_eq!(
            checker.check_permission(&worker, &Role::Viewer, &Action::Read, &scope),
            Decision::Delegate
        );
        // Unrelated principal still denied.
        assert_eq!(
            checker.check_permission(
                &Principal::from_did(Did::weft("intruder").unwrap()),
                &Role::Viewer,
                &Action::Read,
                &scope
            ),
            Decision::Deny
        );
    }

    #[test]
    fn revoked_delegation_denies() {
        let checker = CapabilityChecker::new();
        let root = key(2);
        let worker = Principal::from_did(Did::weft("worker").unwrap());
        let scope = ResourceId::new("/x");
        let mut cert = DelegationCert::grant(
            "admin",
            worker.as_str(),
            scope.clone(),
            &root,
            None,
            None,
        );
        cert.revoke();
        checker.add_delegation(scope.clone(), cert);
        assert_eq!(
            checker.check_permission(&worker, &Role::Viewer, &Action::Read, &scope),
            Decision::Deny
        );
    }

    #[test]
    fn expired_delegation_denies() {
        let checker = CapabilityChecker::new();
        let root = key(3);
        let worker = Principal::from_did(Did::weft("worker").unwrap());
        let scope = ResourceId::new("/x");
        let cert = DelegationCert::grant(
            "admin",
            worker.as_str(),
            scope.clone(),
            &root,
            Some(Utc::now() - Duration::seconds(5)),
            None,
        );
        checker.add_delegation(scope.clone(), cert);
        assert_eq!(
            checker.check_permission_at(
                &worker,
                &Role::Viewer,
                &Action::Read,
                &scope,
                Utc::now()
            ),
            Decision::Deny
        );
    }

    #[test]
    fn multi_node_delegation_chain_e2e() {
        let checker = CapabilityChecker::new();
        let root_key = key(4);
        let mid_key = key(5);
        let leaf_key = key(6);

        let root = DelegationCert::grant(
            "did:weft:root",
            "did:weft:mid",
            ResourceId::new("/apps"),
            &root_key,
            Some(Utc::now() + Duration::hours(2)),
            None,
        );
        let mid = DelegationCert::grant(
            "did:weft:mid",
            "did:weft:leaf-agent",
            ResourceId::new("/apps/secret"),
            &mid_key,
            Some(Utc::now() + Duration::hours(1)),
            Some(&root),
        );
        let leaf = DelegationCert::grant(
            "did:weft:leaf-agent",
            "did:weft:worker",
            ResourceId::new("/apps/secret"),
            &leaf_key,
            Some(Utc::now() + Duration::minutes(30)),
            Some(&mid),
        );

        let chain = vec![root, mid, leaf];
        let worker = Principal::from_did(Did::weft("worker").unwrap());
        let decision = checker
            .check_with_delegation_chain(
                &worker,
                &Role::Operator,
                &Action::Read,
                &ResourceId::new("/apps/secret/file"),
                &chain,
                &root_key.verifying_key(),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(decision, Decision::Delegate);
        assert!(decision.is_allowed());

        // Wrong principal on a valid chain falls through → default deny.
        let stranger = Principal::from_did(Did::weft("stranger").unwrap());
        let deny = checker
            .check_with_delegation_chain(
                &stranger,
                &Role::Operator,
                &Action::Read,
                &ResourceId::new("/apps/secret/file"),
                &chain,
                &root_key.verifying_key(),
                Utc::now(),
            )
            .unwrap();
        assert_eq!(deny, Decision::Deny);
    }

    #[test]
    fn consent_deny_by_policy() {
        let checker = CapabilityChecker::new().with_consent(true);
        let alice = Principal::from_did(Did::weft("alice").unwrap());
        checker.add_consent_policy(ConsentPolicy::deny(
            alice.did().clone(),
            Action::Admin,
            "admin locked by policy",
        ));
        // Also add a consent allow for Read so we can distinguish paths.
        checker.add_consent_policy(ConsentPolicy::allow(alice.did().clone(), Action::Read));

        assert_eq!(
            checker.check_permission(
                &alice,
                &Role::Admin,
                &Action::Admin,
                &ResourceId::new("/kernel")
            ),
            Decision::Deny
        );
        assert_eq!(
            checker.check_permission(
                &alice,
                &Role::Viewer,
                &Action::Read,
                &ResourceId::new("/kernel")
            ),
            Decision::Allow
        );
    }

    #[test]
    fn consent_high_risk_denies_even_with_allow_policy() {
        let checker = CapabilityChecker::new()
            .with_consent(true)
            .with_risk_thresholds(80, 60);
        let eve = Principal::from_did(Did::weft("eve").unwrap());
        checker.add_consent_policy(ConsentPolicy::allow(eve.did().clone(), Action::Write));
        checker.set_risk_score(&eve, RiskScore::new(95, 9900));
        assert_eq!(
            checker.check_permission(
                &eve,
                &Role::Operator,
                &Action::Write,
                &ResourceId::new("/apps")
            ),
            Decision::Deny
        );
    }

    #[test]
    fn grant_with_consent_emits_proof() {
        let checker = CapabilityChecker::new();
        let grantor = Did::weft("admin").unwrap();
        let grantee = Principal::from_did(Did::weft("agent").unwrap());
        let proof = checker.grant_with_consent(
            ResourceId::new("/apps"),
            &grantor,
            grantee.clone(),
            Action::Execute,
            "delegation:operator",
        );
        assert_eq!(proof.grantor, "did:weft:admin");
        assert_eq!(proof.grantee, "did:weft:agent");
        assert_eq!(
            checker.check_permission(
                &grantee,
                &Role::Operator,
                &Action::Execute,
                &ResourceId::new("/apps")
            ),
            Decision::Allow
        );
    }

    #[test]
    fn revoke_delegation_invalidates() {
        let checker = CapabilityChecker::new();
        let root = key(7);
        let worker = Principal::from_did(Did::weft("worker").unwrap());
        let scope = ResourceId::new("/apps");
        let cert = DelegationCert::grant(
            "admin",
            worker.as_str(),
            scope.clone(),
            &root,
            None,
            None,
        );
        checker.add_delegation(scope.clone(), cert);
        assert!(
            checker
                .check_permission(&worker, &Role::Viewer, &Action::Read, &scope)
                .is_allowed()
        );
        assert_eq!(checker.revoke_delegation(&scope, &worker), 1);
        assert_eq!(
            checker.check_permission(&worker, &Role::Viewer, &Action::Read, &scope),
            Decision::Deny
        );
    }

    #[test]
    fn resource_in_scope_prefix() {
        assert!(resource_in_scope(
            &ResourceId::new("/apps/secret/file"),
            &ResourceId::new("/apps")
        ));
        assert!(resource_in_scope(
            &ResourceId::new("/apps"),
            &ResourceId::new("/apps")
        ));
        assert!(!resource_in_scope(
            &ResourceId::new("/other"),
            &ResourceId::new("/apps")
        ));
        assert!(resource_in_scope(
            &ResourceId::new("/apps/x"),
            &ResourceId::root()
        ));
    }

    /// Warm-cache path must be fast. Release builds assert &lt;100 ns;
    /// debug allows a looser bound so CI without `--release` still gates.
    #[test]
    fn warm_cache_latency() {
        let checker = CapabilityChecker::new();
        let p = Principal::from_did(Did::weft("alice").unwrap());
        let r = ResourceId::new("/svc/hot");
        let a = Action::Read;
        checker.grant(r.clone(), p.clone(), a.clone());

        // Populate + warm.
        for _ in 0..10_000 {
            let _ = checker.check_permission(&p, &Role::Viewer, &a, &r);
        }
        assert!(checker.cache().hits() > 0);

        let n = 200_000u32;
        let start = std::time::Instant::now();
        for _ in 0..n {
            let d = checker.check_permission(&p, &Role::Viewer, &a, &r);
            // Prevent optimizer from eliding the loop.
            std::hint::black_box(d);
        }
        let elapsed = start.elapsed();
        let avg_ns = elapsed.as_nanos() / u128::from(n);

        #[cfg(not(debug_assertions))]
        {
            assert!(
                avg_ns < 100,
                "warm-cache avg {avg_ns} ns >= 100 ns (release)"
            );
        }
        #[cfg(debug_assertions)]
        {
            // Debug builds are much slower; gate at 5 µs to catch regressions.
            assert!(
                avg_ns < 5_000,
                "warm-cache avg {avg_ns} ns >= 5000 ns (debug)"
            );
        }
    }
}
