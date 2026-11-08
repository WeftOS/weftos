//! Fleet identity binding for Cognitum Seeds (card mesh-placement-22; ADR-100
//! section 5; `docs/research/mesh-placement/fleet-compat.md` section 2c).
//!
//! A Seed has a device identity (`GET /api/v1/identity`: a device id and an
//! Ed25519 public key) but no WeftOS node key. The operator binds the two
//! with a signed record `{device_id, device_pubkey, node_id, bound_at}`;
//! [`SeedBinder::bind`] verifies it against the pinned operator keys and
//! against the Seed itself, then chains `workload.node.bind`. Anything
//! else (an unpinned signer, a changed record, a key that is not the
//! device's, a record for another node, an expired or replayed record) is
//! refused with a chained `workload.refuse`.
//!
//! The node id of a Seed is the id derived from a per-Seed *adapter key*
//! held by WeftOS (never by the Seed): the Seed cannot sign for itself, so
//! the adapter signs the Seed's facts ([`attest_seed_facts`]) and every
//! capability carries provenance `claimed`, since the device's own report
//! is not something WeftOS measured.
//!
//! Bearer tokens never reach a record, an error or a chain event: the only
//! Seed data used is the (public) identity.
//!
//! ADR-106 phase 1d adds the licence form: [`SeedBinder::bind_v2`] verifies an
//! operator-signed `licence::BindingRecord` v2 under the steward profile (the
//! checks above, plus the grant key fingerprint, `mesh_id` and `seq`), and
//! [`SeedBinder::unbind_v2`] withdraws it. Both live in `seed_bind/v2.rs`.

use std::collections::{BTreeMap, HashSet};
use std::sync::{Arc, Mutex};

use clawft_types::placement::{Capability, NodeFacts, ProbeNote, Provenance};
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::seed::{API_TIMEOUT, SeedApiRuntime};
use super::seed_http::Method;
use super::types::{RuntimeError, WorkloadRuntime};
use crate::chain::{ChainManager, EVENT_KIND_WORKLOAD_NODE_BIND, EVENT_KIND_WORKLOAD_REFUSE};
use crate::node_facts_advert::{NodeFactsAdvertError, SignedNodeFacts, sign_node_facts};
use crate::node_registry::node_id_from_pubkey;
use crate::workload_pkg::codec::hex_encode;
use crate::workload_pkg::manifest::valid_token;

/// Chain source of bind events.
pub const BIND_CHAIN_SOURCE: &str = "workload.bind";
/// Domain tag signed before a bind record.
pub const BIND_DOMAIN: &[u8] = b"weftos.workload.node.bind.v1\0";
/// Default age after which a bind record is no longer accepted.
pub const DEFAULT_MAX_BIND_AGE_SECS: u64 = 600;
/// File name of the persisted bind state, beside `workload-placements.json`.
/// The daemon's `workload.node.bind` uses `dir.join(BIND_STATE_FILE)` (the
/// runtime dir).
pub const BIND_STATE_FILE: &str = "workload-seed-binds.json";
const MAX_STATE_BYTES: u64 = 256 * 1024;
const MAX_BOUND_DEVICES: usize = 1000;
/// Tolerated clock skew for `bound_at`.
pub const BIND_CLOCK_SKEW_SECS: u64 = 60;
/// Facts lifetime for an adapter-attested Seed.
pub const SEED_FACTS_TTL_SECS: u64 = 300;
const MAX_RECORD_BYTES: usize = 4096;

/// What the operator signs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BindRecord {
    /// The Seed's device id, as `GET /api/v1/identity` reports it.
    pub device_id: String,
    /// The Seed's device public key, exactly as `GET /api/v1/identity`
    /// reports it.
    pub device_pubkey: String,
    /// The WeftOS node id the Seed is bound to (derived from the adapter
    /// key, [`seed_node_id`]).
    pub node_id: String,
    /// Unix seconds the operator made the binding.
    pub bound_at: u64,
}

/// A bind record with the operator's signature over the exact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SignedBind {
    /// Exact JSON bytes of the [`BindRecord`] that were signed.
    pub record: String,
    /// Operator Ed25519 public key (32 bytes).
    pub operator_key: Vec<u8>,
    /// Ed25519 signature (64 bytes) over [`BIND_DOMAIN`] || record.
    pub signature: Vec<u8>,
}

/// Node id of a Seed whose adapter key is `adapter`.
pub fn seed_node_id(adapter: &SigningKey) -> String {
    node_id_from_pubkey(&adapter.verifying_key().to_bytes())
}

fn signed_bytes(record: &str) -> Vec<u8> {
    let mut v = BIND_DOMAIN.to_vec();
    v.extend_from_slice(record.as_bytes());
    v
}

/// Sign `record` as the operator.
pub fn sign_bind(record: &BindRecord, operator: &SigningKey) -> SignedBind {
    let record = serde_json::to_string(record).unwrap_or_default();
    let signature = operator.sign(&signed_bytes(&record)).to_bytes().to_vec();
    SignedBind {
        record,
        operator_key: operator.verifying_key().to_bytes().to_vec(),
        signature,
    }
}

/// Why a bind was refused. [`Self::code`] is the stable chain code.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindError {
    /// Not a well-formed record or envelope.
    #[error("malformed bind record: {0}")]
    Malformed(String),
    /// Signed by a key that is not a pinned operator key.
    #[error("bind record is not signed by a pinned operator key")]
    UntrustedOperator,
    /// The signature does not verify: the record was altered.
    #[error("bind record signature does not verify")]
    BadSignature,
    /// The record names another node than this adapter's.
    #[error("bind record is for node {record}, this adapter is node {adapter}")]
    WrongNode {
        /// Node id in the record.
        record: String,
        /// This adapter's node id.
        adapter: String,
    },
    /// The Seed's own identity differs from the record.
    #[error("the Seed's identity does not match the bind record ({0})")]
    IdentityMismatch(&'static str),
    /// `bound_at` is in the future or older than the accepted age.
    #[error("bind record is outside its validity window")]
    Expired,
    /// This exact record was already accepted, or is older than the
    /// binding held for the device.
    #[error("bind record replayed or older than the current binding")]
    Replayed,
    /// The node is already bound to a different device.
    #[error("node {0} is already bound to another device")]
    NodeTaken(String),
    /// The link to the Seed is not pinned (and no lab opt-in is set).
    #[error("{0}")]
    UnpinnedTransport(String),
    /// A licence-record check failed (v2 binding: signature, mesh id,
    /// steward profile, admission posture). Only with the licence module.
    #[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
    #[error("{0}")]
    Licence(crate::licence::LicenceError),
    /// The device is already bound to another mesh here; unbind first.
    #[error("device {device_id} is already bound to another mesh (seed_bound_elsewhere); unbind it first")]
    SeedBoundElsewhere {
        /// The Seed's device id.
        device_id: String,
    },
    /// The bind state could not be saved, so nothing was bound.
    #[error("bind state not saved: {0}")]
    State(String),
    /// The Seed could not be asked.
    #[error("could not read the Seed identity: {0}")]
    Seed(String),
}

impl BindError {
    /// Stable code recorded in the chained refusal.
    pub fn code(&self) -> &'static str {
        match self {
            Self::Malformed(_) => "malformed",
            Self::UntrustedOperator => "untrusted_operator",
            Self::BadSignature => "bad_signature",
            Self::WrongNode { .. } => "wrong_node",
            Self::IdentityMismatch(_) => "identity_mismatch",
            Self::Expired => "expired",
            Self::Replayed => "replayed",
            Self::NodeTaken(_) => "node_taken",
            Self::UnpinnedTransport(_) => "unpinned_transport",
            #[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
            Self::Licence(e) => v2::licence_code(e),
            Self::SeedBoundElsewhere { .. } => "seed_bound_elsewhere",
            Self::State(_) => "state_unwritable",
            Self::Seed(_) => "seed_unreachable",
        }
    }
}

/// A verified binding. Only [`SeedBinder::bind`] makes one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Binding {
    record: BindRecord,
}

impl Binding {
    /// The bound record.
    pub fn record(&self) -> &BindRecord {
        &self.record
    }
}

/// Verifies and remembers bindings.
pub struct SeedBinder {
    operators: Vec<[u8; 32]>,
    chain: Arc<ChainManager>,
    max_age_secs: u64,
    seen: Mutex<HashSet<[u8; 32]>>,
    /// Device id to (bound_at, node id).
    bound: Mutex<BTreeMap<String, (u64, String)>>,
    state_file: Option<std::path::PathBuf>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindState {
    version: u32,
    /// Device id to its latest accepted `bound_at` and node id.
    bound: BTreeMap<String, BoundEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundEntry {
    bound_at: u64,
    node_id: String,
}

fn write_state(
    path: &std::path::Path,
    bound: &BTreeMap<String, (u64, String)>,
) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let st = BindState {
        version: 1,
        bound: bound
            .iter()
            .map(|(d, (at, n))| {
                (
                    d.clone(),
                    BoundEntry {
                        bound_at: *at,
                        node_id: n.clone(),
                    },
                )
            })
            .collect(),
    };
    let bytes = serde_json::to_vec_pretty(&st).map_err(|e| e.to_string())?;
    let tmp = path.with_extension("json.tmp");
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .map_err(|e| e.to_string())?;
    f.write_all(&bytes).map_err(|e| e.to_string())?;
    f.sync_all().map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

impl SeedBinder {
    /// A binder trusting `operators` (pinned operator public keys).
    pub fn new(operators: Vec<[u8; 32]>, chain: Arc<ChainManager>) -> Self {
        Self {
            operators,
            chain,
            max_age_secs: DEFAULT_MAX_BIND_AGE_SECS,
            seen: Mutex::new(HashSet::new()),
            bound: Mutex::new(BTreeMap::new()),
            state_file: None,
        }
    }

    /// Persist each device's latest `bound_at` to `path` (mode 0600,
    /// atomic) and restore it now, so a replayed or older record is still
    /// refused after a restart. A missing file starts empty; a malformed,
    /// oversized or non-regular one is refused (nothing is overwritten).
    pub fn with_state_file(
        mut self,
        path: impl Into<std::path::PathBuf>,
    ) -> Result<Self, BindError> {
        let path = path.into();
        let bad = |m: String| BindError::State(format!("{}: {m}", path.display()));
        if let Ok(meta) = std::fs::symlink_metadata(&path) {
            if !meta.file_type().is_file() || meta.len() > MAX_STATE_BYTES {
                return Err(bad("not a regular file of at most 256 KiB".into()));
            }
            let text = std::fs::read_to_string(&path).map_err(|e| bad(e.to_string()))?;
            let st: BindState = serde_json::from_str(&text).map_err(|e| bad(e.to_string()))?;
            if st.version != 1 || st.bound.len() > MAX_BOUND_DEVICES {
                return Err(bad("unsupported version or too many devices".into()));
            }
            if let Ok(mut b) = self.bound.lock() {
                for (d, e) in st.bound {
                    b.insert(d, (e.bound_at, e.node_id));
                }
            }
        }
        self.state_file = Some(path);
        Ok(self)
    }

    /// Accept records for at most `secs` after `bound_at`.
    pub fn with_max_age_secs(mut self, secs: u64) -> Self {
        self.max_age_secs = secs;
        self
    }

    /// The device id bound to `node_id`, if any.
    pub fn device_of(&self, node_id: &str) -> Option<String> {
        self.bound
            .lock()
            .ok()?
            .iter()
            .find(|(_, (_, n))| n == node_id)
            .map(|(d, _)| d.clone())
    }

    fn refuse(&self, signed: &SignedBind, e: &BindError) {
        self.refuse_record(&signed.record, e);
    }

    /// Chain a refusal for the record text `record` (v1 or v2 payload).
    fn refuse_record(&self, record: &str, e: &BindError) {
        let record_hash = hex_encode(&Sha256::digest(record.as_bytes()));
        // Only identifiers, clipped: never the record or any token.
        let claimed = serde_json::from_str::<Value>(record).unwrap_or(Value::Null);
        let clip = |k: &str| {
            claimed
                .get(k)
                .and_then(Value::as_str)
                .map(|s| s.chars().take(64).collect::<String>())
        };
        self.chain.append(
            BIND_CHAIN_SOURCE,
            EVENT_KIND_WORKLOAD_REFUSE,
            Some(json!({
                "phase": "node.bind", "code": e.code(), "reason": e.to_string(),
                "record_hash": record_hash,
                "claimed_node_id": clip("node_id"), "claimed_device_id": clip("device_id"),
            })),
        );
    }

    /// Verify `signed` for `rt` at `now` (unix seconds) and chain the
    /// outcome (`workload.node.bind`, or `workload.refuse` on any failure).
    pub async fn bind(
        &self,
        signed: &SignedBind,
        rt: &SeedApiRuntime,
        now: u64,
    ) -> Result<Binding, BindError> {
        match self.verify(signed, rt, now).await {
            Ok(b) => Ok(b),
            Err(e) => {
                self.refuse(signed, &e);
                Err(e)
            }
        }
    }

    async fn verify(
        &self,
        signed: &SignedBind,
        rt: &SeedApiRuntime,
        now: u64,
    ) -> Result<Binding, BindError> {
        let bad = |m: &str| BindError::Malformed(m.into());
        // Before anything is read from the Seed: an identity read over an
        // unauthenticated link proves nothing.
        rt.link_security()
            .require_pinned(rt.node_id())
            .map_err(BindError::UnpinnedTransport)?;
        if signed.record.len() > MAX_RECORD_BYTES {
            return Err(bad("record too large"));
        }
        let key: [u8; 32] = signed
            .operator_key
            .as_slice()
            .try_into()
            .map_err(|_| bad("operator key is not 32 bytes"))?;
        let sig: [u8; 64] = signed
            .signature
            .as_slice()
            .try_into()
            .map_err(|_| bad("signature is not 64 bytes"))?;
        if !self.operators.contains(&key) {
            return Err(BindError::UntrustedOperator);
        }
        let vk = VerifyingKey::from_bytes(&key).map_err(|_| bad("bad operator key"))?;
        vk.verify_strict(&signed_bytes(&signed.record), &Signature::from_bytes(&sig))
            .map_err(|_| BindError::BadSignature)?;
        let record: BindRecord = serde_json::from_str(&signed.record)
            .map_err(|e| BindError::Malformed(e.to_string()))?;
        if !valid_token(&record.device_id, 128)
            || !valid_token(&record.node_id, 64)
            || record.device_pubkey.is_empty()
            || record.device_pubkey.len() > 256
        {
            return Err(bad("device id, device key or node id is not well formed"));
        }
        let adapter = rt.node_id().to_string();
        if record.node_id != adapter {
            return Err(BindError::WrongNode {
                record: record.node_id,
                adapter,
            });
        }
        if record.bound_at > now.saturating_add(BIND_CLOCK_SKEW_SECS)
            || now.saturating_sub(record.bound_at) > self.max_age_secs
        {
            return Err(BindError::Expired);
        }
        // The Seed is asked last, so an untrusted record costs it nothing.
        let (device_id, device_key) = rt
            .identity()
            .await
            .map_err(|e| BindError::Seed(e.to_string()))?;
        if device_id != record.device_id {
            return Err(BindError::IdentityMismatch("device id"));
        }
        if device_key != record.device_pubkey {
            return Err(BindError::IdentityMismatch("device key"));
        }
        let digest: [u8; 32] =
            Sha256::digest([signed.record.as_bytes(), signed.signature.as_slice()].concat()).into();
        let mut seen = self.seen.lock().map_err(|_| bad("binder poisoned"))?;
        let mut bound = self.bound.lock().map_err(|_| bad("binder poisoned"))?;
        if seen.contains(&digest) {
            return Err(BindError::Replayed);
        }
        if bound
            .get(&record.device_id)
            .is_some_and(|(at, _)| record.bound_at <= *at)
        {
            return Err(BindError::Replayed);
        }
        if bound
            .iter()
            .any(|(d, (_, n))| *n == record.node_id && *d != record.device_id)
        {
            return Err(BindError::NodeTaken(record.node_id));
        }
        if !bound.contains_key(&record.device_id) && bound.len() >= MAX_BOUND_DEVICES {
            return Err(BindError::State("too many bound devices".into()));
        }
        // Saved before it is accepted: if the replay memory cannot be
        // persisted, nothing is bound.
        let mut next = bound.clone();
        next.insert(
            record.device_id.clone(),
            (record.bound_at, record.node_id.clone()),
        );
        if let Some(path) = &self.state_file {
            write_state(path, &next).map_err(BindError::State)?;
        }
        *bound = next;
        seen.insert(digest);
        self.chain.append(
            BIND_CHAIN_SOURCE,
            EVENT_KIND_WORKLOAD_NODE_BIND,
            Some(json!({
                "device_id": record.device_id, "device_pubkey": record.device_pubkey,
                "node_id": record.node_id, "bound_at": record.bound_at,
                "operator_key": hex_encode(&key),
                "record_hash": hex_encode(&Sha256::digest(signed.record.as_bytes())),
            })),
        );
        Ok(Binding { record })
    }
}

// The v2 (licence) form needs the licence module, which is gated like the
// mesh swarm code; without it only the v1 bind exists.
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
mod v2;
#[cfg(all(feature = "mesh", feature = "ecc", feature = "exochain", feature = "native"))]
pub use v2::{StewardBind, UnbindOutcome, grant_fingerprint};

impl SeedApiRuntime {
    /// This adapter's operator-assigned node id.
    pub fn node_id(&self) -> &str {
        &self.cfg.node_id
    }

    /// The Seed's own `(device_id, public_key)` from `GET /api/v1/identity`.
    pub async fn identity(&self) -> Result<(String, String), RuntimeError> {
        let v = self
            .api(Method::Get, "/api/v1/identity", None, API_TIMEOUT)
            .await?;
        let field = |k: &str| {
            v.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && s.len() <= 256)
                .map(str::to_string)
                .ok_or_else(|| RuntimeError::Backend(format!("seed /api/v1/identity: no {k}")))
        };
        Ok((field("device_id")?, field("public_key")?))
    }
}

/// Facts for a bound Seed, signed by the adapter key. Every capability is
/// forced to provenance `claimed`: the Seed's own report, relayed by the
/// adapter, is never `probed` or `measured` (a `measured` value only comes
/// from WeftOS's own conformance runs).
pub fn attest_seed_facts(
    binding: &Binding,
    rt: &SeedApiRuntime,
    adapter: &SigningKey,
    now: u64,
    seq: u64,
) -> Result<SignedNodeFacts, NodeFactsAdvertError> {
    let node_id = seed_node_id(adapter);
    if node_id != binding.record.node_id {
        return Err(NodeFactsAdvertError::NodeMismatch {
            claimed: binding.record.node_id.clone(),
            derived: node_id,
        });
    }
    let mut facts = NodeFacts::new(node_id, now, SEED_FACTS_TTL_SECS, seq);
    facts.capabilities = rt
        .provides()
        .into_iter()
        .map(|c: Capability| Capability {
            provenance: Provenance::Claimed,
            ..c
        })
        .collect();
    facts.notes.push(ProbeNote::new(
        "remote.api",
        format!(
            "claimed by the adapter for Seed device {} (bound by an operator record), not probed",
            binding.record.device_id
        ),
    ));
    sign_node_facts(&facts, adapter)
}
