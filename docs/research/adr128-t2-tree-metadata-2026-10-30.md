# ADR-128 T2: keyed tree metadata in an RVF lineage

Date: 2026-10-30. Prototype only, nothing committed to the workspace.

The spike is a standalone crate (`weftos-tree-spike`, own empty `[workspace]`, `rvf-runtime 0.5.0`, `rvf-types 0.3.0`, `proptest 1`). It sits outside the repo on the research volume until T0 lands.

## Verdict

**T2 passes every bar, with the per-record variant.** Branching, lookup, depth and promote are inside the bars on an Apple-silicon laptop. Four RVF 0.5 behaviours needed workarounds, and one cost sits just outside the bar's wording (the first commit in a new layer). None needs U2. U4 is not required, but it would remove the one expensive step.

## Design chosen

- **One RVF record per tree entry.** The vector id is a dense ordinal. The vector is a 1-dim zero (RVF needs one). The entry is a single `Bytes` field read through `get_metadata(ordinal)`.
- **The entry holds its dentry,** not its path: `{parent ordinal, name, kind, mode, uid, gid, size, mtime, chunk hashes, symlink target, xattrs}`.
  - Lookup walks path components over `parent ordinal -> name -> ordinal` maps.
  - Renaming a directory rewrites **one** record. Its children point at its ordinal, so a subtree rename costs O(1), not O(subtree).
- **Ordinal 0 is the root directory.** It also carries the next free ordinal. Ordinals are never reused, so deletes are RVF deletes (`delete()` removes the record, and from the membership filter in a child).
- **Dentry maps are in memory, one per layer.** A frozen layer's map is immutable and shared (`Arc`). A lookup probes the head map, then each base down the chain. Children are listed by merging per-layer maps for one directory only, so no scan is needed. A reopened layer rebuilds a flat map by scanning its records once.
- **Writes buffer in memory** and reach the file with one `commit()`: one `ingest_batch_with_metadata` and one `delete`. Each RVF call fsyncs several times, so per-operation commits would be unusable.
- **`branch()`** commits and freezes the head into an `Arc<Base>`, then calls the native `RvfStore::branch()` for the child. Several children of one frozen layer are siblings.
- **`checkpoint()`** is a branch kept inside the same layer. Writing continues in a new head file. **`rollback()`** drops the head file and re-branches from the chosen checkpoint, or from the branch point.
- **`diff()`** (child against its branch parent) walks both views in parallel. It prunes any subtree whose ordinal is unchanged and untouched, using the ordinals each layer wrote. The cost is proportional to the changes. It returns added, changed and removed paths; a moved subtree reports as removed plus added.
- **`promote()`** applies the diff by path into the parent, then retires the child.
- **Encoding choice by measurement** (90k records, same machine):

| Variant | Measured | Verdict |
|---|---|---|
| Path to ordinal in file-level key/value, one key per path | 13.5 ms per `set_file_metadata`, so about 1,200 s for 90k keys | Unusable |
| One 9 MB file-level blob | 270 ms to write (rewritten whole on every change); branch 26 ms | Rejected: whole rewrite per commit, no keyed lookup |
| **Per-record metadata, in-memory dentry index** | ingest 370 ms once; branch 34 ms | **Chosen** |

## Numbers against the bars

Tree: 97,313 files plus 6,345 directories (103,658 entries) from `git ls-tree -r --name-only HEAD` of the Linux kernel checkout already on disk. Release build, one machine, other builds running at times.

| Bar | Target | Measured | |
|---|---|---|---|
| Lookups per second | 100k or more | 1.2-1.4M (1 layer); 0.76-0.96M (11 layers) | pass |
| Branch a 90k-entry tree, including the per-child metadata copy | under 50 ms | 35-44 ms (raw `RvfStore::branch` alone: 34 ms), same at every depth | pass, thin margin |
| 10-layer against 1-layer lookup | at most 2x | 1.45x, 1.67x in two runs (11 layers including the head) | pass |
| Promote a 1k-change branch | under 1 s | 270-360 ms (diff 20-28 ms; reopen, apply and commit the rest) | pass |
| Layer file size | recorded | 12.4 MiB for 103,658 entries (125 B per entry); a changed layer holds a full snapshot, 11-12 MiB | recorded |

Other numbers:

- Lookup per depth, from 1 to 11 layers: about 1.1M falling smoothly to 0.78M.
- `readdir` of a 64-entry directory through 11 layers: 8-10 microseconds.
- Building the 103k entries in memory: 150-175 ms.
- Committing them to the root layer: 420-440 ms.

### The cost the bar does not cover

RVF starts a child's metadata chain empty, so a child's **first commit is a full snapshot** (the whole tree: about 12 MiB).

- First commit in a new layer: **380-450 ms** at 103k entries.
- A later one-change commit: 55 ms. Every commit clones the metadata, so commit cost is O(tree size), not O(changes).
- Branch is cheap (35 ms) only because the snapshot is deferred to that first commit. A clean branch followed by one write costs about 420 ms.
- Every layer also carries a full in-memory copy of the metadata.
- The ADR's U4 (a custom segment that is inherited, not copied) is the real fix. At 1M entries expect roughly 4 s per first commit, so the answer is U4 or a smaller layer granularity.

## Property-test coverage

`tests/model.rs`, proptest, against a reference in-memory filesystem (a path map with POSIX-style rules).

- **Operations:** create (file, directory, symlink), write, unlink, rmdir, rename (files and whole directory subtrees, with replace rules and rename-into-itself refusal), commit, branch from any layer, checkpoint, rollback (to a checkpoint or the branch point), promote, and reopen from disk. Names come from a four-letter alphabet so paths collide often. Sequences are 5-59 operations over up to 14 layers.
- **After every step, for every live layer:**
  - the layered view equals that layer's model (full walk through `readdir` and `lookup`);
  - `diff` against the parent equals the model diff;
  - frozen layers refuse writes.

  This one check covers "a child never mutates its parent" and "a sibling never sees another sibling's writes", because parents and siblings are compared against their own unchanged models on every step.
- **Refusal cases are checked too:** promote with another live child must return `Busy` and change nothing.
- **Explicit tests** in `tests/smoke.rs`: promote then re-branch gives an empty diff and the same view; isolation of parent, siblings and frozen layers; a depth-2 branch chain with reopen.
- **Runs:** 1,000 cases passed on the eager variant of the membership workaround. After the change to deferred persistence (below), 600 cases plus the explicit tests passed. The final code was not run at 1,000.
- **Failures the harness found,** each now a workaround or a documented limit: the four items below, plus the detached-layer rules.

## RVF 0.5 behaviours found (upstream needs)

1. **U5, new: `branch()` of a COW child builds its membership filter from the parent's local vectors only.**
   - Records inherited from the grandparent drop out of the child's persisted metadata. Reopen either loses them or fails with `InvalidMetadata`. In memory it looks fine.
   - A minimal raw `RvfStore` repro is in the spike's `examples/depth2.rs`.
   - Our workaround rebuilds the filter from what the parent can see, and forces a manifest write. This is a deferred write: it is persisted by the layer's first commit, or by `commit()` or `freeze()` if nothing is pending.
   - Without it, any layer deeper than 1 is unsafe to reopen. **This needs an upstream fix** (a PR in the U1-U4 series).
2. **`RvfStore::freeze()` on a COW child persists an irreversible frozen flag,** so the file can no longer be reopened and extended (`SnapshotFrozen` on the next branch). The spike never calls it. A layer is frozen by the wrapper dropping its write handle.
3. **A child's recorded parent hash covers the parent's file tail,** so writing into a parent breaks every other child of it (and a reopened child's parent link).
   - `promote()` therefore refuses with `Busy` unless the child is the parent's only live child, checked by `Arc` ownership.
   - **Weakness:** a child that was reopened from disk holds no `Arc`, so the check cannot see it. Promote then succeeds and that child's file is broken (`ParentNotFound` or a hash mismatch).
   - Production needs a layer registry above the files, or promote should write a new sealed layer and never append to a parent. The sealed-layer model of T4 and T5 needs the second anyway.
4. **Reopen does not relink the parent.** `TreeLayer::open` returns a flat, detached view. `diff` and `promote` return `Detached`, and rebuilding the chain means scanning each ancestor (O(depth x tree)). It was not built.

## What U2 and U4 would change

- **U2 (sparse membership filter): not needed.** Dense ordinals work. The 64 MiB bitmap cap allows about 537M ordinals, and one ordinal per entry stays under it.
- **U4 (custom segment): not needed for the bars.** It would remove the full-snapshot first commit, the per-layer in-memory metadata copy and the commit-time clones. It becomes necessary around 1M entries per layer or if sealing must take under about 100 ms.
- **New: the U5 fix** (item 1).
