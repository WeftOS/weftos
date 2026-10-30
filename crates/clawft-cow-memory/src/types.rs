//! Public value types for `BranchableMemory`'s ingest/introspection surface
//! -- split out of `branchable_memory.rs` purely to keep that file (and
//! `ops.rs`) under ~500 lines; these are plain data, not part of either
//! `impl BranchableMemory` block.

use crate::node::CheckpointId;

/// Free-form provenance tags carried alongside a vector, the same way
/// [`VectorItem::text`] is: an in-process side-channel, not persisted into
/// the `.rvf` file itself (see `BranchableMemory`'s "What is *not* durable"
/// section). This crate does not interpret the keys -- callers pick their
/// own vocabulary (e.g. the turn-checkpoint bracket uses
/// `role`/`turn`/`channel`/`chat_id`/`ts_ms`).
pub type VectorTags = std::collections::HashMap<String, String>;

/// A vector to ingest. `id`, left `None`, is auto-assigned from the
/// process-wide monotonic counter (`crate::id_gen`) so it cannot collide
/// with an id assigned in a sibling fork of the same lineage.
pub struct VectorItem {
    pub id: Option<u64>,
    pub vector: Vec<f32>,
    /// Optional text payload carried alongside the vector (agenticow's
    /// `texts` map). Not persisted into the `.rvf` file; survives only as
    /// long as this process's `BranchableMemory` does.
    pub text: Option<String>,
    /// Free-form tags -- see [`VectorTags`]. Empty by default.
    pub tags: VectorTags,
}

impl VectorItem {
    /// Item with just a vector: no id (auto-assigned), no text, no tags.
    pub fn new(vector: Vec<f32>) -> Self {
        Self {
            id: None,
            vector,
            text: None,
            tags: VectorTags::new(),
        }
    }

    /// Pins an explicit id instead of letting ingest auto-assign one.
    pub fn with_id(mut self, id: u64) -> Self {
        self.id = Some(id);
        self
    }

    /// Attaches an in-process text payload to the item.
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = Some(text.into());
        self
    }

    /// Replaces the item's provenance tags.
    pub fn with_tags(mut self, tags: VectorTags) -> Self {
        self.tags = tags;
        self
    }
}

/// What `promote` moved into `base`, plus the lineage identifiers a chain
/// witness needs to record the merge (WEFT-616 Phase 3, `TurnLedger::on_promote`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PromoteReport {
    pub ingested: usize,
    pub deleted: usize,
    /// `file_id` of the `working` node this promote collapsed into `base`,
    /// captured before the collapse removes its backing `.rvf` file -- it
    /// remains a valid provenance pointer for a witness even though the
    /// file itself is gone by the time this returns.
    pub child_id: [u8; 16],
    /// `file_id` of `base` after this promote's replay. Stable across the
    /// call in practice: `base` is mutated in place (ingest/delete), never
    /// re-derived, and `RvfStore::file_id` does not change on mutation.
    pub parent_id: [u8; 16],
    /// `base`'s witness-chain hash at promote time (`RvfStore::last_witness_hash`).
    ///
    /// POPULATED in practice: `RvfOptions`' default `WitnessConfig` has
    /// `witness_ingest`/`witness_delete` = `true`, so the `ingest_batch`/
    /// `delete` calls `promote`'s replay makes append witness entries and
    /// advance this hash (verified by `promote_report_parent_hash_probe`).
    /// Narrow residuals, all upstream `rvf-runtime` 0.2 semantics:
    /// - a promote that replays NOTHING leaves the previous value ([0; 32]
    ///   only if `base` has never witnessed anything);
    /// - `RvfStore::open()` re-initializes the in-memory hash to zeros — a
    ///   reopened lineage's witness chain restarts at its next append
    ///   rather than resuming (upstream wart, noted on WEFT-662);
    /// - `compact()` resets it by design (file rewritten).
    pub parent_hash: [u8; 32],
}

/// What changed in `working` relative to its immediate parent -- i.e. what
/// this turn has done so far (plan §3: "what changed this turn").
#[derive(Clone, Debug, Default)]
pub struct MemoryDiff {
    pub added: Vec<u64>,
    pub deleted: Vec<u64>,
}

/// A node's position in the lineage, as reported by [`crate::BranchableMemory::lineage`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeRole {
    /// The current writable tip.
    Working,
    /// A frozen checkpoint between `working` and `base`, newest to oldest.
    Checkpoint,
    /// This lineage's own writable, promotable root.
    Base,
    /// A read-only node inherited from the source lineage at fork time
    /// (empty unless this `BranchableMemory` was produced by `branch`/`fork`).
    Inherited,
}

/// One entry in [`crate::BranchableMemory::lineage`]'s provenance walk.
#[derive(Clone, Debug)]
pub struct LineageEntry {
    pub role: NodeRole,
    pub id: CheckpointId,
    pub label: Option<String>,
    pub file_id: [u8; 16],
    pub parent_id: [u8; 16],
    pub lineage_depth: u32,
    pub created_at_ms: u64,
    /// Vectors ingested directly at this node (0 for `Inherited` nodes,
    /// whose edit-log is not carried forward -- see `MemoryNode::open_readonly_snapshot`).
    pub mutation_count: usize,
    pub tombstone_count: usize,
}

/// Snapshot of store health across the lineage, mirroring agenticow's
/// `status()` (plan §3) but reporting `working` and `base` separately since
/// there is no single `RvfStore` that represents "the whole lineage."
#[derive(Clone, Debug)]
pub struct MemoryStatus {
    pub dimension: u16,
    pub own_checkpoint_depth: usize,
    pub inherited_depth: usize,
    /// While `paused` is `true` there is no live `working` node (see
    /// [`crate::BranchableMemory::pause`]) -- this reports the status of
    /// what is now the head of `ancestors` (or `base`, if there are none)
    /// instead, since that is the frozen state `working` would resume from.
    pub working: rvf_runtime::StoreStatus,
    pub base: rvf_runtime::StoreStatus,
    /// `true` if this lineage is parked (see [`crate::BranchableMemory::pause`] /
    /// [`crate::BranchableMemory::resume`]).
    pub paused: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_leaves_id_text_none_and_tags_empty() {
        let item = VectorItem::new(vec![1.0, 2.0]);
        assert_eq!(item.id, None);
        assert_eq!(item.text, None);
        assert!(item.tags.is_empty());
        assert_eq!(item.vector, vec![1.0, 2.0]);
    }

    #[test]
    fn builders_set_id_text_and_tags() {
        let mut tags = VectorTags::new();
        tags.insert("role".to_string(), "user".to_string());
        let item = VectorItem::new(vec![0.5, 0.25])
            .with_id(7)
            .with_text("hello")
            .with_tags(tags);
        assert_eq!(item.id, Some(7));
        assert_eq!(item.text.as_deref(), Some("hello"));
        assert_eq!(item.tags.len(), 1);
        assert_eq!(item.tags.get("role").map(String::as_str), Some("user"));
        assert_eq!(item.vector, vec![0.5, 0.25]);
    }
}
