//! The lineage-mutating half of `BranchableMemory`: `checkpoint`, `rollback`,
//! `promote`, `branch`, `fork`. Split from `branchable_memory.rs` purely to
//! keep files under ~500 lines; this is still `impl BranchableMemory`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::branchable_memory::BranchableMemory;
use crate::error::{CowMemoryError, Result};
use crate::node::{self, CheckpointId, MemoryNode};
use crate::types::{PromoteReport, VectorTags};

/// One node's owned edits/texts/tags/tombstones, snapshotted for replay in
/// `promote` (see its doc comment for why this is a clone-first pass).
type NodeReplaySnapshot = (
    HashMap<u64, Vec<f32>>,
    HashMap<u64, String>,
    HashMap<u64, VectorTags>,
    HashSet<u64>,
);

/// Which point in `self`'s current lineage a `branch`/`fork` reads from.
enum ForkSource {
    /// Everything visible right now: `working` plus this lineage's own
    /// checkpoints, newest first (agenticow's "clone" -- fork off the tip).
    Head,
    /// A specific earlier checkpoint (and everything at/older than it),
    /// excluding `working` and anything newer ("clone-of-clone" -- you can
    /// fork from a fork's own checkpoint just as well as from the tip).
    Checkpoint(usize),
}

impl BranchableMemory {
    /// The node a fresh `working` branches from: the newest checkpoint, or
    /// `base` when there is none.
    fn head_node_mut(&mut self) -> &mut MemoryNode {
        match self.ancestors.first_mut() {
            Some(n) => n,
            None => &mut self.base,
        }
    }

    /// Freeze `working` into a new checkpoint (pushed onto `ancestors`,
    /// newest-first) and derive a fresh, empty, writable `working` from it.
    /// O(1) in vectors: `branch` copies none (see `MemoryNode::branch_from`).
    pub fn checkpoint(&mut self, label: impl Into<String>) -> Result<CheckpointId> {
        let label = label.into();
        let current = self.working.as_mut().ok_or(CowMemoryError::Paused)?;
        current.freeze()?;
        let checkpoint_id = current.id;

        let new_id = CheckpointId::alloc();
        let new_path = self.dir.join(format!("node-{}.working.rvf", new_id.0));
        let opts = self.opts.clone();
        let new_working = MemoryNode::child_of(
            new_id,
            self.working.as_mut().expect("checked above"),
            &new_path,
            opts,
            Some("working".into()),
        )?;

        let mut frozen = self.working.replace(new_working).expect("checked above");
        frozen.label = Some(label);
        self.ancestors.insert(0, frozen);
        self.write_manifest()?;
        Ok(checkpoint_id)
    }

    /// Freeze `working` into the ancestors chain -- exactly like
    /// `checkpoint` -- but do NOT derive a fresh writable child afterward:
    /// the lineage is parked. `query`/`diff`/`lineage`/`status` still walk
    /// the (now `working`-less) chain fine; `ingest`/`delete`/`checkpoint`/
    /// `rollback`/`promote`/`branch` fail closed with
    /// [`CowMemoryError::Paused`] until [`Self::resume`]. Idempotent:
    /// pausing an already-paused lineage is a no-op `Ok(())`.
    ///
    /// Durable: writes a manifest recording the parked state, so a
    /// `pause` → process exit → [`Self::open`] round-trip reopens still
    /// paused (see `crate::manifest`).
    pub fn pause(&mut self, label: impl Into<String>) -> Result<()> {
        let Some(mut working) = self.working.take() else {
            return Ok(());
        };
        if let Err(e) = working.freeze() {
            // Put it back so a failed freeze doesn't strand the lineage in
            // a working-less-but-not-actually-paused limbo.
            self.working = Some(working);
            return Err(e);
        }
        working.label = Some(label.into());
        self.ancestors.insert(0, working);
        self.write_manifest()?;
        Ok(())
    }

    /// Derive a fresh, empty, writable `working` from the current head
    /// (this lineage's newest checkpoint, or `base` if there is none) --
    /// O(1) regardless of lineage size, the same `derive`-not-copy
    /// guarantee as every other node derivation in this crate (see
    /// `MemoryNode::derive_from`): the returned `working` starts with zero
    /// vectors of its own. Idempotent: resuming a lineage that isn't
    /// paused is a no-op `Ok(())`.
    pub fn resume(&mut self) -> Result<()> {
        if self.working.is_some() {
            return Ok(());
        }
        let new_id = CheckpointId::alloc();
        let new_path = self.dir.join(format!("node-{}.working.rvf", new_id.0));
        let opts = self.opts.clone();
        let new_working = MemoryNode::child_of(
            new_id,
            self.head_node_mut(),
            &new_path,
            opts,
            Some("working".into()),
        )?;
        self.working = Some(new_working);
        self.write_manifest()?;
        Ok(())
    }

    /// Discard `working` and every checkpoint newer than `to`, then derive
    /// a fresh `working` from whatever is now the head (the target
    /// checkpoint, or `base` if `to` is `None`).
    ///
    /// `to` must name one of `self`'s *own* checkpoints (an id returned by
    /// a prior `checkpoint()` call on this same `BranchableMemory` that
    /// hasn't since been rolled back or promoted away). It cannot target
    /// `working` itself (never checkpointed, so not a valid rollback point)
    /// or an `inherited` node (frozen forever from a source lineage,
    /// nothing to "roll back to" there -- fork from it again instead).
    pub fn rollback(&mut self, to: Option<CheckpointId>) -> Result<()> {
        if self.working.is_none() {
            return Err(CowMemoryError::Paused);
        }
        let target_idx = match to {
            None => None,
            Some(id) => Some(
                self.ancestors
                    .iter()
                    .position(|n| n.id == id)
                    .ok_or_else(|| node::not_found(id))?,
            ),
        };

        // `ancestors` is newest..oldest. Discard everything strictly newer
        // than the target (index range `0..idx`); `None` discards the
        // whole chain.
        let discarded: Vec<MemoryNode> = match target_idx {
            None => std::mem::take(&mut self.ancestors),
            Some(idx) => self.ancestors.drain(0..idx).collect(),
        };

        let new_id = CheckpointId::alloc();
        let new_path = self.dir.join(format!("node-{}.working.rvf", new_id.0));
        let opts = self.opts.clone();
        let new_working = MemoryNode::child_of(
            new_id,
            self.head_node_mut(),
            &new_path,
            opts,
            Some("working".into()),
        )?;
        let old_working = self.working.replace(new_working).expect("checked above");

        let mut to_remove: Vec<PathBuf> = discarded.iter().map(|n| n.path.clone()).collect();
        to_remove.push(old_working.path.clone());
        drop(discarded);
        drop(old_working);
        for path in &to_remove {
            MemoryNode::remove_file_best_effort(path);
        }

        self.write_manifest()?;
        Ok(())
    }

    /// Replay this lineage's own checkpoint chain (oldest first, so a later
    /// checkpoint's edits correctly win over an earlier one on id
    /// collision) plus `working` into `base`, then collapse: discard the
    /// checkpoint chain and `working`, and derive a fresh `working` from
    /// the now-updated `base`.
    ///
    /// `base` stays writable throughout (it is never frozen by
    /// `checkpoint`), so this is a plain `ingest_batch`/`delete` replay, not
    /// a COW operation.
    ///
    /// **Known limitation**, unlike `BranchableMemory::delete`: this *does*
    /// call `RvfStore::delete` on `base`, because `base`'s tombstones need
    /// to be physically enforceable independent of any particular
    /// in-process `BranchableMemory` (a reopened lineage has no working/
    /// ancestor tombstone sets at all -- see `BranchableMemory::open`). The
    /// tradeoff is `delete`'s sticky-bitmap behavior (see `delete`'s doc
    /// comment): if a promoted delete's id is later re-ingested into `base`
    /// under the same id, it will not become visible again short of a
    /// `compact()` (which itself needs care -- see `delete`'s note on why
    /// `compact` would drop the re-ingested data too). Flagged as a Phase 2
    /// open question rather than solved here.
    pub fn promote(&mut self) -> Result<PromoteReport> {
        let working = self.working.as_ref().ok_or(CowMemoryError::Paused)?;

        // Captured before the replay/collapse below removes `working`'s
        // backing file -- this is the lineage id a chain witness needs for
        // the "what got promoted" side of `TurnLedger::on_promote`
        // (WEFT-616 Phase 3), and it would otherwise be unrecoverable once
        // `promote` returns.
        let child_id = *working.store.file_id();

        // Snapshot each node's edits/tombstones as owned data first. This
        // sidesteps needing simultaneous `&mut` borrows of `base` and of
        // `ancestors`/`working` to do the replay in one pass -- the clone
        // cost is a Phase 1 simplicity trade-off, not a hard constraint.
        let mut replay: Vec<NodeReplaySnapshot> = Vec::with_capacity(self.ancestors.len() + 1);
        for node in self.ancestors.iter().rev() {
            replay.push((
                node.edit_log.clone(),
                node.texts.clone(),
                node.tags.clone(),
                node.tombstones.clone(),
            ));
        }
        replay.push((
            working.edit_log.clone(),
            working.texts.clone(),
            working.tags.clone(),
            working.tombstones.clone(),
        ));

        // `base` was frozen so children could `branch()` from it; reopen it
        // writable for the in-place replay. The next `branch_from` re-freezes.
        self.base.reopen_writable()?;

        let mut ingested = 0usize;
        let mut deleted = 0usize;
        for (edit_log, texts, tags, tombstones) in &replay {
            if !edit_log.is_empty() {
                let ids: Vec<u64> = edit_log.keys().copied().collect();
                let vectors: Vec<&[f32]> = ids.iter().map(|id| edit_log[id].as_slice()).collect();
                self.base.store.ingest_batch(&vectors, &ids, None)?;
                ingested += ids.len();
                for id in &ids {
                    if let Some(text) = texts.get(id) {
                        self.base.texts.insert(*id, text.clone());
                    }
                    if let Some(t) = tags.get(id) {
                        self.base.tags.insert(*id, t.clone());
                    }
                }
            }
            if !tombstones.is_empty() {
                let ids: Vec<u64> = tombstones.iter().copied().collect();
                // Meaningful for ids physically present in `base`; a no-op
                // otherwise. The BranchableMemory-level tombstone set
                // (below) is what actually masks `inherited` history.
                self.base.store.delete(&ids)?;
                deleted += ids.len();
                self.base.tombstones.extend(ids.iter().copied());
            }
        }

        // `base` is mutated in place above (ingest/delete), never
        // re-derived, so its `file_id` is stable -- captured after the
        // replay purely for locality with the report it feeds.
        let parent_id = *self.base.store.file_id();
        let parent_hash = *self.base.store.last_witness_hash();

        let discarded_ancestors = std::mem::take(&mut self.ancestors);
        let new_id = CheckpointId::alloc();
        let new_path = self.dir.join(format!("node-{}.working.rvf", new_id.0));
        let opts = self.opts.clone();
        let new_working = MemoryNode::child_of(
            new_id,
            &mut self.base,
            &new_path,
            opts,
            Some("working".into()),
        )?;
        let old_working = self.working.replace(new_working).expect("checked above");

        let mut to_remove: Vec<PathBuf> =
            discarded_ancestors.iter().map(|n| n.path.clone()).collect();
        to_remove.push(old_working.path.clone());
        drop(discarded_ancestors);
        drop(old_working);
        for path in &to_remove {
            MemoryNode::remove_file_best_effort(path);
        }

        self.write_manifest()?;
        Ok(PromoteReport {
            ingested,
            deleted,
            child_id,
            parent_id,
            parent_hash,
        })
    }

    /// Fork off the current tip: a brand-new, independent `BranchableMemory`
    /// at `path` that can see everything this lineage sees *right now*
    /// (`working` + this lineage's own checkpoints + `base` + anything this
    /// lineage itself inherited), but whose own writes never appear back in
    /// `self`, and vice versa (agenticow's "isolated per-actor view" /
    /// cubecow's "clone").
    pub fn branch(&self, path: impl AsRef<Path>) -> Result<BranchableMemory> {
        if self.working.is_none() {
            return Err(CowMemoryError::Paused);
        }
        self.spawn_independent(ForkSource::Head, path.as_ref())
    }

    /// Fork from a specific earlier checkpoint in `self`'s own chain
    /// (cubecow's "clone-of-clone" -- forking a fork, or forking a point in
    /// the past rather than the tip). `from` must be one of `self`'s own
    /// checkpoint ids (not `working`, not an `inherited` node).
    pub fn fork(&self, from: CheckpointId, path: impl AsRef<Path>) -> Result<BranchableMemory> {
        let idx = self
            .ancestors
            .iter()
            .position(|n| n.id == from)
            .ok_or_else(|| node::not_found(from))?;
        self.spawn_independent(ForkSource::Checkpoint(idx), path.as_ref())
    }

    fn spawn_independent(&self, source: ForkSource, target_dir: &Path) -> Result<BranchableMemory> {
        std::fs::create_dir_all(target_dir)?;

        // The new lineage's `inherited` chain: read-only reopens of every
        // node visible from the chosen source point, newest first, ending
        // at `self.base` and then anything `self` itself inherited (so
        // forking a fork carries the whole history forward).
        let mut inherited: Vec<MemoryNode> = Vec::new();
        match source {
            ForkSource::Head => {
                // `branch()` (the only caller that reaches `ForkSource::Head`)
                // already fails closed with `Paused` when `self.working` is
                // `None`, so this is always `Some` here.
                inherited.push(
                    self.working
                        .as_ref()
                        .expect("ForkSource::Head requires working; branch() checks paused")
                        .open_readonly_snapshot()?,
                );
                for n in &self.ancestors {
                    inherited.push(n.open_readonly_snapshot()?);
                }
            }
            ForkSource::Checkpoint(idx) => {
                for n in &self.ancestors[idx..] {
                    inherited.push(n.open_readonly_snapshot()?);
                }
            }
        }
        inherited.push(self.base.open_readonly_snapshot()?);
        for n in &self.inherited {
            inherited.push(n.open_readonly_snapshot()?);
        }

        let head_store = &inherited
            .first()
            .ok_or_else(|| {
                CowMemoryError::Io(std::io::Error::other(
                    "spawn_independent: inherited chain unexpectedly empty",
                ))
            })?
            .store;

        let new_base_id = CheckpointId::alloc();
        let new_base_path = target_dir.join(format!("node-{}.base.rvf", new_base_id.0));
        let mut new_base = MemoryNode::derive_from(
            new_base_id,
            head_store,
            &new_base_path,
            self.opts.clone(),
            Some("base".into()),
        )?;

        let new_working_id = CheckpointId::alloc();
        let new_working_path = target_dir.join(format!("node-{}.working.rvf", new_working_id.0));
        let opts = self.opts.clone();
        let new_working = MemoryNode::child_of(
            new_working_id,
            &mut new_base,
            &new_working_path,
            opts,
            Some("working".into()),
        )?;

        let child = BranchableMemory {
            dir: target_dir.to_path_buf(),
            opts: self.opts.clone(),
            working: Some(new_working),
            ancestors: Vec::new(),
            base: new_base,
            inherited,
        };
        child.write_manifest()?;
        Ok(child)
    }
}
