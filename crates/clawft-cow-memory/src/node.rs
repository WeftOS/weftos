//! One physical `.rvf` file plus the lineage bookkeeping RVF itself doesn't
//! persist (see crate-level docs in `lib.rs` for why: `RvfStore::query`
//! (crates.io 0.2.0) does not read through the COW boundary, so tombstones
//! and the ingest edit-log have to live at this layer, not inside RVF).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use rvf_runtime::RvfStore;
use rvf_types::DerivationType;

use crate::error::{CowMemoryError, Result};
use crate::types::VectorTags;

static NODE_SEQ: AtomicU64 = AtomicU64::new(1);

/// Process-wide, monotonically increasing identifier for a node (a
/// checkpoint, the base, or an inherited ancestor) in a `BranchableMemory`
/// lineage. Never reused, even after the node it names has been rolled back
/// or promoted away, so a stale `CheckpointId` fails closed
/// (`CheckpointNotFound`) rather than silently targeting the wrong node.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct CheckpointId(pub(crate) u64);

impl CheckpointId {
    /// Allocate a fresh id. Exposed `pub(crate)` (rather than folded into
    /// `MemoryNode` construction) so callers in `branchable_memory.rs` can
    /// derive a node's filename from its id *before* constructing the
    /// `RvfStore` at that path -- `RvfStore::create`/`derive` need the path
    /// up front, so the id has to exist first.
    pub(crate) fn alloc() -> Self {
        Self(NODE_SEQ.fetch_add(1, Ordering::Relaxed))
    }

    /// Peek the next id this process would hand out, without allocating it
    /// -- used to stamp a watermark into the lineage manifest
    /// (`crate::manifest`).
    pub(crate) fn watermark() -> u64 {
        NODE_SEQ.load(Ordering::Relaxed)
    }

    /// Bump the process-wide counter past `min` -- mirrors
    /// `id_gen::bump_watermark`, see its doc comment for why `fetch_max`.
    pub(crate) fn bump_watermark(min: u64) {
        NODE_SEQ.fetch_max(min, Ordering::Relaxed);
    }
}

/// One node in the lineage: an open `RvfStore` handle plus the crate-level
/// tombstone set, ingest edit-log, and optional text payloads for the
/// vectors ingested directly at this node.
pub(crate) struct MemoryNode {
    pub(crate) id: CheckpointId,
    pub(crate) label: Option<String>,
    pub(crate) path: PathBuf,
    pub(crate) created_at_ms: u64,
    pub(crate) store: RvfStore,
    /// Vectors ingested directly at this node (already L2-normalized),
    /// keyed by id. Replayed into `base` on `promote`; diffed on `diff`.
    pub(crate) edit_log: HashMap<u64, Vec<f32>>,
    /// Ids hidden at or below this node, regardless of whether they were
    /// ever physically present here. During chain-walk this masks the same
    /// id surfacing from an older (ancestor/base/inherited) node.
    pub(crate) tombstones: HashSet<u64>,
    /// Optional text payload carried alongside a vector id (agenticow's
    /// `texts` map). Not persisted into the `.rvf` file itself.
    pub(crate) texts: HashMap<u64, String>,
    /// Free-form tags carried alongside a vector id -- see
    /// [`VectorTags`]. Not persisted into the `.rvf` file itself.
    pub(crate) tags: HashMap<u64, VectorTags>,
    /// `true` once this node's store is frozen (or opened read-only), i.e.
    /// it may be the parent of a native `branch()`. `base` starts `false`
    /// and is frozen lazily by [`MemoryNode::branch_from`].
    pub(crate) frozen: bool,
}

impl MemoryNode {
    /// Create a brand-new root store at `path` (used for `BranchableMemory`
    /// bases, including a fork's own fresh base). `id` must be freshly
    /// allocated via `CheckpointId::alloc()` by the caller, who needs it to
    /// name `path` before this constructor can run.
    pub(crate) fn create_root(
        id: CheckpointId,
        path: &Path,
        opts: rvf_runtime::RvfOptions,
        label: Option<String>,
    ) -> Result<Self> {
        let store = RvfStore::create(path, opts)?;
        Ok(Self::wrap(id, store, path.to_path_buf(), label))
    }

    /// Derive a fresh, empty, writable child of `parent` at `path`. O(1):
    /// per `RvfStore::derive`, the child starts with zero vectors of its
    /// own -- it is not a copy of the parent's data, just a new node in the
    /// lineage that chain-walk queries will fall through to the parent for.
    pub(crate) fn derive_from(
        id: CheckpointId,
        parent: &RvfStore,
        path: &Path,
        opts: rvf_runtime::RvfOptions,
        label: Option<String>,
    ) -> Result<Self> {
        let store = parent.derive(path, DerivationType::Clone, Some(opts))?;
        Ok(Self::wrap(id, store, path.to_path_buf(), label))
    }

    /// Reopen this node's file read-only, as an independent handle carrying
    /// forward this node's *current* tombstones/texts (cloned, not shared --
    /// the reopened copy is frozen forever from the new lineage's point of
    /// view; it must not see future tombstones the original goes on to
    /// accumulate). Used to build a fork's `inherited` chain: `open_readonly`
    /// takes no writer lock, so it is safe to call while the original node
    /// is still open read-write in the source `BranchableMemory`.
    pub(crate) fn open_readonly_snapshot(&self) -> Result<Self> {
        let store = RvfStore::open_readonly(&self.path)?;
        Ok(Self {
            id: self.id,
            label: self.label.clone(),
            path: self.path.clone(),
            created_at_ms: self.created_at_ms,
            store,
            // Inherited nodes are read-only and never a `promote` target;
            // their own edit-log is irrelevant to the new lineage (the
            // vectors are still reachable through `store.query`, which is
            // what chain-walk actually uses).
            edit_log: HashMap::new(),
            tombstones: self.tombstones.clone(),
            texts: self.texts.clone(),
            tags: self.tags.clone(),
            frozen: true,
        })
    }

    pub(crate) fn freeze(&mut self) -> Result<()> {
        self.store.freeze()?;
        self.frozen = true;
        Ok(())
    }

    /// Freeze unless already frozen/read-only; upstream requires a branch
    /// parent to be immutable.
    pub(crate) fn ensure_frozen(&mut self) -> Result<()> {
        if !self.frozen {
            self.freeze()?;
        }
        Ok(())
    }

    /// Native copy-on-write child of `parent` (`RvfStore::branch`): the
    /// child reads through to the parent's own vectors and hides ids via
    /// its membership filter. Freezes `parent` first. Read-through is
    /// exactly ONE level (the parent's own slab), so deeper history still
    /// needs the chain walk -- see `crate::chain_walk`.
    ///
    /// Upstream sizes the membership bitmap from the highest vector id in
    /// the parent, so ids must stay dense (the process-wide counter in
    /// `id_gen` is; caller-supplied sparse ids above ~512M make `branch`
    /// fail closed with `MembershipInvalid`). Metadata is copied in full per
    /// child; this crate stores none (`ingest_batch(.., None)`), so the
    /// copy is empty.
    pub(crate) fn branch_from(
        id: CheckpointId,
        parent: &mut MemoryNode,
        path: &Path,
        label: Option<String>,
    ) -> Result<Self> {
        parent.ensure_frozen()?;
        let store = parent.store.branch(path)?;
        Ok(Self::wrap(id, store, path.to_path_buf(), label))
    }

    /// Build the next `working` node under `parent`. Uses native `branch()`
    /// when `parent` is not itself a COW child, and plain `derive` (a
    /// lineage link with no COW state) otherwise.
    ///
    /// Why not always branch: opening or querying a COW child recurses
    /// through every COW ancestor (`open_readonly_with_ancestry`, the
    /// parent arm of `query`), one deep stack frame set per level. A
    /// lineage with a checkpoint per turn branched all the way down
    /// overflowed the stack at ~80 checkpoints. Alternating this way
    /// bounds COW depth at one, which is also all the native read-through
    /// covers (see `crate::chain_walk`).
    pub(crate) fn child_of(
        id: CheckpointId,
        parent: &mut MemoryNode,
        path: &Path,
        opts: rvf_runtime::RvfOptions,
        label: Option<String>,
    ) -> Result<Self> {
        if parent.store.is_cow_child() {
            parent.ensure_frozen()?;
            Self::derive_from(id, &parent.store, path, opts, label)
        } else {
            match Self::branch_from(id, parent, path, label.clone()) {
                // Sparse ids (e.g. digest-derived, as `clawft-versions`
                // uses): upstream's membership bitmap is dense and refuses
                // ids past 64 MiB of bitmap before creating any file. Fall
                // back to a plain derive; the chain walk then supplies the
                // read-through. Dense ordinals are an ADR-128 T2 concern.
                Err(CowMemoryError::Rvf(rvf_types::RvfError::Code(
                    rvf_types::ErrorCode::MembershipInvalid,
                ))) => Self::derive_from(id, &parent.store, path, opts, label),
                other => other,
            }
        }
    }

    /// Reopen this (frozen) node's file writable, releasing the old
    /// handle's writer lock first. Used by `promote`, which mutates `base`
    /// in place; the caller re-freezes before branching again.
    pub(crate) fn reopen_writable(&mut self) -> Result<()> {
        let placeholder = RvfStore::open_readonly(&self.path)?;
        drop(std::mem::replace(&mut self.store, placeholder));
        self.store = RvfStore::open(&self.path)?;
        self.frozen = false;
        Ok(())
    }

    /// Whether `id` is in this node's own slab (not read through from a
    /// parent). O(n); only used on the rare resurrection-guard path.
    pub(crate) fn owns(&self, id: u64) -> bool {
        self.store.iter_vectors().any(|(v, _)| v == id)
    }

    /// Wrap an already-open `RvfStore` (e.g. from `RvfStore::open`) as a
    /// node with fresh, empty bookkeeping. Used by `BranchableMemory::open`.
    pub(crate) fn adopt(
        id: CheckpointId,
        store: RvfStore,
        path: PathBuf,
        label: Option<String>,
    ) -> Self {
        Self::wrap(id, store, path, label)
    }

    /// Best-effort remove this node's backing `.rvf` file from disk. Called
    /// after a node has been dropped (releasing its writer lock) during
    /// `rollback`/`promote` cleanup. Missing-file is not an error -- the
    /// node may never have been fsynced past creation, or cleanup may be
    /// retried after a partial failure.
    pub(crate) fn remove_file_best_effort(path: &Path) {
        match std::fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "clawft-cow-memory: failed to remove discarded lineage file"
                );
            }
        }
    }

    fn wrap(id: CheckpointId, store: RvfStore, path: PathBuf, label: Option<String>) -> Self {
        Self {
            id,
            label,
            path,
            created_at_ms: now_ms(),
            store,
            edit_log: HashMap::new(),
            tombstones: HashSet::new(),
            texts: HashMap::new(),
            tags: HashMap::new(),
            frozen: false,
        }
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Surface `CheckpointNotFound` uniformly for callers that search a slice
/// of nodes for an id and come up empty.
pub(crate) fn not_found(id: CheckpointId) -> CowMemoryError {
    CowMemoryError::CheckpointNotFound(id)
}
