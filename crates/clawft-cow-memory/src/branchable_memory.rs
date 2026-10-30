//! `BranchableMemory` -- the crate's one public entry point. Construction,
//! ingest/delete/query, and introspection (`status`/`lineage`/`diff`) live
//! here; the lineage-mutating operations (`checkpoint`/`rollback`/`promote`/
//! `branch`/`fork`) live in `ops.rs` as a second `impl` block, split out
//! purely to keep any one file under ~500 lines.

use std::path::{Path, PathBuf};

use rvf_runtime::options::DistanceMetric;
use rvf_runtime::{QueryOptions, RvfOptions};

use crate::chain_walk::{self, ScoredId};
use crate::error::{CowMemoryError, Result};
use crate::id_gen::next_vector_id;
use crate::node::{CheckpointId, MemoryNode};
use crate::normalize::l2_normalize;
use crate::types::{LineageEntry, MemoryDiff, MemoryStatus, NodeRole, VectorItem};

/// A branchable, checkpointable, chain-walk-queryable view over an RVF
/// vector lineage -- the Rust port of agenticow's `RvfDatabase` orchestration
/// (`.planning/ruv/integration/agenticow-integration-plan.md`).
///
/// # Layout
///
/// ```text
/// query priority (newest first):
///     working                 <- current writable tip
///     ancestors[0..]          <- this lineage's own checkpoints, newest..oldest
///     base                    <- this lineage's own writable, promotable root
///     inherited[0..]          <- read-only, inherited from a fork/branch source, newest..oldest
/// ```
///
/// `query` walks every node in that order and merges (see [`crate::chain_walk`])
/// because `RvfStore::query` (crates.io 0.2.0) does not read through the COW
/// boundary on any platform the published crate ships for -- verified against
/// `store.rs:314-368`, which scans only `self.vectors.ids()`.
///
/// # Fixed metric discipline
///
/// Every store in a lineage is created with `DistanceMetric::L2` over
/// L2-normalized vectors, never `DistanceMetric::Cosine` directly -- see
/// `crate::normalize` for why. `ingest`/`query` normalize automatically;
/// callers never need to.
///
/// # What is *not* durable across a process restart
///
/// The `.rvf` files themselves are durable (fsynced per write). Most of the
/// crate-level bookkeeping that makes chain-walk correct -- each node's
/// tombstone set, plus which files are `ancestors` vs `inherited` vs `base`
/// -- is now also durable, via a `manifest.json` this crate writes on every
/// topology change and [`BranchableMemory::open`] restores from (see
/// `crate::manifest` for the exact contract, including what still isn't
/// restored: `working`'s uncommitted edits, and every node's `edit_log`/
/// `texts`/`tags`). A lineage that predates this manifest, or was never
/// checkpointed/paused/etc., falls back to the original lossy base-only
/// reopen.
///
/// # Parked lineages
///
/// [`BranchableMemory::pause`] freezes `working` into `ancestors` without
/// deriving a fresh child, leaving `working` as `None` -- see that method's
/// doc comment. `chain_priority_order` and every read (`query`/`diff`/
/// `lineage`/`status`) simply skip a `None` `working`; every write
/// (`ingest`/`delete`/`checkpoint`/`rollback`/`promote`/`branch`) fails
/// closed with [`crate::CowMemoryError::Paused`] until [`BranchableMemory::resume`].
pub struct BranchableMemory {
    pub(crate) dir: PathBuf,
    pub(crate) opts: RvfOptions,
    /// `None` while the lineage is parked (see "Parked lineages" above);
    /// `Some` otherwise, always.
    pub(crate) working: Option<MemoryNode>,
    pub(crate) ancestors: Vec<MemoryNode>,
    pub(crate) base: MemoryNode,
    pub(crate) inherited: Vec<MemoryNode>,
}

impl BranchableMemory {
    /// Create a new lineage at `dir` (created if missing) for vectors of the
    /// given `dimension`. Forces `DistanceMetric::L2` (see crate docs).
    pub fn create(dir: impl AsRef<Path>, dimension: u16) -> Result<Self> {
        let mut opts = RvfOptions {
            dimension,
            ..Default::default()
        };
        opts.metric = DistanceMetric::L2;
        Self::create_with_options(dir, opts)
    }

    /// Create a new lineage with caller-supplied `RvfOptions` (HNSW
    /// parameters, witness config, security policy, ...). `metric` is
    /// always overridden to `L2` regardless of what's passed in -- see
    /// `crate::normalize` for why this is not optional.
    pub fn create_with_options(dir: impl AsRef<Path>, mut opts: RvfOptions) -> Result<Self> {
        if opts.dimension == 0 {
            return Err(CowMemoryError::InvalidDimension);
        }
        opts.metric = DistanceMetric::L2;

        let dir = dir.as_ref().to_path_buf();
        std::fs::create_dir_all(&dir)?;

        let base_id = CheckpointId::alloc();
        let base_path = dir.join(format!("node-{}.base.rvf", base_id.0));
        let mut base = MemoryNode::create_root(base_id, &base_path, opts.clone(), Some("base".into()))?;

        let working_id = CheckpointId::alloc();
        let working_path = dir.join(format!("node-{}.working.rvf", working_id.0));
        let working = MemoryNode::child_of(
            working_id,
            &mut base,
            &working_path,
            opts.clone(),
            Some("working".into()),
        )?;

        let mem = Self {
            dir,
            opts,
            working: Some(working),
            ancestors: Vec::new(),
            base,
            inherited: Vec::new(),
        };
        mem.write_manifest()?;
        Ok(mem)
    }

    /// Reopen a lineage previously created at `dir`.
    ///
    /// If `dir` has a `manifest.json` (written by every topology-changing
    /// operation since this lineage was created -- see `crate::manifest`),
    /// the **full** lineage is restored: `base`, this lineage's own
    /// checkpoint chain, anything it inherited from a fork/branch source,
    /// every node's tombstones, and whether it was left paused. Without a
    /// manifest (a pre-manifest lineage, or one that was never
    /// checkpointed/paused/etc. before this reopen), this falls back to the
    /// original **lossy** behavior: only `base` is reopened, a fresh
    /// `working` is derived from it, and everything else is gone. Neither
    /// path is full crash recovery for an in-flight (not yet checkpointed)
    /// turn -- see `crate::manifest`'s "What is NOT restored" section.
    pub fn open(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        if let Some(restored) = Self::open_with_manifest(&dir)? {
            return Ok(restored);
        }

        let base_path = Self::find_base_file(&dir)?;
        let base_store = rvf_runtime::RvfStore::open(&base_path)?;
        let opts = RvfOptions {
            dimension: base_store.dimension(),
            metric: DistanceMetric::L2,
            ..Default::default()
        };

        let base_id = CheckpointId::alloc();
        let mut base = MemoryNode::adopt(base_id, base_store, base_path, Some("base".into()));

        let working_id = CheckpointId::alloc();
        let working_path = dir.join(format!("node-{}.working.rvf", working_id.0));
        let working = MemoryNode::child_of(
            working_id,
            &mut base,
            &working_path,
            opts.clone(),
            Some("working".into()),
        )?;

        Ok(Self {
            dir,
            opts,
            working: Some(working),
            ancestors: Vec::new(),
            base,
            inherited: Vec::new(),
        })
    }

    fn find_base_file(dir: &Path) -> Result<PathBuf> {
        let mut candidates: Vec<PathBuf> = std::fs::read_dir(dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.ends_with(".base.rvf"))
                    .unwrap_or(false)
            })
            .collect();
        candidates.sort();
        candidates.into_iter().next().ok_or_else(|| {
            CowMemoryError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("no *.base.rvf file found in {}", dir.display()),
            ))
        })
    }

    pub fn dimension(&self) -> u16 {
        self.opts.dimension
    }

    /// Ingest a batch of vectors into `working`. Each vector is
    /// L2-normalized before it reaches `RvfStore` (see `crate::normalize`).
    /// Returns the assigned id for each item, in input order.
    pub fn ingest(&mut self, items: &[VectorItem]) -> Result<Vec<u64>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if self.working.is_none() {
            return Err(CowMemoryError::Paused);
        }

        let dim = self.opts.dimension as usize;
        let mut normalized: Vec<Vec<f32>> = Vec::with_capacity(items.len());
        let mut ids: Vec<u64> = Vec::with_capacity(items.len());
        for item in items {
            if item.vector.len() != dim {
                return Err(CowMemoryError::DimensionMismatch {
                    expected: dim,
                    got: item.vector.len(),
                });
            }
            normalized.push(l2_normalize(&item.vector));
            ids.push(item.id.unwrap_or_else(next_vector_id));
        }

        let working = self.working.as_mut().expect("checked above");
        let refs: Vec<&[f32]> = normalized.iter().map(|v| v.as_slice()).collect();
        working.store.ingest_batch(&refs, &ids, None)?;

        for i in 0..items.len() {
            let id = ids[i];
            working.edit_log.insert(id, normalized[i].clone());
            // Re-ingesting a previously-tombstoned id (within this same
            // node) un-hides it -- the fresh write is what should be seen.
            working.tombstones.remove(&id);
            if let Some(text) = &items[i].text {
                working.texts.insert(id, text.clone());
            }
            if !items[i].tags.is_empty() {
                working.tags.insert(id, items[i].tags.clone());
            }
        }

        Ok(ids)
    }

    /// Hide `ids` from this lineage's view.
    ///
    /// Deliberately does **not** call `RvfStore::delete` on `working` for
    /// locally-present ids. `RvfStore::delete` sets a bit in that store's
    /// `deletion_bitmap` that nothing clears short of `compact()` (verified
    /// against `store.rs`: `ingest_batch` never touches the bitmap, so
    /// re-ingesting under the same id afterward would leave the fresh
    /// vector data present but permanently excluded from `query()` by the
    /// stale bit -- and `compact()` would then *remove* that fresh data too,
    /// since it drops every id still in the bitmap regardless of whether it
    /// was re-ingested since). So this crate's own tombstone set is the
    /// *only* mechanism for hiding an id, at every level of the chain --
    /// including within a single node -- see `crate::chain_walk`, which
    /// masks a node's own results against its own tombstones, not just
    /// older nodes'. This also means `delete` followed by `ingest` under
    /// the same id, within `working`, correctly un-hides it.
    pub fn delete(&mut self, ids: &[u64]) -> Result<()> {
        if ids.is_empty() {
            return Ok(());
        }
        let working = self.working.as_mut().ok_or(CowMemoryError::Paused)?;
        for id in ids {
            working.edit_log.remove(id);
            working.texts.remove(id);
            working.tags.remove(id);
        }
        working.tombstones.extend(ids.iter().copied());
        Ok(())
    }

    /// Chain-walk k-nearest-neighbor query across the whole visible lineage
    /// (`working`, this lineage's own checkpoints, `base`, and anything
    /// inherited from a fork source). See `crate::chain_walk` for why this
    /// is a manual merge rather than a single `RvfStore::query` call.
    pub fn query(&self, vector: &[f32], k: usize) -> Result<Vec<ScoredId>> {
        let dim = self.opts.dimension as usize;
        if vector.len() != dim {
            return Err(CowMemoryError::DimensionMismatch {
                expected: dim,
                got: vector.len(),
            });
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        let normalized = l2_normalize(vector);
        let qopts = QueryOptions::default();
        // Single-parent shape with a native COW child: no walk needed. A
        // working node that fell back to `derive` (sparse ids, see
        // `MemoryNode::child_of`) has no read-through and takes the walk.
        if let (Some(working), true, true, true) = (
            &self.working,
            self.ancestors.is_empty(),
            self.inherited.is_empty(),
            self.working.as_ref().is_some_and(|w| w.store.is_cow_child()),
        ) {
            return chain_walk::query_single_parent(working, &self.base, &normalized, k, &qopts);
        }
        let chain = self.chain_priority_order();
        chain_walk::query_chain(&chain, &normalized, k, &qopts)
    }

    pub(crate) fn chain_priority_order(&self) -> Vec<&MemoryNode> {
        let mut v: Vec<&MemoryNode> =
            Vec::with_capacity(1 + self.ancestors.len() + 1 + self.inherited.len());
        // `None` while paused (see `pause`'s doc comment) -- the frozen
        // former `working` is already the newest entry in `ancestors` at
        // that point, so simply skipping it here still walks every vector.
        if let Some(working) = &self.working {
            v.push(working);
        }
        v.extend(self.ancestors.iter());
        v.push(&self.base);
        v.extend(self.inherited.iter());
        v
    }

    /// What `working` has added/deleted relative to its immediate parent --
    /// "what this turn changed" (plan §3). Does not look at ancestors
    /// checkpointed earlier in this same turn; call `checkpoint`-by-
    /// `checkpoint` if that finer granularity is needed. Returns the empty
    /// diff while paused (see `pause`'s doc comment) -- there is no live
    /// `working` to have pending changes.
    pub fn diff(&self) -> MemoryDiff {
        let Some(working) = &self.working else {
            return MemoryDiff::default();
        };
        let mut added: Vec<u64> = working.edit_log.keys().copied().collect();
        added.sort_unstable();
        let mut deleted: Vec<u64> = working.tombstones.iter().copied().collect();
        deleted.sort_unstable();
        MemoryDiff { added, deleted }
    }

    /// Walk the full provenance chain, newest to oldest, `Working` first
    /// (omitted while paused -- see `pause`'s doc comment) and the
    /// inherited fork source (if any) last.
    pub fn lineage(&self) -> Vec<LineageEntry> {
        let mut out = Vec::with_capacity(1 + self.ancestors.len() + 1 + self.inherited.len());
        if let Some(working) = &self.working {
            out.push(Self::describe(working, NodeRole::Working));
        }
        out.extend(
            self.ancestors
                .iter()
                .map(|n| Self::describe(n, NodeRole::Checkpoint)),
        );
        out.push(Self::describe(&self.base, NodeRole::Base));
        out.extend(
            self.inherited
                .iter()
                .map(|n| Self::describe(n, NodeRole::Inherited)),
        );
        out
    }

    fn describe(node: &MemoryNode, role: NodeRole) -> LineageEntry {
        LineageEntry {
            role,
            id: node.id,
            label: node.label.clone(),
            file_id: *node.store.file_id(),
            parent_id: *node.store.parent_id(),
            lineage_depth: node.store.lineage_depth(),
            created_at_ms: node.created_at_ms,
            mutation_count: node.edit_log.len(),
            tombstone_count: node.tombstones.len(),
        }
    }

    pub fn status(&self) -> MemoryStatus {
        let working_status = match &self.working {
            Some(working) => working.store.status(),
            None => self
                .ancestors
                .first()
                .map(|n| n.store.status())
                .unwrap_or_else(|| self.base.store.status()),
        };
        MemoryStatus {
            dimension: self.opts.dimension,
            own_checkpoint_depth: self.ancestors.len(),
            inherited_depth: self.inherited.len(),
            working: working_status,
            base: self.base.store.status(),
            paused: self.working.is_none(),
        }
    }

    /// `true` if this lineage is parked -- see [`BranchableMemory::pause`].
    pub fn is_paused(&self) -> bool {
        self.working.is_none()
    }
}
