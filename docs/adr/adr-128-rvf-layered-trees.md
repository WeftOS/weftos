# ADR-128: RVF layered trees: copy-on-write branch metadata and agent memory, composed per job, with git kept as the integrator

- **Status**: Proposed (2026-10-29; owner direction, see Context). Adoption is gated on the proofs in "Prove first".
- **Updated**: 2026-10-30 (T0 finished by ADR-129: upstream rvf-crypto, rvf-types 0.3 workspace-wide). Also 2026-10-30 (T4 passes locally; ML-DSA placeholder finding). Also 2026-10-30 (T3 passes). Also 2026-10-30 (T5 passes). Also 2026-10-30 (T2 passes with the per-record design; U5 added). Also 2026-10-30 (T1 partial results from the JuiceFS spike are recorded in the proof table). Also 2026-10-30: the RVF facts are rewritten from a source study of ruvnet/ruvector `fefc2ec9` (`docs/research/rvf-container-study-2026-10-29.md`). The first draft read the pinned rvf-runtime 0.3.2 and misjudged the format. The proofs now name their upstream dependencies, with a fallback for each, and the plan runs the proofs alongside the upstream contributions.
- **Deciders**: owner
- **Prior decisions**: the decision store, consulted on 2026-10-29, holds ADR-126 and ADR-127 on this topic.
- **Builds on**:
  - ADR-126: the mesh object store on RustFS;
  - ADR-127: WeftOS-orchestrated CI;
  - ADR-017: the substrate state tree;
  - ADR-057: per-path read access rules;
  - ADR-063: signed envelopes;
  - ADR-028: SHAKE-256 hashing with Ed25519 and ML-DSA-65 dual signing;
  - ADR-089: ExoChain DAG merge;
  - ADR-099: placement;
  - ADR-121: versions and instances;
  - `crates/clawft-cow-memory`.
- **Changes**: ADR-126 §4 (the worktree overlay) and ADR-127 §3 (caches) are re-expressed as RVF layers. Both carry an Updated note pointing here.

## Context

The owner's direction on 2026-10-29, in his words where possible:

- **No copies.** "A copy from remote to local should never need happen." A checked-out tree is worked on in place, from any node.
- **Roles on different nodes.** "Git may not need to be done by the same node that runs build or the same one that runs the tests."
- **Branching storage.** A writer branches, and the branch holds only what it changed, while "most of its files [are] still in parent".
- **Metadata as RVF.** "It should be able to create a CoW rvf that is the metadata that gets published and maintained with each branch." **RVF holds metadata only:** paths, entries, attributes, directory listings and chunk references. File contents are content-addressed chunks in RustFS.
- **Agent memory too.** "This is also how we prime memory for each agent, rvf should be the carrier on this stuff because we can actually compose them in the ci/cd for each specific env/job."
- **Git stays.** "We dont get rid of git, keep it there. Then use it to sort it out. The agent doing the work, the reviewer, the build process, etc.. they do not care."
- **Ordered merging.** "Almost deterministic merging async ... Dependency is enforced by the CoW for the purpose of merge order."
- **WeftOS wraps it.** "weftos can wrap both very easily."

### What RVF is today (source study, ruvnet/ruvector `fefc2ec9`)

RVF is a **container format**, not only a vector store. The full study with file:line citations is `docs/research/rvf-container-study-2026-10-29.md`.

| Area | Finding | Consequence here |
|---|---|---|
| Container | Append-only segments with 64-byte headers, a manifest found from the tail, and a 4 KB Level-0 root page. `SegmentType` is a closed enum, and `RvfStore` has no API to append a custom segment. But `scan_preservable_segments` (`rvf-runtime/src/store.rs`) keeps unknown "vendor extension" segment types byte for byte through compaction. | Typed tree records ride in `META_SEG` today. A custom segment type needs an upstream API (issue B). |
| Copy-on-write | `branch()` is native, and already was in 0.3.2: a cluster map, a membership filter, and parent read-through in exact query (`cow_exact_parent_scan`). Vectors, the index and metadata inherit, but **metadata is a full in-memory copy per child**. `derive()` is **not** copy-on-write and **ignores its `DerivationType`** (`store.rs:3146`); the Node binding hard-codes `Filter`. Each file has a single parent, and there's no merge or promote. | `clawft-cow-memory` uses `derive()`, which is why it walks layers itself. Switching to `branch()` removes the vector walk. |
| Lineage | `FileIdentity` holds the file id, the parent id, `parent_hash` and depth. `parent_hash` comes from `simple_shake256_256`, a **non-cryptographic add-and-rotate fold** over the parent file's last 64 KB (`store.rs:3234`, `:4546`). The same fold is used for compaction's cluster dedup (`cow_compact.rs:114-117`, where **a collision drops child data**), for witness parent-cluster hashes, for the membership-filter hash, and for kernel and eBPF hashes. `rvf-runtime` already depends on `rvf-crypto`, which has real `shake256_256` and `compute_manifest_hash`. | Lineage isn't tamper-evident, and compaction can lose data. Upstream PR U1 fixes this. |
| Keyed data | `get_metadata(id)` and file-level key/value metadata arrived in 0.4. They're stored as a delta chain with a snapshot every 32 deltas, and read from a BTreeMap loaded at open: O(log n) lookups. `META_IDX_SEG` is a type code only; nothing reads or writes it. Every record needs a vector id. `branch()` sizes a **dense membership bitmap from the maximum id** and fails above 64 MiB (about 537M ids). | 90k tree entries fit (about 9 MB per snapshot), but **ids must be dense ordinals**, not path hashes, until upstream PR U2 lands. |
| Signing | Whole-file Ed25519 (`create_signed` / `open_signed`), with per-segment SHAKE hashes. **No ML-DSA upstream.** Branched children aren't signed. | Layer signing uses our own envelope (ADR-063), with ML-DSA-65 dual signing per ADR-028 via `weftos-rvf-crypto`. |
| Streaming | "Ofer" (ruvector ADR-353, Proposed, code on main): a signed descriptor, SHA-256 chunk hashes, resumable HTTP Range through a `Transport` trait. It streams a whole body, not a layer fetched by hash, and it isn't S3-native. | Publication uses sealed layers named by SHAKE-256, fetched from RustFS. A custom `Transport` is optional. |
| DiskANN | `ruvector-diskann` 2.3.2 is a **separate crate**: a directory of files with string ids and no RVF dependency. It has no copy-on-write and no membership filter. Its graph lives in RAM, with vectors optionally mmapped, and search ignores the PQ codes. RVF's own index is HNSW (`INDEX_SEG`, with RaBitQ as an option). | Agent memory inside layers uses RVF's HNSW. DiskANN is only a possible separate large-corpus index, not a layer format. |
| Versions | crates.io has rvf-runtime 0.5.0 and rvf-types 0.3.0 (2026-10-27/28). WeftOS locks 0.3.2 and 0.2.1. The breaking changes are new fields on the options, query and result types, new `MetadataValue` variants, and a non-exhaustive `ErrorCode`. | T0 does the upgrade. Whether upstream `rvf-crypto` coexists with our `weftos-rvf-crypto` fork is still to be established (ADR-029). |

The ruvnet-brain knowledge base was unavailable during this work, so every RVF claim comes from source.

## Decision

### 1. A tree is a stack of RVF layers

- **A working tree's metadata** (paths, entries, attributes, chunk references) is an RVF lineage: the project base, then the worktree branch, then any sub-branches, then a writable tip.
- **A branch stores only what it changed,** and reads fall through to the parent.
- **File contents are chunks in RustFS,** addressed by content hash, so unchanged files are shared by every branch at no cost.
- **Tree entries are records keyed by dense ordinals,** with a path-to-ordinal map in the layer's key/value metadata. Path-hash ids wait for U2.

### 2. RVF is also the carrier for agent memory

- Agent knowledge (project facts, role packs, lessons, conventions) is RVF layers with vectors and HNSW, the native case, with the same branch, checkpoint, rollback, promote and diff.
- **Priming an agent means choosing layers,** not assembling prompt text.

### 3. CI composes a layer stack per job

- **A job's view, top to bottom:**
  1. the job tip, writable;
  2. the job spec;
  3. the agent or role pack;
  4. the environment (toolchain, OS, node facts);
  5. the worktree branch;
  6. the project base.
- **Every layer is identified by SHAKE-256 and signed** (§4). After the job, its tip is promoted or discarded.
- **Composition across several independent parents isn't native** (one parent per file). It is built in WeftOS first, by generalising `clawft-cow-memory`'s ordered `inherited` walk into a `compose` op, and proposed upstream as `DerivationType::Merge` (issue A).

### 4. Publication through the substrate

- **Sealed layers are immutable.** Each is named by SHAKE-256 over its bytes and signed with our envelope: Ed25519 plus ML-DSA-65, per ADR-028 and ADR-063.
- **Announcing:** publishing means announcing the layer's name, its parent names and its signature over the substrate.
- **Access:** ADR-057 rules decide which nodes and agents receive which layers. Client layers go only to that client's boundary.
- **Fetching:** nodes fetch layers from RustFS and keep them locally, so lookups and `git status` read local metadata. Only writes go to the tip's owner.

### 5. One writer per tip, roles on separate branches

- **Each writable tip has exactly one owning node.** Different roles get their own sub-branches. For example, git work on the tree branch, build outputs in a build branch, test results in a test branch.
- **No two nodes write the same tip,** so no distributed lock is needed on the hot path.

### 6. Git stays, as the integrator

- **The tree keeps a real `.git`.** Workers (the agent doing the work, the reviewer, the build and test jobs) see only files.
- **A WeftOS integrator role owns git:** commits from a branch's diff, three-way merges, conflicts, rebases onto upstream, pushes and the hand-off to CI.
- **Git stays the publishing and interoperability format.**

### 7. Merge order comes from the copy-on-write DAG

- **A layer's parent names are dependency edges,** so the integrator merges in topological order. A branch is never merged before the layer it was based on.
- **Merging is asynchronous,** with no landing queue.
- **Merging is deterministic:** git's three-way merge of known base, branch and target layers gives the same result on any node.
- **The remaining cases:** sibling line conflicts are detected deterministically and resolved with a fixed tie-break, and semantic conflicts are caught by CI on the merged result.
- **The edges are our signed layer names (§4),** not RVF's `parent_hash`, which isn't cryptographic until U1. After U1, the RVF `FileIdentity` hash is cross-checked against ours. This mirrors ExoChain's DAG rules (ADR-089).

### 8. Privacy and isolation

- **Paths and file names are confidential for client work.** So each account has its own layers, its own RustFS store and its own metadata owner, running as that account's user (the PG isolation rules).
- **Nothing published across the mesh** carries client paths outside that client's boundary.

### 9. WeftOS wraps it

Agents and scripts see a directory or a `weftos://` name. WeftOS does the rest: mounting, the fixed mount path, the uid map, branching per worktree, composition per job, publication, the integrator and audit.

## Upstream track (runs alongside the proofs)

Each contribution makes a proof cleaner. None blocks a proof, because each has a WeftOS-side fallback.

| Id | Upstream | What it changes | Unblocks | Fallback until it lands |
|---|---|---|---|---|
| U1 | ruvnet/RuVector **#1287**: real SHAKE-256 (`rvf_crypto::hash::shake256_256` and `compute_manifest_hash`) in place of `simple_shake256_256`, plus a byte compare before compaction drops a cluster, while still reading legacy files | lineage becomes tamper-evident, and compaction can't lose data on a collision | T4 and T5 can trust RVF lineage; safe compaction in B1 | Our own SHAKE-256 layer names and signatures (§4, §7). Never run `compact()` on tree layers |
| U2 | ruvnet/RuVector **#1288** (merges after #1287): a membership filter sized by the visible count, not the maximum id, with the old dense format still readable | non-dense ids work | path-hash ids in T2 | dense ordinals plus a path-to-ordinal map |
| U3 | ruvnet/RuVector **#1289**: multi-parent derivation (`DerivationType::Merge`); `derive()` honours its type and records it in lineage | native composition | T3 without our walk | the WeftOS `compose` walk in `clawft-cow-memory` |
| U4 | ruvnet/RuVector **#1290**: a typed custom-segment API for vendor types, inherited through branch, hashed and signed | tree records as their own segment type | T2's option (b) | `META_SEG` key/value plus per-record metadata |
| U5 | ruvnet/RuVector (to file): `branch()` of a copy-on-write child builds its membership filter from local vectors only, so inherited records vanish or fail to reopen at depth 2 and deeper (repro in the T2 spike, `examples/depth2.rs`) | nested branches keep their inherited records | T2 at depth, B1 | rebuild the membership filter after `branch()` in WeftOS |
| R1 | rustfs/rustfs **#9067 merged** 2026-10-30 (multipart and bucket-config audit, and it included #9066's queued-replay fix; #9066 closed as superseded), and **#9073** (denial audit, open) | complete audit of store operations | ADR-126 governance for the stores under T1 and T4 | tail `rustfs.log` for `s3_authorization_denied` |

A scheduled check every 2 hours watches these: it fixes CI failures, rebases, answers concrete review requests, and brings design questions to the owner.

## Prove first (P0, each with a pass bar)

| # | Proof | Depends on | Pass when |
|---|---|---|---|
| T0 | **Move WeftOS from rvf-runtime 0.3 to 0.5** (rvf-types 0.3); switch `clawft-cow-memory` from `derive()` to `branch()`; confirm upstream `rvf-crypto` and `weftos-rvf-crypto` coexist | none | the workspace builds; `clawft-cow-memory`, `clawft-versions` and `clawft-core --features rvf` tests pass; the vector layer-walk is removed and native read-through gives the same results on the existing suite **Result, 2026-10-30: partial pass.** `clawft-cow-memory` and `clawft-core` (rvf) run on rvf-runtime 0.5 / rvf-types 0.3 and the whole workspace checks; cow-memory, clawft-versions and clawft-core tests pass (4 pre-existing chain-event failures in clawft-core are identical on the base). The workspace pins stay at 0.3 / 0.2 because `clawft-kernel` hands rvf-types values to `weftos-rvf-crypto` / `weftos-rvf-wire`, which still depend on rvf-types 0.2 (upstream `rvf-crypto` 0.3.0 itself coexists: different package and lib name); the forks need rebasing onto rvf-types 0.3. `branch()` read-through is one level only (the membership filter holds the parent's own vectors), so the walk is removed only for the working-over-base shape and kept for checkpoint chains and fork sources. `branch()` also needs dense ids: digest-derived ids (`clawft-versions`) fall back to `derive`, and deep COW chains overflow the stack, so COW depth is held at one. See commit in the T0 note on card 4df0d7b0. **Finished 2026-10-30 by ADR-129:** the forks are replaced by upstream `rvf-crypto` 0.3 / `rvf-wire` 0.2, the workspace pins are rvf-runtime 0.5 / rvf-types 0.3 with the per-crate overrides removed, the lockfile holds one rvf-types, and clawft-kernel (3810 tests), clawft-core `rvf` (1756 under nextest), clawft-cow-memory and clawft-versions pass. Under `cargo test --lib`, two clawft-core session chain-event tests still fail, as on base; they do not touch signing. |
| T1 | **Shared POSIX tree over RustFS** (the JuiceFS spike, `docs/research/juicefs-spike-2026-10-29.md`). **Partial result, 2026-10-30:** git and cargo are correct on the mount; cross-machine names appear in 0.3-0.5 s, renames in 0.3 s, and flock works; warm `git status` inside the VM is 0.11 s (WeftOS) and 0.73 s (Linux kernel), within the bar. Over the Mac's NFS it fails (26.5 s), and with metadata on PG at 56 ms RTT it is unusable (~0.84 s per create). Metadata round trips are the bottleneck, which supports §4's local published metadata. The stress phase, role split and kill-recovery were not run | R1 for full audit (not for the pass) | git and cargo are correct on the mount; `git fsck` and `juicefs fsck` are clean after the stress phase; cross-node visibility is about 2 s or less; a killed mount recovers; warm `git status` on the 90k-file tree is within 5x of local disk; same-path, uid and watcher behaviour is recorded |
| T2 | **PASS 2026-10-30** (`docs/research/adr128-t2-tree-metadata-2026-10-30.md`: one record per entry, dense ordinals, in-memory chained dentry maps; on 103,658 real entries, 1.2-1.4M lookups/s at 1 layer and 0.76-0.96M/s at 11; branch 35-44 ms including the metadata copy; 1k-change promote 270-360 ms; 600 property cases to 14 layers. Deferred cost: a child's first commit writes a full snapshot, 380-450 ms at 90k. New upstream bug U5: `branch()` of a copy-on-write child drops inherited records from depth 2). **Keyed tree metadata in an RVF lineage,** with dense ordinals and a path-to-ordinal map in key/value metadata | T0. Optional: U2 (path-hash ids), U4 (a custom segment) | Property tests match a reference in-memory filesystem model (create, write, rename, unlink, readdir) across branch, checkpoint, rollback and promote. On a laptop: 100k or more lookups a second; branching a 90k-entry tree in under 50 ms, **including the per-child metadata copy**, which must be measured; a 10-layer lookup at most 2x a 1-layer one; promoting a 1k-change branch in under 1 s. If the per-child copy fails the branch bar, record it and move tree records to per-record metadata or U4 |
| T3 | **PASS 2026-10-30** (`docs/research/adr128-t3-compose-2026-10-30.md`: compose of 5 layers over a 103,658-entry base in ~0.5 us median, 9 us max; base index built once in 84 ms; view name identical across 3 processes and paths; 3,000 random stacks match an overlay model for lookup, readdir, diff, kv and vector search. Lookup through 5 layers is 691 ns vs 201 ns for 1 (3.4x); U3 not required). **`compose`**: one view from 5 independent layers | T0. U3 replaces our walk later | precedence is deterministic and documented per record type; the composed view's name is identical on two nodes; composing takes under 100 ms |
| T4 | **PASS (local, 2 processes) 2026-10-30** (`docs/research/adr128-t4-publication-2026-10-30.md`: announce-to-installed 2.5-3 ms at 64 KiB, ~28 ms at 1 MiB, ~850 ms at 32 MiB, within the 2 s bar; byte-flip, truncation, swapped object, forged, spliced or truncated signatures, rogue and unpinned keys all rejected with an empty cache; account B never fetched A's layers; 17 fail-closed ACL cases deny. Real: SHAKE-256, Ed25519 plus ML-DSA-65 via pqcrypto-mldsa. Simulated: file bus, directory store. SHAKE-256 at ~40 MB/s dominates, so B2 keeps layers under ~16 MiB or names them over chunk hashes. Found: `weftos-rvf-crypto`'s ML-DSA-65 is an HMAC placeholder, see the security card). **Publication**: sealed layers named by SHAKE-256 and signed (Ed25519 plus ML-DSA-65), announced on the substrate, fetched from RustFS, with ADR-057 access | T0. U1 for the RVF lineage cross-check | a second node holds a new layer within 2 s; a tampered layer is rejected; a node outside a client's boundary never receives a client layer (must fail closed) |
| T5 | **PASS 2026-10-30** (`docs/research/adr128-t5-integrator-2026-10-30.md`: SHAKE-256 sealed layers, DAG order with a (depth, name) tie-break, git `merge-tree --write-tree` with no working tree; 4 from-scratch runs incl. a different path give identical commit and tree hashes; a sibling conflict is recorded identically and leaves the target unchanged; 24 random DAGs are arrival-order independent. Production needs: a layer registry, a closed batch cut, a merge-commit policy, a real second-node run). **The git integrator:** two sibling branches plus one stacked branch | T2, T4 | merge order follows the layer DAG, with the stacked branch never first; the same inputs give identical commit and tree hashes in 3 runs on 2 nodes; a same-line sibling conflict is detected the same way every time |
| T6 | **Agent memory priming:** compose a role pack plus project memory, using RVF HNSW, for a real task | T3 | the agent retrieves the expected facts, and the same composition gives the same retrieval on two nodes |

### Gates

- **G1, adopt the direction:** T0, T1, T2 and T5 pass.
  - **If T2 fails on the per-child metadata copy or on speed:** retry with tree records as per-record metadata, or as a custom segment once U4 exists. RVF stays the carrier. Revisit this ADR only if both fail.
  - **If T1 fails on semantics:** revisit the shared-tree route before building anything else.
- **G2, choose the mount route,** from the T1 and T2 numbers:
  - (a) JuiceFS, with a metadata adapter over RVF layers; or
  - (b) a WeftOS filesystem: FUSE metadata from RVF layers, data chunks in RustFS.
- **Later dependencies:** T3, T4 and T6 pass before the phases that depend on them.

## Plan

| Step | Work | Runs with | Exit |
|---|---|---|---|
| 1 | T0: upgrade rvf to 0.5 and switch to `branch()` | U1-U4 opening upstream; the T1 spike finishing | T0 passes |
| 2 | T1 report | T0 | T1 verdict and numbers |
| 3 | T2 prototype (`weftos-tree` spike crate) with the property-test harness and benches | U1 and U2 under review | T2 verdict |
| 4 | T4 publication spike: the layer naming, signing and substrate announce/fetch path | T2 | T4 verdict |
| 5 | T5 integrator spike over T2 trees | T4 | T5 verdict, then **G1** |
| 6 | T3 and T6 | after G1 | both verdicts |
| 7 | **G2,** then build out B1 to B6 | upstream merges are folded in as they land (each fallback in the upstream track is retired) | B6 done |
| 8 | Adopt: shadow, then cutover (below) | | |

## Build out, after G1

| Phase | Builds | Done when |
|---|---|---|
| B1 | A `weftos-tree` crate: the tree schema on RVF 0.5 (T2's winner), lineage operations, a `compose` op in `clawft-cow-memory`, and no `compact()` on tree layers until U1 | the T2 and T3 property suites run in `scripts/build.sh test` |
| B2 | Layer publication: SHAKE-256 names, the dual-signed envelope, substrate announcements, ADR-057 filtering | the T4 negative test is in the gate |
| B3 | The mount route chosen at G2: a fixed mount path, a uid map, and change notifications from substrate events | git and cargo run on a mounted branch on the Mac, the CI host and PG |
| B4 | The integrator: git commits from diffs, DAG-ordered asynchronous merges, the tie-break, push and the hand-off to CI | the T5 suite runs in CI; agent-git-flow's land path can call it |
| B5 | Per-job composition in CI: ADR-127 runners mount the job's composed stack, and promote or discard the tip | a CI job runs on a composed view with no checkout and no cache download |
| B6 | Agent memory priming through composed layers (ADR-121 instances) | a lane's agent starts from a composed pack, and its tip's lessons promote on landing |

## Adopt

1. **Shadow on WeftOS:** one lane class runs both a git worktree and a layered-tree branch for two weeks. Adopt when the integrator's merges are identical and nothing is lost.
2. **Cut over WeftOS:** layered trees become the default for lanes, and agent-git-flow worktrees become the fallback.
3. **Client projects:** each with its own account, layers, store and metadata owner (§8). The owner picks the first client.

## Consequences

- **What it gains:**
  - no copies and no cold worktrees;
  - roles on different nodes over one tree;
  - deterministic, asynchronous, dependency-ordered merging;
  - one carrier for metadata and memory;
  - replayable job environments.
- **What it costs:** a real build (B1-B6) and a correctness-critical tree-metadata component, gated by T2's property tests. It also means tracking RVF releases: we are on 0.3, and 0.5 is current.
- **Upstream work** fixes real RVF defects along the way: the hash fold and its compaction data-loss risk, the dense-id limit, the derivation type being ignored.
- **Git stays** the interoperability format, so nothing outside WeftOS changes.

## Open questions for the owner

1. **The commit granularity the integrator writes:** one commit per landing, or one per checkpoint?
2. **The first client** to adopt layered trees, after WeftOS.
