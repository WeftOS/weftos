# ADR-128 T5: git integrator over sealed layer trees

Date: 2026-10-30. Verdict: **T5 passes** on every bar below. Spike crate: `weftos-integrator-spike`, a sibling of the T2 `weftos-tree-spike` crate (own `[workspace]`, path dependency on T2). Not committed.

## Design

- **Chunks** (`chunk.rs`): file bytes are split into fixed chunks and stored by SHAKE-256 (`sha3::Shake256`, 32 bytes) in a local directory standing in for RustFS. Reads re-verify the hash. Entries carry chunk refs, as in T2.
- **Sealed layer** (`seal.rs`): the name is SHAKE-256 over a canonical byte form of the full entry list (path, kind, mode, uid, gid, size, mtime, chunk refs, symlink, xattrs, sorted by path), the parent names and the layer metadata (author, email, commit time, message). No file-system path, ordinal or wall-clock enters it. Metadata is in the seal so the commit it produces is bound to the name. Sealing also computes the op list (removals, then puts) from the T2 `diff_ops`, and self-checks that parent files plus ops equal the layer's files.
- **Git access**: the `git` CLI plumbing, not `git2`. Reasons: no C library or build.rs to add (the diff gate and the Mac build both stay simple), every step is an explicit argv, and `merge-tree --write-tree` gives git's own three-way merge without a working tree. The environment is cleared (no user config, no signing, no hooks), and dates are passed as `<unix> +0000`.
- **Integrator** (`integrate.rs`):
  1. `init_repo` creates a real `.git` with one root commit holding the base layer. `open` refuses a repo whose HEAD tree differs from the base layer (path, mode and blob id).
  2. `submit` accepts sealed layers in any order. `run` computes depth (base children are 1) and repeatedly picks the lowest `(depth, name)` among layers whose parent's merge is recorded. A layer whose parent failed is logged as blocked; a layer whose parent is unknown is logged as blocked at the end.
  3. For an eligible layer: build its tree from the parent layer's commit plus ops using a private index (`read-tree`, `update-index -z --index-info`, `write-tree`); commit with `commit-tree` using the layer's metadata for author, committer and date; put it on `refs/integrator/layer/<id>`; `git merge-tree --write-tree` against the target ref.
  4. Clean: a no-fast-forward merge commit (parents target, layer commit), identity fixed to the integrator, time taken from the layer. Conflict: record the conflicted paths, the conflict-marker hunks read from the result tree, and git's messages; skip the layer; target unchanged.
  5. Temporary refs are deleted; HEAD's branch moves to the final commit. Output is the log, the final commit and the final tree. `Integration::render()` is a stable text form used for equality.
- **Order note.** With `(depth, name)` the stacked layer S (depth 2) merges after both depth-1 siblings, so the order is A, B, S or B, A, S by name, never S first. The ADR's "A, then stacked, then B" is also satisfied as "parent before child".

## Scenario results

Base: five files (three 10-line text files, README, an executable script). A edits `src/a.txt` line 3 and adds a file. B edits `src/b.txt` and deletes README. S is stacked on A and edits `src/a.txt` line 8, `src/c.txt` and adds `deep/er/file.txt`. Names below are the first 12 hex of the layer name.

| seq | layer | depth | result |
|---|---|---|---|
| 1 | A `25d6ba922f06` | 1 | merged |
| 2 | B `5ca32363fb05` | 1 | merged |
| 3 | S `f486a8bb9338` | 2 | merged |

Final commit `0c4d3b1508cfa4ae90f7ef05d9534513f4003017`, tree `b6d1109909a83378cbaccb363a68cebf734675e1`. The final tree keeps the 100755 mode on the script, lacks README, and `src/a.txt` carries both A's and S's edits.

## Determinism evidence

Whole scenario (layers, chunks, repo) rebuilt from scratch each time, with C present. Four separate processes of the example binary, the fourth in a path with spaces and a different depth:

| run | work dir | final commit | final tree |
|---|---|---|---|
| 1 | fresh temp dir | `0c4d3b15...3003017` | `b6d11099...4675e1` |
| 2 | fresh temp dir | identical | identical |
| 3 | fresh temp dir | identical | identical |
| 4 | `another/deeper/working dir` | identical | identical |

The in-process test repeats this and also compares the full rendered log (every layer commit, merge commit and tree hash), not only the final ones. The run without C gives the same final commit and tree, as it should: C is skipped. The cross-node bar was replaced by the different-path run, as agreed (Tailscale down); a real second node is still to do.

## Conflict handling

Sibling C edits the same line of `src/a.txt` as A. Name order puts A first, so C loses. Every run records the same entry: layer commit `5c643f910e53...`, one file `src/a.txt`, one hunk, git messages "Auto-merging" and "CONFLICT (content)":

```
<<<<<<< refs/integrator/target
a line 2 edited by A
=======
a line 2 edited by C
>>>>>>> refs/integrator/layer/e2f7ac2d3b9b
```

Target is unchanged by C, S still merges (its parent A merged). The test also covers the other case (if C had sorted first, A would conflict and S would be logged blocked). Modify/delete conflicts have no markers: the path and git's message are recorded and `hunks` is empty.

## Property tests

`tests/prop.rs`, 24 cases of proptest: random DAGs of 3 to 8 branches (each parent is the base or any earlier branch), 1 to 3 random line edits or deletes per branch over four files. Each case builds the world in three separate temp dirs and integrates with arrival order identity, a random shuffle, and identity again. Asserted: the three renderings (all hashes, conflicts, blocks) are equal; every layer is logged exactly once; every child is logged after its parent, and a merged child has a merged parent; `(depth, name)` is non-decreasing over the log. All 24 pass (about 68 s in a debug build). Scenario tests: 5 pass.

## What production needs

- **A layer registry.** Sealed names, parents and metadata must be published somewhere the integrator and CI can read (T2 flagged this: the RVF file does not relink a reopened child). Here `SealedLayer` is passed in memory.
- **A cut manifest.** Determinism holds for a given submitted set. A late sibling merges at a different point if the integrator already ran; production needs a closed batch (a manifest naming the layers and the base) so a run is reproducible, and later arrivals form the next batch on the new target.
- **Merge commits per layer** bloat history; B4 may want fast-forward or squash policy. This is a policy choice, not a determinism issue.
- **Gaps**: empty directories do not exist in git; names with tabs or newlines are untested; rename detection is left to git's merge (a renamed file in one layer and an edit in another is not covered by tests); only single-parent layers are implemented; large files need streaming rather than full reads in `blob_for`; the integrator identity and the push/hand-off to CI are not built (B4).
- **Cross-node** run on a second machine, and a CI job running this suite, per B4.
- Real SHAKE-256 in RVF lineage (U1) is separate: these names are our own hashes and do not depend on it.
