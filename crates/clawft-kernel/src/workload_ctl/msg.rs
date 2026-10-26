//! The `workload.ctl` message set (ADR-099 section 7).
//!
//! Every request is a [`CtlRequest`] signed by the requesting node's Ed25519
//! key ([`SignedCtl`]). It names the requester (the node id derived from the
//! signing key), the target `workload-host`, a random nonce and an expiry.
//! Mutating methods also carry the id of the chained governance decision
//! that authorised them. Responses are signed by the target and bound to
//! the request nonce, so a response cannot be replayed onto another request.
//!
//! Verification order on the target: size, signature, structure, key
//! binding, addressee, time window, authorised controller key, nonce
//! freshness. The nonce is recorded only after everything else passed, so a
//! forged request cannot burn a legitimate nonce.

use std::collections::HashMap;
use std::sync::Mutex;

use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::node_registry::node_id_from_pubkey;
use crate::workload_pkg::codec::{hex_decode_exact, hex_encode};

/// Service name the control plane addresses on every node.
pub const WORKLOAD_HOST_SERVICE: &str = "workload-host";
/// Wire version.
pub const CTL_VERSION: u32 = 1;
/// Domain tag signed before a request payload.
pub const REQUEST_DOMAIN: &[u8] = b"weftos.workload_ctl.request.v1\0";
/// Domain tag signed before a response payload.
pub const RESPONSE_DOMAIN: &[u8] = b"weftos.workload_ctl.response.v1\0";
/// Longest request lifetime a target accepts.
pub const MAX_TTL_MS: u64 = 300_000;
/// Tolerated clock skew for `issued_at_ms`.
pub const MAX_SKEW_MS: u64 = 30_000;
/// Largest signed request payload.
pub const MAX_CTL_BYTES: usize = 256 * 1024;
/// Largest signed response payload: a bulk read (a `project.fetch` chunk,
/// base64 in JSON) is answered in one frame; the mesh and IPC layers carry
/// 16 MiB, so this stays well inside them.
pub const MAX_CTL_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Most nonces remembered at once (fail closed when full).
pub const MAX_NONCES: usize = 8192;
/// Target id a `describe` may use before the caller knows the node id.
pub const ANY_TARGET: &str = "*";

/// Methods of the message set. Anything else under `workload.` is denied.
pub mod method {
    /// Signed facts and the `workload-host` advertisement (read-only).
    pub const DESCRIBE: &str = "workload.describe";
    /// Fetch, verify, admit, load and optionally start a placed workload.
    pub const PLACE: &str = "workload.place";
    /// Like place, without starting and without a placement decision.
    pub const LOAD: &str = "workload.load";
    /// Start a loaded instance.
    pub const START: &str = "workload.start";
    /// Stop a running instance.
    pub const STOP: &str = "workload.stop";
    /// Unload an instance.
    pub const UNLOAD: &str = "workload.unload";
    /// Instance status (read-only).
    pub const STATUS: &str = "workload.status";
    /// Last captured output of an instance (read-only).
    pub const LOGS: &str = "workload.logs";
    /// Every method of the set.
    pub const ALL: &[&str] = &[DESCRIBE, PLACE, LOAD, START, STOP, UNLOAD, STATUS, LOGS];

    /// Dashboard reporter status on the target node (read-only). Not part of
    /// [`ALL`]: it is a node-admin method served by the node's
    /// [`NodeAdmin`](crate::workload_ctl::NodeAdmin) hook, not by an adapter.
    pub const DASHBOARD_STATUS: &str = "dashboard.status";
    /// Rotate the target node's dashboard token (changes state; carries a
    /// decision id and is chained on both nodes).
    pub const DASHBOARD_ROTATE: &str = "dashboard.token.rotate";
    /// Fetch a project's repositories and non-git content from the node
    /// holding its primary installation (ADR-108 P3b). Read-only on the
    /// target; served to a paired peer that holds a fetch grant for the
    /// project (the node's `project-fetch.json`), not only to controllers.
    pub const PROJECT_FETCH: &str = "project.fetch";
    /// Node-admin methods: signed like the rest of the set, but answered by
    /// the node's own admin hook rather than a workload adapter.
    pub const NODE_ADMIN: &[&str] = &[DASHBOARD_STATUS, DASHBOARD_ROTATE, PROJECT_FETCH];

    /// True for a node-admin method.
    pub fn is_node_admin(m: &str) -> bool {
        NODE_ADMIN.contains(&m)
    }

    /// Methods that only take down what is running. Trust policy never
    /// blocks these on the controller: an operator can always stop and
    /// unload what they placed.
    pub fn is_teardown(m: &str) -> bool {
        matches!(m, STOP | UNLOAD)
    }

    /// Methods that change state (they need a decision id).
    pub fn mutates(m: &str) -> bool {
        matches!(m, PLACE | LOAD | START | STOP | UNLOAD | DASHBOARD_ROTATE)
    }
}

/// One control request.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CtlRequest {
    /// [`CTL_VERSION`].
    pub version: u32,
    /// Method (`workload.place`, ...).
    pub method: String,
    /// Requesting node id (derived from the signing key).
    pub requester: String,
    /// Target node id, or [`ANY_TARGET`] for `describe`.
    pub target: String,
    /// 32 lower-case hex characters, fresh per request.
    pub nonce: String,
    /// Issue time, ms since the Unix epoch.
    pub issued_at_ms: u64,
    /// Expiry, ms since the Unix epoch.
    pub expires_at_ms: u64,
    /// Chain hash of the requester's governance decision (mutations).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision_id: Option<String>,
    /// Method-specific body.
    #[serde(default)]
    pub body: Value,
}

/// Signed envelope for a request or response. `payload` is the exact JSON
/// that was signed, so verification never re-serialises.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SignedCtl {
    /// Exact JSON bytes of the request / response.
    pub payload: String,
    /// Hex Ed25519 public key (32 bytes).
    pub public_key: String,
    /// Hex Ed25519 signature (64 bytes) over domain || payload.
    pub signature: String,
}

/// Why a target refused a request. Stable names (chained).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RefusalCode {
    /// Signature, key or structure check failed.
    Signature,
    /// Outside its time window.
    Expired,
    /// Nonce already seen.
    Replay,
    /// Addressed to another node.
    NotForMe,
    /// Signed by a key this node does not accept as a controller.
    Unauthorized,
    /// Not a method of the message set (default deny).
    UnknownMethod,
    /// Malformed body or missing decision id.
    InvalidRequest,
    /// Governance denied the transition on this node.
    Governance,
    /// The admission self-check disagreed with the placement.
    Admission,
    /// The payload could not be fetched.
    Fetch,
    /// The payload failed verification.
    Verify,
    /// The runtime adapter failed.
    Runtime,
    /// No such instance on this node.
    UnknownInstance,
}

impl RefusalCode {
    /// Stable name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Signature => "signature",
            Self::Expired => "expired",
            Self::Replay => "replay",
            Self::NotForMe => "not_for_me",
            Self::Unauthorized => "unauthorized",
            Self::UnknownMethod => "unknown_method",
            Self::InvalidRequest => "invalid_request",
            Self::Governance => "governance",
            Self::Admission => "admission",
            Self::Fetch => "fetch",
            Self::Verify => "verify",
            Self::Runtime => "runtime",
            Self::UnknownInstance => "unknown_instance",
        }
    }
}

/// A refusal: code plus human-readable reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{} refused: {reason}", code.as_str())]
pub struct Refusal {
    /// Code.
    pub code: RefusalCode,
    /// Reason.
    pub reason: String,
}

impl Refusal {
    /// Build a refusal.
    pub fn new(code: RefusalCode, reason: impl Into<String>) -> Self {
        Self {
            code,
            reason: reason.into(),
        }
    }
}

/// What the target did.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CtlOutcome {
    /// Done; method-specific result.
    Ok {
        /// Result.
        result: Value,
    },
    /// Refused.
    Refused {
        /// Why.
        refusal: Refusal,
    },
}

/// One control response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CtlResponse {
    /// [`CTL_VERSION`].
    pub version: u32,
    /// Method answered.
    pub method: String,
    /// Responding node id (derived from its key).
    pub responder: String,
    /// Nonce of the request this answers.
    pub request_nonce: String,
    /// Outcome.
    pub outcome: CtlOutcome,
    /// Byte count of one raw frame that follows this response on the same
    /// connection (a bulk read the caller asked for in binary, ADR-108 P3b).
    /// The frame is not signed; the signed result carries its SHA-256, which
    /// the caller checks. Absent for every ordinary response.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trailing: Option<u64>,
}

pub(crate) fn signed_bytes(domain: &[u8], payload: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(domain.len() + payload.len());
    v.extend_from_slice(domain);
    v.extend_from_slice(payload.as_bytes());
    v
}

pub(crate) fn sign(domain: &[u8], payload: String, key: &SigningKey) -> SignedCtl {
    let sig = key.sign(&signed_bytes(domain, &payload));
    SignedCtl {
        payload,
        public_key: hex_encode(&key.verifying_key().to_bytes()),
        signature: hex_encode(&sig.to_bytes()),
    }
}

/// Check the envelope signature; returns the signer's raw key.
pub(crate) fn open(domain: &[u8], s: &SignedCtl) -> Result<[u8; 32], Refusal> {
    let bad = |m: &str| Refusal::new(RefusalCode::Signature, m);
    let max = if domain == RESPONSE_DOMAIN { MAX_CTL_RESPONSE_BYTES } else { MAX_CTL_BYTES };
    if s.payload.len() > max {
        return Err(bad("payload too large"));
    }
    let pk = hex_decode_exact::<32>(&s.public_key).ok_or_else(|| bad("bad public key"))?;
    let sig = hex_decode_exact::<64>(&s.signature).ok_or_else(|| bad("bad signature encoding"))?;
    let vk = VerifyingKey::from_bytes(&pk).map_err(|_| bad("bad public key"))?;
    vk.verify(
        &signed_bytes(domain, &s.payload),
        &Signature::from_bytes(&sig),
    )
    .map_err(|_| bad("signature does not verify"))?;
    Ok(pk)
}

/// A fresh random nonce (32 hex).
pub fn fresh_nonce() -> String {
    let b: [u8; 16] = rand::random();
    hex_encode(&b)
}

impl CtlRequest {
    /// Build a request from `key`'s node, valid for `ttl_ms` from `now_ms`.
    pub fn new(
        key: &SigningKey,
        method: &str,
        target: &str,
        now_ms: u64,
        ttl_ms: u64,
        decision_id: Option<String>,
        body: Value,
    ) -> Self {
        Self {
            version: CTL_VERSION,
            method: method.to_string(),
            requester: node_id_from_pubkey(&key.verifying_key().to_bytes()),
            target: target.to_string(),
            nonce: fresh_nonce(),
            issued_at_ms: now_ms,
            expires_at_ms: now_ms.saturating_add(ttl_ms),
            decision_id,
            body,
        }
    }

    /// Sign with `key`. The requester must be `key`'s node id.
    pub fn sign(&self, key: &SigningKey) -> Result<SignedCtl, Refusal> {
        let payload = serde_json::to_string(self)
            .map_err(|e| Refusal::new(RefusalCode::InvalidRequest, e.to_string()))?;
        Ok(sign(REQUEST_DOMAIN, payload, key))
    }
}

impl CtlResponse {
    /// Sign with the responder's key.
    pub fn sign(&self, key: &SigningKey) -> SignedCtl {
        let payload = serde_json::to_string(self).unwrap_or_else(|_| "{}".into());
        sign(RESPONSE_DOMAIN, payload, key)
    }
}

/// Replay guard: nonces seen, kept until their request expires.
#[derive(Debug, Default)]
pub struct NonceGuard {
    seen: Mutex<HashMap<String, u64>>,
}

impl NonceGuard {
    /// Empty guard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record `nonce` (valid until `expires_ms`). False if already seen or
    /// the guard is full of unexpired nonces (fail closed).
    pub fn admit(&self, nonce: &str, expires_ms: u64, now_ms: u64) -> bool {
        let Ok(mut seen) = self.seen.lock() else {
            return false;
        };
        if seen.contains_key(nonce) {
            return false;
        }
        if seen.len() >= MAX_NONCES {
            seen.retain(|_, exp| *exp > now_ms);
            if seen.len() >= MAX_NONCES {
                return false;
            }
        }
        seen.insert(nonce.to_string(), expires_ms);
        true
    }
}

/// Who may control this node: a set of controller public keys.
pub trait ControllerPolicy: Send + Sync {
    /// True if a request signed by `public_key` may be served.
    fn allows(&self, public_key: &[u8; 32]) -> bool;

    /// True if `public_key` may call `method`. A controller may call
    /// anything; a policy may admit other keys for named read-only methods
    /// (a paired peer with a fetch grant calls `workload.describe` and
    /// `project.fetch`, ADR-108). The default admits controllers only.
    fn allows_method(&self, public_key: &[u8; 32], method: &str) -> bool {
        let _ = method;
        self.allows(public_key)
    }

    /// True if the controller with node id `node_id` (derived from its key)
    /// may still control this node. A restart asks this of the requester an
    /// instance was journaled with: the policy as it is now, not as it was.
    /// The default refuses, so a policy that cannot answer restores nothing.
    fn allows_node(&self, node_id: &str) -> bool {
        let _ = node_id;
        false
    }
}

impl ControllerPolicy for Vec<[u8; 32]> {
    fn allows(&self, public_key: &[u8; 32]) -> bool {
        self.contains(public_key)
    }

    fn allows_node(&self, node_id: &str) -> bool {
        self.iter().any(|k| crate::node_registry::node_id_from_pubkey(k) == node_id)
    }
}

/// Verify a request addressed to `me` at `now_ms`. See the module docs for
/// the order of checks.
pub fn verify_request(
    s: &SignedCtl,
    me: &str,
    now_ms: u64,
    controllers: &dyn ControllerPolicy,
    nonces: &NonceGuard,
) -> Result<CtlRequest, Refusal> {
    let pk = open(REQUEST_DOMAIN, s)?;
    let req: CtlRequest = serde_json::from_str(&s.payload)
        .map_err(|e| Refusal::new(RefusalCode::Signature, format!("malformed request: {e}")))?;
    if req.version != CTL_VERSION {
        return Err(Refusal::new(
            RefusalCode::InvalidRequest,
            "unsupported version",
        ));
    }
    let derived = node_id_from_pubkey(&pk);
    if req.requester != derived {
        return Err(Refusal::new(
            RefusalCode::Signature,
            format!("requester {} is not the signing key's node", req.requester),
        ));
    }
    let any_ok = req.target == ANY_TARGET && req.method == method::DESCRIBE;
    if req.target != me && !any_ok {
        return Err(Refusal::new(
            RefusalCode::NotForMe,
            format!("addressed to {}", req.target),
        ));
    }
    if req.expires_at_ms <= now_ms {
        return Err(Refusal::new(RefusalCode::Expired, "request expired"));
    }
    if req.issued_at_ms > now_ms.saturating_add(MAX_SKEW_MS) {
        return Err(Refusal::new(
            RefusalCode::Expired,
            "request issued in the future",
        ));
    }
    if req.expires_at_ms.saturating_sub(req.issued_at_ms) > MAX_TTL_MS
        || req.expires_at_ms < req.issued_at_ms
    {
        return Err(Refusal::new(
            RefusalCode::Expired,
            "request lifetime out of bounds",
        ));
    }
    if !crate::workload_pkg::codec::is_lower_hex(&req.nonce, 32) {
        return Err(Refusal::new(
            RefusalCode::InvalidRequest,
            "nonce must be 32 hex",
        ));
    }
    if !controllers.allows_method(&pk, &req.method) {
        return Err(Refusal::new(
            RefusalCode::Unauthorized,
            format!("{derived} is not an authorised controller of this node"),
        ));
    }
    if !nonces.admit(&req.nonce, req.expires_at_ms, now_ms) {
        return Err(Refusal::new(RefusalCode::Replay, "nonce already used"));
    }
    Ok(req)
}

/// Verify a response to `request` from the node whose key is `expected`
/// (`None` accepts any key, then binds `responder` to it: `describe` to an
/// unknown node). Returns the response and the responder's key.
pub fn verify_response(
    s: &SignedCtl,
    request: &CtlRequest,
    expected: Option<&[u8; 32]>,
) -> Result<(CtlResponse, [u8; 32]), Refusal> {
    let pk = open(RESPONSE_DOMAIN, s)?;
    if expected.is_some_and(|e| e != &pk) {
        return Err(Refusal::new(
            RefusalCode::Signature,
            "response signed by another key",
        ));
    }
    let resp: CtlResponse = serde_json::from_str(&s.payload)
        .map_err(|e| Refusal::new(RefusalCode::Signature, format!("malformed response: {e}")))?;
    if resp.responder != node_id_from_pubkey(&pk) {
        return Err(Refusal::new(
            RefusalCode::Signature,
            "responder is not the signing key",
        ));
    }
    if resp.request_nonce != request.nonce || resp.method != request.method {
        return Err(Refusal::new(
            RefusalCode::Replay,
            "response answers another request",
        ));
    }
    if request.target != ANY_TARGET && resp.responder != request.target {
        return Err(Refusal::new(
            RefusalCode::NotForMe,
            "response from another node",
        ));
    }
    Ok((resp, pk))
}

#[cfg(test)]
#[path = "tests_msg.rs"]
mod tests;
