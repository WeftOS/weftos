//! Reading a COW lineage.
//!
//! Since rvf-runtime 0.5 a node built with `RvfStore::branch` reads through
//! to its parent natively -- but exactly ONE level: the child's membership
//! filter is sized from, and filled with, the parent's *own* vectors only,
//! so a grandparent's vectors are dropped even though the parent's query
//! recursion returns them. A lineage with checkpoints (working -> ckpt ->
//! ... -> base) or an inherited fork source therefore still has to be
//! composed across several layers, which is what [`query_chain`] does.
//! The common single-parent shape (working -> base) is served by one native
//! query: [`query_single_parent`].
//!
//! Crate-level tombstones remain the only hiding mechanism (see
//! `BranchableMemory::delete`), so both paths apply them explicitly.

use rvf_runtime::QueryOptions;

use crate::error::Result;
use crate::node::MemoryNode;

/// A single chain-walk result: a vector id and its distance from the query
/// vector, comparable across every node in the lineage because the whole
/// lineage shares one metric (`DistanceMetric::L2`) over one normalized
/// vector space (see `crate::normalize`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoredId {
    pub id: u64,
    pub distance: f32,
}

/// Per-node overscan factor. agenticow overscans `k*4` per node
/// (`index.js:304`) before merging and re-ranking, so that a node whose
/// nearest-by-its-own-metric candidates get masked by a newer node's
/// tombstones still leaves enough headroom to fill out `k` results.
const OVERSCAN_FACTOR: usize = 4;

/// Query every node in `chain` (already in priority order: nearest/newest
/// first, oldest/base-and-inherited last) and merge into the top `k`.
///
/// `chain[0]` (the topmost / newest node, i.e. `working`) wins ties over
/// everything below it: for a given id, the first node in the walk that
/// returns it is authoritative, and every node's tombstones mask that id
/// from surfacing there *and* from every node still older. This is
/// agenticow's "child wins on id collision" contract (plan §3), implemented
/// explicitly here because RVF's own COW read-through does not exist on
/// this platform.
///
/// A node's tombstones are checked against its *own* results too (not only
/// propagated to older nodes) -- deliberately: `BranchableMemory::delete`
/// never calls `RvfStore::delete`, precisely so this crate-level tombstone
/// set is the single source of truth for "is this id hidden here," at
/// every level including same-node re-ingest-after-delete. See the doc
/// comment on `BranchableMemory::delete` for why relying on RVF's own
/// `deletion_bitmap` for that would be actively wrong (its delete bit
/// outlives a later re-ingest of the same id, and outlives `compact()` too).
/// Whether a result `id` that surfaced from `node` only by native
/// read-through of `parent` must stay hidden: the parent tombstoned it and
/// `node` did not re-ingest it into its own slab. Without this, an id that
/// was ingested then deleted inside `parent` would resurface through the
/// child (the old per-node walk never saw the parent's slab from the child).
fn hidden_by_parent(node: &MemoryNode, parent: &MemoryNode, id: u64) -> bool {
    parent.tombstones.contains(&id) && !node.owns(id)
}

/// Single-parent lineage (`working` branched from `base`, no checkpoints,
/// nothing inherited): one native query reads through to `base`; no walk.
pub(crate) fn query_single_parent(
    working: &MemoryNode,
    base: &MemoryNode,
    query_vector: &[f32],
    k: usize,
    query_opts: &QueryOptions,
) -> Result<Vec<ScoredId>> {
    let overscan_k = k.saturating_mul(OVERSCAN_FACTOR).max(k).max(1);
    let results = working.store.query(query_vector, overscan_k, query_opts)?;
    let mut out: Vec<ScoredId> = Vec::with_capacity(k);
    for r in results {
        if working.tombstones.contains(&r.id) || hidden_by_parent(working, base, r.id) {
            continue;
        }
        out.push(ScoredId {
            id: r.id,
            distance: r.distance,
        });
        if out.len() == k {
            break;
        }
    }
    Ok(out)
}

pub(crate) fn query_chain(
    chain: &[&MemoryNode],
    query_vector: &[f32],
    k: usize,
    query_opts: &QueryOptions,
) -> Result<Vec<ScoredId>> {
    let overscan_k = k.saturating_mul(OVERSCAN_FACTOR).max(k).max(1);

    let mut masked: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut seen: std::collections::HashSet<u64> = std::collections::HashSet::new();
    let mut merged: Vec<ScoredId> = Vec::new();

    for (i, node) in chain.iter().enumerate() {
        // A branched node also returns its native parent's vectors (the next
        // node in the chain); those duplicates are deduped by `seen`, but the
        // parent's own tombstones must still apply to them.
        let native_parent = if node.store.is_cow_child() {
            chain.get(i + 1).copied()
        } else {
            None
        };
        let results = node.store.query(query_vector, overscan_k, query_opts)?;
        for r in results {
            if masked.contains(&r.id) || node.tombstones.contains(&r.id) || seen.contains(&r.id) {
                continue;
            }
            if native_parent.is_some_and(|parent| hidden_by_parent(node, parent, r.id)) {
                continue;
            }
            seen.insert(r.id);
            merged.push(ScoredId {
                id: r.id,
                distance: r.distance,
            });
        }
        // Shadow every *older* node from here on. Applied after scanning
        // this node's own results (which are checked against
        // `node.tombstones` directly, above) so a newer node's tombstones
        // never mask something a still-newer node already returned.
        masked.extend(node.tombstones.iter().copied());
    }

    merged.sort_by(|a, b| {
        a.distance
            .partial_cmp(&b.distance)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    merged.truncate(k);
    Ok(merged)
}

#[cfg(test)]
mod tests {
    // Exercised end-to-end via `tests/tombstone.rs` and `tests/acceptance.rs`
    // against real `RvfStore` instances -- chain-walk merge logic isn't
    // meaningfully unit-testable without a real store to query.
}
