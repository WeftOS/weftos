# ADR-128: RVF layered trees: copy-on-write branch metadata and agent memory, composed per job, with git kept as the integrator

- **Status**: Proposed (2026-10-29; owner direction, see Context). Adoption is gated on the proofs in "Prove first".
- **Deciders**: owner
- **Prior decisions**: the decision store, consulted on 2026-10-29, holds ADR-126 and ADR-127 on this topic.
- **Builds on**:
  - ADR-126: the mesh object store on RustFS;
  - ADR-127: WeftOS-orchestrated CI;
  - ADR-017: the substrate state tree;
  - ADR-057: per-path read access rules;
  - ADR-063: signed envelopes;
  - ADR-089: ExoChain DAG merge;
  - ADR-099: placement;
  - ADR-121: versions and instances;
  - `crates/clawft-cow-memory`: BranchableMemory over RVF.
- **Changes**: ADR-126 §4 (the worktree overlay) and ADR-127 §3 (caches) are re-expressed as RVF layers. Both get an Updated note pointing here.

## Context

The owner's direction on 2026-10-29, in his words where possible:

- **No copies.** "A copy from remote to local should never need happen." A checked-out tree is worked on in place, from any node.
- **Roles on different nodes.** "Git may not need to be done by the same node that runs build or the same one that runs the tests."
- **Branching storage.** A writer branches, and the branch holds only what it changed, while "most of its files [are] still in parent".
- **Metadata as RVF.** "It should be able to create a CoW rvf that is the metadata that gets published and maintained with each branch."
- **Agent memory too.** "This is also how we prime memory for each agent, rvf should be the carrier on this stuff because we can actually compose them in the ci/cd for each specific env/job."
- **Git stays.** "We dont get rid of git, keep it there. Then use it to sort it out. The agent doing the work, the reviewer, the build process, etc.. they do not care."
- **Ordered merging.** "Almost deterministic merging async ... Dependency is enforced by the CoW for the purpose of merge order."
- **WeftOS wraps it.** "weftos can wrap both very easily", meaning the shared filesystem and the branching buckets.

What exists today, checked in source on 2026-10-29:

| Piece | State |
|---|---|
| `clawft-cow-memory` `BranchableMemory` | `branch`, `fork`, `checkpoint`, `rollback`, `promote`, `diff`, `lineage`. Each layer is its own `.rvf` file. The manifest holds a writable tip, its checkpoints, a base, and an ordered `inherited: Vec<ManifestNode>`. `query` walks newest to oldest, because `RvfStore::query` doesn't read through the copy-on-write boundary. Inheritance comes from a **single** branch or fork source. |
| `rvf-runtime` 0.3.2 | **Vector-addressed.** `ingest_batch(vectors, ids, metadata)`, where each record needs a vector and an id and may carry typed fields (`u16` field id → U64, I64, F64, String or Bytes). Lookup is a vector query or a field `FilterExpr`; there's no keyed `get(id)` in the public API. Copy-on-write works per vector cluster (`cow.rs`: ParentRef chains, `freeze(epoch)`). The store also carries opaque payload segments (`embed_kernel`, `embed_wasm`, `embed_ebpf`, `embed_dashboard`). |
| Substrate | A state tree with put, get, subscribe and adapters (ADR-017), per-path read access (ADR-057, Accepted), signed messages (ADR-063), and anti-entropy reconciliation (ADR-064, Proposed). It's eventually consistent, not transactional. |
| RustFS on PG | Spike S0 passed (ADR-126): IAM, revocation, STS, audit (with known gaps, fixed upstream in rustfs/rustfs#9066 and #9067), and the xl.meta format. |
| JuiceFS over RustFS | A spike is running across PG, the CI host, the Mac's VM and the Venq; results go to `docs/research/juicefs-spike-2026-10-29.md`. |

The ruvnet-brain knowledge base was unavailable when this was written. The RVF claims above come from the crate source in the cargo registry, not from the brain.

## Decision

### 1. A tree is a stack of RVF layers

- **A working tree's metadata** (paths, entries, attributes, chunk references) is an RVF lineage: the project base, then the worktree branch, then any sub-branches, then a writable tip.
- **A branch stores only what it changed,** and reads fall through to the parent.
- **File contents are chunks in RustFS,** addressed by content hash, so unchanged files are shared by every branch at no cost.

### 2. RVF is also the carrier for agent memory

- Agent knowledge (project facts, role packs, lessons, conventions) is RVF layers too, with the same branch, checkpoint, rollback, promote and diff.
- **Priming an agent means choosing layers,** not assembling prompt text.

### 3. CI composes a layer stack per job

- **A job's view, top to bottom:**
  1. the job tip, writable;
  2. the job spec;
  3. the agent or role pack;
  4. the environment (toolchain, OS, node facts);
  5. the worktree branch;
  6. the project base.
- **Every layer is identified by content hash and may be signed,** so a job's exact files and memory are recorded and replayable.
- **After the job,** its tip is promoted (results and lessons flow up) or discarded.

### 4. Publication through the substrate

- **Sealed layers are immutable and content-addressed.** Publishing one means announcing its hash and parent hash over the substrate.
- **Access:** ADR-057 rules decide which nodes and agents receive which layers. Client layers go only to that client's boundary.
- **Fetching:** nodes fetch layers from RustFS and keep them locally, so lookups and `git status` read local metadata. Only writes go to the tip's owner.

### 5. One writer per tip, roles on separate branches

- **Each writable tip has exactly one owning node.** Different roles get their own sub-branches. For example, git work on the tree branch, build outputs in a build branch, test results in a test branch.
- **No two nodes write the same tip,** so no distributed lock is needed on the hot path.

### 6. Git stays, as the integrator

- **The tree keeps a real `.git`.** Workers (the agent doing the work, the reviewer, the build and test jobs) see only files and don't use git.
- **A WeftOS integrator role owns git:** it turns a branch's diff into commits, merges with git's three-way merge, detects and resolves or flags conflicts, rebases onto upstream, pushes, and hands off to CI.
- **Git stays the publishing and interoperability format:** GitHub, PR review, upstream PRs and release history.

### 7. Merge order comes from the copy-on-write DAG

- **A branch's parent hash is a dependency edge,** so the integrator merges in topological order of the layer graph. A branch is never merged before the layer it was based on.
- **Merging is asynchronous:** a branch becomes eligible as soon as its dependencies have landed. There's no shared landing queue.
- **Merging is deterministic:** git's three-way merge of known base, branch and target hashes gives the same result on any node, and can be replayed.
- **The remaining cases:**
  - sibling branches that touch the same lines are detected deterministically and resolved with a fixed tie-break (DAG order, then branch id) or by the branch's agent;
  - semantic conflicts are caught by CI on the merged result.

  This mirrors ExoChain's DAG merge rules (ADR-089).

### 8. Privacy and isolation

- **File names, paths and structure are confidential for client work,** even when contents are encrypted. So each account (WeftOS, and each client) has its own layers, its own RustFS store and its own metadata owner, running as that account's user (the PG isolation rules).
- **Nothing published across the mesh** (activity log, health facts, store names) carries client paths or file names outside that client's boundary.

### 9. WeftOS wraps it

Agents and scripts see a directory, or a `weftos://` name (ADR-114 and ADR-126). WeftOS does the rest: mounting, the fixed mount path, the uid mapping, branching per worktree, composition per job, publication, the integrator, and audit.

## Prove first (P0 spikes, each with a pass bar)

| # | Proof | Pass when |
|---|---|---|
| T1 | **Shared POSIX tree over RustFS** (the JuiceFS spike, running) | git and cargo behave correctly on the mount, with no corruption under the stress phase (`git fsck`, `juicefs fsck` clean); a write on one node is visible on another within about 2 s; a killed mount recovers; warm `git status` on the 90k-file tree is within 5x of local disk |
| T2 | **Keyed tree metadata in an RVF lineage.** Encode entries (path → attributes, chunk references, directory listing) either as RVF records (an id from a path hash, a minimal vector, Bytes fields) or as a WeftOS-defined keyed segment inside RVF, using its copy-on-write, freeze and witness | Property tests match a reference in-memory filesystem model (create, write, rename, unlink, readdir) across branch, checkpoint, rollback and promote. On a laptop: 100k or more local lookups a second; a branch of a 90k-entry tree in under 50 ms; a 10-layer-deep lookup at most 2x a 1-layer one; promoting a 1k-change branch in under 1 s |
| T3 | **`compose`**: build one view from 5 independent layers (project, branch, environment, agent pack, job) | Precedence is deterministic and documented per record type. The composed view's hash is identical on two nodes. Composing takes under 100 ms |
| T4 | **Publication**: announce sealed layer hashes on the substrate, fetch from RustFS, ADR-057 access | A second node has a new layer within 2 s. A node outside the client's boundary never receives a client layer, a negative test that must fail closed |
| T5 | **The git integrator**: two sibling branches plus one branch stacked on another | Merge order follows the DAG, and the stacked branch is never merged first. The same inputs give identical commit and tree hashes in 3 runs on 2 nodes. A same-line sibling conflict is detected the same way every time |
| T6 | **Agent memory priming**: compose a role pack plus project memory for a real task | The agent retrieves the expected facts. The same composition gives the same retrieval results on two nodes |

**Gate G1, adopt the direction:** T1, T2 and T5 pass.
- **If T2 fails** on keyed lookups or performance, retry with the WeftOS-defined keyed segment carried inside RVF. RVF stays the carrier; the keyed structure is ours. Only if that also fails is this ADR revisited.
- **If T1 fails** on semantics, the shared-tree route is revisited before anything else is built.

**Gate G2, choose the mount route,** from the T1 and T2 numbers:
- (a) JuiceFS with a metadata adapter backed by RVF layers; or
- (b) a WeftOS filesystem: FUSE metadata from RVF layers, data chunks in RustFS.

T3, T4 and T6 must pass before the phases that depend on them.

## Build out, after G1

| Phase | Builds | Done when |
|---|---|---|
| B1 | A `weftos-tree` crate: the tree-metadata schema in RVF (T2's winner), the lineage operations, and a `compose` op in `clawft-cow-memory` (multi-source `inherited`) | the T2 and T3 property suites run in `scripts/build.sh test` |
| B2 | Layer publication: substrate announcements, fetch from the store, ADR-057 filtering, signing | the T4 negative test is in the gate |
| B3 | The mount route chosen at G2, with a fixed mount path, a uid map, and change notifications from substrate events (closing the cross-node watcher gap) | git and cargo run on a mounted branch on the Mac, the CI host and PG |
| B4 | The integrator role: git commits from diffs, DAG-ordered asynchronous merges, the conflict tie-break, push and hand-off to CI | the T5 suite runs in CI; agent-git-flow's land path can call it |
| B5 | Per-job composition in CI: ADR-127 runners mount the job's composed stack, and promote or discard the tip | a CI job runs on a composed view with no checkout and no cache download |
| B6 | Agent memory priming through composed layers (ADR-121 instances) | a lane's agent starts from a composed pack, and its tip's lessons promote to the project layer on landing |

## Adopt

1. **Shadow on WeftOS itself:** one lane class runs both a git worktree and a layered-tree branch for the same work. Builds, tests and the integrator's merges are compared over two weeks. Adopt when the merges are identical and nothing is lost.
2. **Cut over WeftOS:** layered trees become the default for lanes, and agent-git-flow worktrees become the fallback.
3. **Client projects:** each client gets its own account, layers, store and metadata owner (§8). The first client is the owner's decision.

## Consequences

- **What it gains:**
  - no copies and no cold worktrees;
  - git, build and test can run on different nodes over one tree;
  - deterministic, asynchronous, dependency-ordered merging;
  - one carrier for files and memory;
  - replayable job environments.
- **What it costs:** a real build (B1-B6), and a new correctness-critical component, the tree metadata. It's gated by T2's property tests.
- **Dependencies:** RVF's API stability (rvf-runtime 0.3), and RustFS (ADR-126).
- **Git stays** the interoperability format, so nothing outside WeftOS (GitHub, CI triggers, reviewers, upstream) changes.

## Open questions for the owner

1. **The commit granularity the integrator writes:** one commit per landing, or one per checkpoint?
2. **The first client** to adopt layered trees, after WeftOS.
