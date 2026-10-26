//! BVH-backed [`SpatialBackend`] (WEFT-718 / ADR-056 Phase C).
//!
//! Thin adapter: `Arc<Mutex<BvhStore>>` + optional [`ChainSink`] wired to
//! the kernel [`ChainManager`]. Epoch versioning follows `vector_hnsw`.

use std::sync::{Arc, Mutex};

use clawft_bvh::{
    Aabb, BranchId, BranchMeta, BvhChainKind, BvhError, BvhStore, BvhStoreConfig, ChainSink,
    DiffEntry, Frustum, Leaf, LeafId, Ray, RayHit, Vec3,
};

use crate::spatial_backend::{SpatialBackend, SpatialError, SpatialResult};

#[cfg(feature = "exochain")]
use crate::chain::ChainManager;

// ── Chain sink adapter ───────────────────────────────────────────────────

/// Kernel `ChainSink` implementation: maps BVH mutations to ExoChain events.
///
/// Payloads are JSON (chain path already CBOR-encodes the envelope per
/// ADR-030 when the chain is dual-signed). Dual-sign is applied by
/// `ChainManager` when keys are configured (ADR-028).
pub struct KernelBvhChainSink {
    #[cfg(feature = "exochain")]
    chain: Arc<ChainManager>,
}

impl KernelBvhChainSink {
    /// Construct from a live chain manager.
    #[cfg(feature = "exochain")]
    pub fn new(chain: Arc<ChainManager>) -> Self {
        Self { chain }
    }

    #[cfg(not(feature = "exochain"))]
    pub fn new_noop() -> Self {
        Self {}
    }
}

impl ChainSink for KernelBvhChainSink {
    fn on_event(&self, event: &BvhChainKind) {
        #[cfg(feature = "exochain")]
        {
            let (kind, payload) = match event {
                BvhChainKind::Insert {
                    leaf_id,
                    leaf,
                    branch,
                } => (
                    crate::chain::EVENT_KIND_BVH_INSERT,
                    serde_json::json!({
                        "leaf_id": leaf_id.0,
                        "branch": branch.0,
                        "tag": leaf.tag,
                        "identity": match leaf.identity_kind {
                            clawft_bvh::IdentityKind::Object => "object",
                            clawft_bvh::IdentityKind::Event => "event",
                        },
                        "bound": {
                            "min": [leaf.bound.min.x, leaf.bound.min.y, leaf.bound.min.z],
                            "max": [leaf.bound.max.x, leaf.bound.max.y, leaf.bound.max.z],
                        },
                        // Payload as hex for replay without embedding raw binary in logs.
                        "payload_hex": hex_encode(&leaf.payload),
                    }),
                ),
                BvhChainKind::Remove { leaf_id, branch } => (
                    crate::chain::EVENT_KIND_BVH_REMOVE,
                    serde_json::json!({
                        "leaf_id": leaf_id.0,
                        "branch": branch.0,
                    }),
                ),
                BvhChainKind::Derive {
                    parent,
                    child,
                    meta,
                } => (
                    crate::chain::EVENT_KIND_BVH_DERIVE,
                    serde_json::json!({
                        "parent": parent.0,
                        "child": child.0,
                        "name": meta.name,
                        "priority_tier": meta.priority_tier,
                    }),
                ),
                BvhChainKind::RebalanceSeal {
                    branch,
                    leaf_count,
                    epoch,
                } => (
                    crate::chain::EVENT_KIND_BVH_REBALANCE_SEAL,
                    serde_json::json!({
                        "branch": branch.0,
                        "leaf_count": leaf_count,
                        "epoch": epoch,
                    }),
                ),
            };
            self.chain.append("spatial_bvh", kind, Some(payload));
        }
        #[cfg(not(feature = "exochain"))]
        {
            let _ = event;
        }
    }
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// Decode hex produced by [`hex_encode`] (replay helper).
pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = hex_nibble(bytes[i])?;
        let lo = hex_nibble(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

/// Outcome of [`BvhBackend::restore_from_chain`].
#[cfg(feature = "exochain")]
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ChainRestoreReport {
    /// Chain events applied to the store.
    pub applied: usize,
    /// Chain events that did not decode or did not apply.
    pub skipped: usize,
}

/// Inverse of [`KernelBvhChainSink::on_event`]'s payload encoding.
#[cfg(feature = "exochain")]
fn decode_chain_event(kind: &str, p: &serde_json::Value) -> Option<BvhChainKind> {
    use crate::chain::{
        EVENT_KIND_BVH_DERIVE, EVENT_KIND_BVH_INSERT, EVENT_KIND_BVH_REBALANCE_SEAL, EVENT_KIND_BVH_REMOVE,
    };
    let u = |k: &str| p.get(k).and_then(serde_json::Value::as_u64);
    let vec3 = |v: &serde_json::Value| -> Option<Vec3> {
        let a = v.as_array().filter(|a| a.len() == 3)?;
        let f = |i: usize| a[i].as_f64().map(|x| x as f32);
        Some(Vec3::new(f(0)?, f(1)?, f(2)?))
    };
    match kind {
        EVENT_KIND_BVH_INSERT => {
            let bound = p.get("bound")?;
            let identity = match p.get("identity")?.as_str()? {
                "object" => clawft_bvh::IdentityKind::Object,
                "event" => clawft_bvh::IdentityKind::Event,
                _ => return None,
            };
            let leaf = Leaf::new(
                Aabb::from_min_max(vec3(bound.get("min")?)?, vec3(bound.get("max")?)?),
                identity,
                u32::try_from(u("tag")?).ok()?,
                hex_decode(p.get("payload_hex")?.as_str()?)?,
            );
            Some(BvhChainKind::Insert { leaf_id: LeafId(u("leaf_id")?), leaf, branch: BranchId(u("branch")?) })
        }
        EVENT_KIND_BVH_REMOVE => Some(BvhChainKind::Remove { leaf_id: LeafId(u("leaf_id")?), branch: BranchId(u("branch")?) }),
        EVENT_KIND_BVH_DERIVE => Some(BvhChainKind::Derive {
            parent: BranchId(u("parent")?),
            child: BranchId(u("child")?),
            meta: BranchMeta {
                name: p.get("name")?.as_str()?.to_owned(),
                priority_tier: u8::try_from(u("priority_tier")?).ok()?,
            },
        }),
        EVENT_KIND_BVH_REBALANCE_SEAL => Some(BvhChainKind::RebalanceSeal {
            branch: BranchId(u("branch")?),
            leaf_count: usize::try_from(u("leaf_count")?).ok()?,
            epoch: u("epoch")?,
        }),
        _ => None,
    }
}

// ── Backend ──────────────────────────────────────────────────────────────

/// BVH spatial backend wrapping [`BvhStore`].
pub struct BvhBackend {
    store: Mutex<BvhStore>,
    max_leaves: Option<usize>,
    /// In-process record of every mutation (for `ecc.spatial.events` / replay checks), fed
    /// alongside any ExoChain sink.
    log: Arc<EventLog>,
}

/// Most mutation events kept in memory; older ones are dropped and replay is reported as
/// incomplete rather than wrong.
pub const EVENT_LOG_CAP: usize = 100_000;

/// Bounded in-memory mutation log.
#[derive(Default)]
pub struct EventLog {
    events: Mutex<Vec<clawft_bvh::BvhChainKind>>,
    truncated: std::sync::atomic::AtomicBool,
}

impl ChainSink for EventLog {
    fn on_event(&self, event: &clawft_bvh::BvhChainKind) {
        let mut ev = self.events.lock().expect("event log poisoned");
        if ev.len() >= EVENT_LOG_CAP {
            ev.remove(0);
            self.truncated.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        ev.push(event.clone());
    }
}

/// Fans one event out to the log and the optional external (ExoChain) sink.
struct TeeSink {
    log: Arc<EventLog>,
    inner: Option<Arc<dyn ChainSink>>,
}

impl ChainSink for TeeSink {
    fn on_event(&self, event: &clawft_bvh::BvhChainKind) {
        self.log.on_event(event);
        if let Some(ref s) = self.inner {
            s.on_event(event);
        }
    }
}

/// Result of rebuilding the store from its mutation log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayReport {
    /// Events replayed.
    pub events: usize,
    /// The replica has the same branches and the same leaf ids per branch as the live store.
    pub ok: bool,
    /// The log dropped old events (replay cannot be complete).
    pub truncated: bool,
    /// Branches compared.
    pub branches: usize,
}

impl BvhBackend {
    /// Create from store config (no chain sink yet).
    pub fn new(config: BvhStoreConfig) -> Self {
        let max = if config.max_leaves == 0 {
            None
        } else {
            Some(config.max_leaves)
        };
        let log = Arc::new(EventLog::default());
        let tee: Arc<dyn ChainSink> = Arc::new(TeeSink { log: log.clone(), inner: None });
        Self {
            store: Mutex::new(BvhStore::with_sink(config, tee)),
            max_leaves: max,
            log,
        }
    }

    /// Defaults (1M leaves).
    pub fn with_defaults() -> Self {
        Self::new(BvhStoreConfig::default())
    }

    /// Create with a chain sink attached before first mutation.
    pub fn with_sink(config: BvhStoreConfig, sink: Arc<dyn ChainSink>) -> Self {
        let max = if config.max_leaves == 0 {
            None
        } else {
            Some(config.max_leaves)
        };
        let log = Arc::new(EventLog::default());
        let tee: Arc<dyn ChainSink> = Arc::new(TeeSink { log: log.clone(), inner: Some(sink) });
        Self {
            store: Mutex::new(BvhStore::with_sink(config, tee)),
            max_leaves: max,
            log,
        }
    }

    /// Attach / replace the chain sink after construction (boot order).
    pub fn set_chain_sink(&self, sink: Arc<dyn ChainSink>) {
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        store.set_sink(Arc::new(TeeSink { log: self.log.clone(), inner: Some(sink) }));
    }

    /// Run `f` with the store locked (branch-scoped insert / remove / query for the daemon's
    /// `ecc.spatial.*` RPCs). Mutations still go through the store's chain sink.
    pub fn with_store<R>(&self, f: impl FnOnce(&mut BvhStore) -> R) -> R {
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        f(&mut store)
    }

    /// Mutation events recorded so far, and whether old ones were dropped.
    pub fn events(&self) -> (Vec<clawft_bvh::BvhChainKind>, bool) {
        let ev = self.log.events.lock().expect("event log poisoned").clone();
        (ev, self.log.truncated.load(std::sync::atomic::Ordering::Relaxed))
    }

    /// Rebuild a replica from the mutation log and compare it with the live store (branches
    /// and leaf ids per branch). The live store is not replaced.
    pub fn verify_replay(&self) -> ReplayReport {
        let (events, truncated) = self.events();
        let store = self.store.lock().expect("BvhStore lock poisoned");
        let mut replica = BvhStore::new(store.config().clone());
        let applied = events.iter().all(|e| replica.apply_chain_event(e).is_ok());
        let branches = store.branch_count();
        let ids = |s: &BvhStore, b: u64| -> Option<Vec<u64>> {
            let mut v: Vec<u64> = s.snapshot_leaves(BranchId(b)).ok()?.into_iter().map(|(id, _)| id.0).collect();
            v.sort_unstable();
            Some(v)
        };
        let same = replica.branch_count() == branches && (0..branches as u64).all(|b| ids(&store, b) == ids(&replica, b));
        ReplayReport { events: events.len(), ok: applied && same && !truncated, truncated, branches }
    }

    /// Rebuild the store from `spatial_bvh` events on a loaded chain (restart path; the chain
    /// is the source of truth, ADR-056 §chain audit). Call before the chain sink is attached,
    /// so replayed events are not appended to the chain a second time. Events that do not
    /// decode or do not apply are counted and skipped, never guessed at.
    #[cfg(feature = "exochain")]
    pub fn restore_from_chain(&self, events: &[crate::chain::ChainEvent]) -> ChainRestoreReport {
        let mut report = ChainRestoreReport::default();
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        for e in events.iter().filter(|e| e.source == "spatial_bvh") {
            let Some(kind) = e.payload.as_ref().and_then(|p| decode_chain_event(&e.kind, p)) else {
                report.skipped += 1;
                continue;
            };
            match store.apply_chain_event(&kind) {
                Ok(()) => report.applied += 1,
                Err(_) => report.skipped += 1,
            }
        }
        report
    }

    /// Snapshot leaves on the main branch (restart equality tests).
    pub fn snapshot_main(&self) -> Vec<(LeafId, Leaf)> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store
            .snapshot_leaves(BranchId::MAIN)
            .unwrap_or_default()
    }

    /// Restore leaves on the main branch (restart helper).
    pub fn restore_main(&self, leaves: &[(LeafId, Leaf)]) -> SpatialResult<()> {
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        store
            .restore_leaves(BranchId::MAIN, leaves)
            .map_err(map_bvh_err)
    }

    /// Number of branches.
    pub fn branch_count(&self) -> usize {
        self.store
            .lock()
            .expect("BvhStore lock poisoned")
            .branch_count()
    }

    /// Seal / rebalance witness for the active branch.
    pub fn rebalance_seal(&self) -> SpatialResult<()> {
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        let branch = store.active_branch();
        store.rebalance_seal(branch).map_err(map_bvh_err)
    }
}

fn map_bvh_err(e: BvhError) -> SpatialError {
    match e {
        BvhError::CapacityExceeded { max, current } => {
            SpatialError::CapacityExceeded { max, current }
        }
        BvhError::UnknownBranch(id) => SpatialError::UnknownBranch(id.0),
        BvhError::Other(msg) => SpatialError::Other(msg.to_string()),
    }
}

impl SpatialBackend for BvhBackend {
    fn insert(&self, leaf: Leaf) -> SpatialResult<LeafId> {
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        store.insert(leaf).map_err(map_bvh_err)
    }

    fn remove(&self, id: LeafId) -> bool {
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        store.remove(id)
    }

    fn get(&self, id: LeafId) -> Option<Leaf> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.get(id).cloned()
    }

    fn query_point(&self, p: Vec3) -> Vec<LeafId> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.query_point(p)
    }

    fn query_aabb(&self, bb: Aabb) -> Vec<LeafId> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.query_aabb(bb)
    }

    fn query_sphere(&self, center: Vec3, radius: f32) -> Vec<LeafId> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.query_sphere(center, radius)
    }

    fn query_ray(&self, ray: Ray, max_t: f32) -> Vec<RayHit> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.query_ray(ray, max_t)
    }

    fn query_frustum(&self, frustum: &Frustum) -> Vec<LeafId> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.query_frustum(frustum)
    }

    fn query_knn(&self, p: Vec3, k: usize) -> Vec<LeafId> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.query_knn(p, k)
    }

    fn derive_branch(&self, meta: BranchMeta) -> SpatialResult<BranchId> {
        let mut store = self.store.lock().expect("BvhStore lock poisoned");
        store.derive(meta).map_err(map_bvh_err)
    }

    fn branch_diff(&self, a: BranchId, b: BranchId, region: Aabb) -> Vec<DiffEntry> {
        let store = self.store.lock().expect("BvhStore lock poisoned");
        store.branch_diff(a, b, region).unwrap_or_default()
    }

    fn epoch(&self) -> u64 {
        self.store.lock().expect("BvhStore lock poisoned").epoch()
    }

    fn len(&self) -> usize {
        self.store.lock().expect("BvhStore lock poisoned").len()
    }

    fn backend_name(&self) -> &str {
        "bvh"
    }

    fn max_leaves(&self) -> Option<usize> {
        self.max_leaves
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clawft_bvh::{IdentityKind, RecordingChainSink};

    fn leaf_at(x: f32, tag: u32) -> Leaf {
        Leaf::empty_payload(
            Aabb::from_min_max(
                Vec3::new(x, 0.0, 0.0),
                Vec3::new(x + 0.5, 0.5, 0.5),
            ),
            IdentityKind::Object,
            tag,
        )
    }

    #[test]
    fn insert_query_remove() {
        let b = BvhBackend::with_defaults();
        let id = b.insert(leaf_at(0.0, 1)).unwrap();
        assert_eq!(b.len(), 1);
        assert!(b.query_point(Vec3::new(0.25, 0.25, 0.25)).contains(&id));
        assert!(b.remove(id));
        assert!(b.is_empty());
    }

    #[test]
    fn chain_sink_receives_insert() {
        let sink = Arc::new(RecordingChainSink::new());
        let b = BvhBackend::with_sink(BvhStoreConfig::default(), sink.clone());
        let _ = b.insert(leaf_at(1.0, 9)).unwrap();
        assert_eq!(sink.len(), 1);
    }

    #[test]
    fn snapshot_restore_byte_identical() {
        let b = BvhBackend::with_defaults();
        for i in 0..20 {
            b.insert(leaf_at(i as f32, i)).unwrap();
        }
        let snap = b.snapshot_main();
        let b2 = BvhBackend::with_defaults();
        b2.restore_main(&snap).unwrap();
        assert_eq!(b.snapshot_main(), b2.snapshot_main());
        assert_eq!(b.len(), b2.len());
    }

    #[test]
    fn hex_roundtrip() {
        let raw = vec![0u8, 1, 0xab, 0xff];
        let h = hex_encode(&raw);
        assert_eq!(hex_decode(&h).unwrap(), raw);
    }
}
