# ADR-126: Mesh object store: WeftOS-governed buckets for nodes, projects, worktrees and releases

- **Status**: Accepted (owner, 2026-11-18 09:40 ET; proposed 2026-10-29). S1 to S5 ship in 0.8.5. Acceptance does not pass any WO-069 §4a gate item: shared worktrees are still not cleared for client projects (items 1, 2, 5 and 7 are open)
- **Updated**: 2026-11-18 (status: Accepted on the owner's word, 09:40 ET; no gate item is passed)
- **Updated**: 2026-11-18 (status line only: states that S1 to S5 are implemented and ship in 0.8.5; the ADR stays Proposed and no gate item is passed)
- **Updated**: 2026-11-16 (card 5e5b5d23, branch `w2/card-5e5b5d23`, local, not pushed: follow-up to card 6f0bc9c8 after the owner answered Q1 to Q4; `gc` for a project admin, `store.view.*` and `store.drive.*` under the same caller check, review r2 Lows N1 to N4. Under the owner's model several roles may share one OS user, so the peer uid cannot separate them and D1-1 and D1-2 stay open; what could separate them is an owner question in "S2 status". No gate item is passed)
- **Updated**: 2026-11-16 (card 6f0bc9c8, branch `w2/card-6f0bc9c8`, local, not pushed: verified caller identity for the `store.worktree.*` verbs and `store grant`, so several principals can share one daemon; design in "S2 status"; drill findings D1-1 and D1-2 close only under one OS uid per principal, which has not been shown end to end. Off by default: principal mode needs `WEFTOS_STORE_PRINCIPALS=1` and a vetted `store-principals.toml`, otherwise one daemon is still one trust domain. No gate item is passed)
- **Updated**: 2026-11-16 (WO-069 §4a gate: item 8 marked passed on the owner's word, 21:18 ET, after the installed daemons on the MacBook, aepod-xpc and the production host's hub were replaced with a local build of `0992ade32`; items 1, 2, 5 and 7 are not passed)
- **Updated**: 2026-11-15 (WO-069: lanes A, B, E, F and the fixes from the §4a drills are together on the local branch `w2/wo069-worktree-mirror`, not on `target-0.8.5`, not pushed; drill evidence and open owner questions added to the gate in §4a; no gate item is passed)
- **Updated**: 2026-10-29 (§4's worktree overlay is re-expressed as RVF layers and branch metadata in ADR-128)
- **Updated**: 2026-10-29 (S0 spike applied on PG and run; results in "S0 results"; open question 1 answered: import, not in place; §6 amended: the activity log must take RustFS's own log as well as its audit stream)
- **Updated**: 2026-10-30 (ADR-130 adds a GCS seed bucket as the tier above this store; this store becomes the optional L1 staging tier)
- **Updated**: 2026-11-07 (follower view §4a, Windows-safe fetch, notification/poll answers from ADR-131; S2 implemented on `feat/adr126-s2`, not yet landed: overlay, promotion, agent-git-flow hooks, union mount; §4 amended by the owner extension, board card f40828fb: the worktree tree itself is shared, so unmerged docs stay visible and another agent can merge)
- **Updated**: 2026-11-07 (S1 built on `feat/adr126-s1`: `weftos-store` crate, `weaver store`, daemon wiring, local-directory backend; RustFS S3 + admin transport added and verified against a throwaway RustFS 1.0.4, audit-stream and notification parsers fixed from captured records, `put_if_match`, release-store `append`/`sweep` roles; see "S1 status")
- **Updated**: 2026-11-08 (chain compaction proposal and grant-ledger spike; review fixes and handoffs). Earlier: 2026-11-07 (live run against RustFS 1.0.4 in a loopback container: results in "S3 results"; edge presign fixed)
- **Updated**: 2026-11-07 (card b76c74a7: live STS minter and chain-fed grant ledger added to `weftos-edge-creds`, still tested against a mock store)
- **Updated**: 2026-11-07 (S3 implemented in `crates/weftos-edge-creds` against a mock store; see "S3 results". The ADR stays Proposed: S1, S2, S4, S5 are not landed)
- **Updated**: 2026-11-07 (S5 gate and per-account instance spec in `crates/clawft-weave/src/store_account.rs`; wiring to `weftos-store` waits on S1)
- **Updated**: 2026-11-07 (S5 gate and per-account instance spec in `crates/clawft-weave/src/store_account.rs`; `client_store.rs` puts the gate in front of S1's `StoreService`, tested on local directories, not yet on RustFS)
- **Updated**: 2026-11-08 (S5 enforces one RustFS instance per client account: a marker bucket names the owner and a second account is refused with a clear error; on a client daemon the client store rides the daemon's own S1 service when it is on the same instance; see "S5 status")
- **Updated**: 2026-11-08 (S5 on S1's RustFS transport: `ClientStore::open_rustfs`, config backend `rustfs`; the S5 isolation, WeftOS-account and cross-mesh tests pass against two throwaway loopback RustFS 1.0.4 instances; see "S5 status")
- **Updated**: 2026-11-08 (S5: cross-mesh grant signatures use `verify_strict`; gate snapshots follow chain key rotations; the compacted segment can be archived and checked against the signed snapshot before the owner prunes it: `store.client.archive`, `archive_verify`)
- **Updated**: 2026-11-08 (S5 adopts S3's chain compaction for the gate: signed `store.snapshot` events plus tail, restore from the verified snapshot that covers the most, `store.client.snapshot` verb and `weaver store client snapshot`, a sweeper compacts every 256 changes; see "S5 status")
- **Updated**: 2026-11-08 (S5 gate state is rebuilt from the daemon's chain: every gate change is a `store.client.gate.*` event and open replays them; `gate.json` remains only as the no-chain fallback; `weaver store client xmesh-sign` produces a cross-mesh grant file; see "S5 status")
- **Updated**: 2026-11-08 (S5 daemon wiring in code only: a daemon running as a client user opens that account's `ClientStore` when `<runtime dir>/client-store.json` names it (`client_store_rt.rs`); `store.client.*` verbs and `weaver store client` (`client_store_rpc.rs`, `commands/store_client_cmd.rs`); gate state persisted in `gate.json` under the client root; not installed or run on any machine, local directories only)
- **Updated**: 2026-11-07 (S1 built on `feat/adr126-s1`: `weftos-store` crate, `weaver store`, daemon wiring, local-directory backend; no RustFS backend or S3 transport yet, see "S1 status")
- **Updated**: 2026-11-09 (S1 to S5 landed on `target-0.8.5`, origin tip e58899bb0: S1 merge 68e010acf with the Mac slices and approvals d26c0cbec (15dcf72c3); S2 land 7ba34b071; S5 on it, 64c547f4a (S3 came in through S5's merges 96c78e454 and 3fca8c95c, nothing of S3 left to land); S4 rebased onto 64c547f4a as 3d4d52834 with the owner's approvals e58899bb0. ADR stays Proposed. Still open: snapshot cadence and location ("Chain compaction" switch 2), `prep/adr126-s3-reqwest-client` (needs a reqwest diff-gate approval), `opt/adr126-nodekeys-adapter`; see "Phase status")
- **Updated**: 2026-11-09 19:44 ET (owner decision: store-capable weavers are OK to install. The install stop on store-capable weavers is lifted; store support stays unconditional in `clawft-weave` and is not to be made optional. Reason: at `origin/target-0.8.5` 88b2fb172 no weaver can be built without the store code. Nothing installed yet; no push)
- **Updated**: 2026-11-10 (Tapestry E3 / item 10 ran against the landed `weaver store view` verbs on `weftos-ci` with a throwaway daemon and a local-directory store: the interface matches with no functional mismatch, but the store keeps no file modes and the view writes base files 0444, so a root image has nothing executable and bwrap cannot exec `/bin/sh`; new open item: preserve or declare the exec bit. Evidence in agent-notes `scratch/p3-e3/results.md`)
- **Updated**: 2026-11-10 17:08 ET (owner answer on the E3 exec bit: record each file's mode in the object metadata, not a view-spec list; base files keep write bits off and exec bits preserved; implemented by the Tapestry lead on `feat/store-file-mode` off 88b2fb172, local only; E3 to be rerun without the chmod workaround)
- **Updated**: 2026-11-10 (object attributes, on WeftOS branch `feat/store-file-mode` 367bed4e7 off 88b2fb172; landed on target-0.8.5 as merge 72a98d05c under ADR-131 Accepted 2026-11-10; pushed by the owner 2026-11-10 ~20:11 ET, `origin/target-0.8.5` = 7311b9443, fast-forward, no tag. `ObjectBackend` gains `put_attrs`, `put_if_match_attrs` and `attrs`; the first attribute is `executable` (RustFS: `x-amz-meta-weft-exec`; directory backend: the file's execute bits). The drive records it in the index entry, sync_up puts it, materialise applies it, and the scoped view gives base 0555/0444 and work 0755/0644. See `docs/architecture/store-scoped-view.md`, "File modes". E3 reruns without the chmod workaround and passes; no diff-gate approvals needed)
- **Updated**: 2026-11-10 evening (Tapestry E3 round two against a weaver from local target-0.8.5 7311b9443: overlay, conflict/resolve, refresh, lease expiry/takeover and the Landlock proxy all pass without the chmod workaround. New open items for the verbs owner: overlay `lower/` is materialised 0444 and copy-up keeps the mode, so in-place edits need `chmod u+w` (owner question pending; `lower/` should carry the stored mode, only `base/` read-only); `weaver store put` lacks `--as <principal>`; status JSON `work.open_conflicts` is empty while the top-level list is filled. All three closed since: `lower/` writable in 431e3060c, `--as` required in 54aaa95aa, the status JSON item was a mis-read of `includes[0].open_conflicts`; all landed in c00fe351b)
- **Updated**: 2026-11-10 ~20:11 ET (owner pushed `target-0.8.5` 7311b9443 to origin, fast-forward from 88b2fb172, no tag: the exec-bit store attribute and the ADR-126 docs catch-up are now on origin. Owner approved writable overlay `lower/` (stored mode on `lower/`, only `base/` read-only); the Tapestry lead implements it on `feat/store-overlay-lower-writable` off 7311b9443; delivered as 431e3060c and landed on target-0.8.5 in c00fe351b, 2026-11-10 ~22:40 ET)
- **Updated**: 2026-11-10 (overlay lower, branch `feat/store-overlay-lower-writable` 431e3060c off 7311b9443, landed in c00fe351b): in overlay mode the scoped view's `lower/` is materialised writable (dirs 0755, files 0644, executables 0755), not read-only. Overlayfs copy-up keeps the lower's mode, so a read-only lower made in-place edits of existing files fail with Permission denied. `base/` stays read-only. The lower is never written by the session (overlayfs writes only to the upper); on the host it is hard-linked to the work drive, and scan/repair still detect content and execute-bit tampering. See `docs/architecture/store-scoped-view.md`, "File modes"
- **Updated**: 2026-11-10 (store verbs, pile B, WeftOS branch `feat/store-verbs-polish` 54aaa95aa off 7311b9443, landed in c00fe351b via 7719ea4c5 after 1e6b9d2ab removed the `$WEAVER_STORE_AS` fallback, so the principal comes only from `--as`): `store.view.*` refusals carry `error_kind` `path_refused` or `invalid_params`. The `limits` field has CLI flags `--max-file`, `--max-files` and `--max-total`. The bucket is always the one `--name` binds to. `weaver store put|get` take the principal from `--as` only, under the same grant gate (the branch first had a `$WEAVER_STORE_AS` fallback, flagged to the owner as an ambient default; **owner 2026-11-10 22:02 ET: not approved**; removed in 1e6b9d2ab before the landing, no such string left under `crates/`)
- **Updated**: 2026-11-10, feat/store-attrs-followups (off 431e3060c, local): the execute attribute now reaches (a) refresh, through `list_stamped` modification stamps recorded in the index so a mode-only change costs one HEAD for the changed object and none for the rest; (b) conflict remote copies and `Keep::Remote` (`remote_exec`); (c) locked puts, multipart create and put_stream (`PutOpts.attrs`). Checked live on RustFS 1.0.4: the header round-trips on every put path, CopyObject keeps it, LastModified moves on a mode-only put. The E3b 'work.open_conflicts empty' note was a mis-read of a grep (no top-level key exists). Not covered: macOS/APFS; a mode-only put inside the same instant as the previous put (stamp resolution). (Pile A, Tapestry lead; branch tip d989f426d, 4 commits, landed in c00fe351b; diff gate matched by an existing approval, no new approvals)
- **Updated**: 2026-11-10 night (Tapestry pile C, fork `tapestry/p3-item10-real-view` 467968ba: the E3 overlay case now covers a new file in an existing lower directory plus a nested edit through real overlayfs copy-up, and the chmod fallback is gone; all 6 round-two cases pass on a weaver from `feat/store-overlay-lower-writable` 431e3060c and the new case fails on 7311b9443, so the overlay `lower/` fix is required for item 10. Also: the view's `lower/` must-be-writable finding and the exec-attribute follow-ups (piles A, B) remain local branches awaiting the owner's merge call)
- **Updated**: 2026-11-10 22:02 ET (owner: client names leave public WeftOS docs going forward; the two mentions in this ADR's shared-worktree gate section now read "a client project" and "a client-scale project"; history untouched; the leak denylist already carries the name)
- **Updated**: 2026-11-10 ~22:40 ET (owner OK 22:20: pushed to origin as c00fe351b, fast-forward, no tag. Now on origin: the overlay `lower/` writable view, the exec-attribute follow-ups (refresh stamps, conflict copies, locked/multipart/stream puts, live RustFS round trip), the store verb polish without any ambient principal fallback, the test-health fixes, the client-name scrub and this ADR's catch-up. Earlier 2026-11-10 Updated lines that said "not landed" are superseded by this one)
- **Updated**: 2026-11-11 (pile T, `w2/t-names-and-conflicts` 9d90ee210, local, four commits on 431e3060c, merges clean onto origin: the names slice is rebased onto the landed store with a copy-forward migration, `adopt_legacy_node_bucket`: on `ensure_store` an `n-<id>` bucket carrying this store's kind and owner tags is copied into `node-<id>`, a `store.bucket_migrate` event is recorded, the old bucket is never changed or deleted, deleted keys do not return, a foreign `n-<id>` is left alone; `weftos-edge-creds` derives `node-<id>`, the node id cap is 58, `node-` archives keep their kind. weftos-store 369 passed and weftos-edge-creds 99 passed on weftos-ci; the clawft-weave `store_rt` slice was not run (xpc disk). The owner call on the bucket rename and the pinned creator stays open; this ADR's wording is reconciled so that the 2026-11-08 decision covers RustFS user names and one creator per user only. `origin/target-0.8.5` = 9523587e1, pushed by the Tapestry lead 2026-11-10 ~23:57 ET on the owner's OK through the Grok relay, fast-forward from c00fe351b, no tag; diff gate 2 files 0 failed, leak check clean)
- **Deciders**: owner
- **Research**: `docs/research/rclone-shared-filesystem-2026-10-29.md` (rclone, RustFS,
  OpenDAL, governance gaps). Board card `3605565e`.
- **Prior decisions**: unknown. The decision store (Ruflo) was unavailable when this was
  written. The repo has no earlier ADR on shared storage.
- **Builds on**: ADR-114 (`weftos://` names; a name grants nothing), ADR-120 (exposures and
  the activity log), ADR-122 (cog data plane and blob store), ADR-099 (governed placement),
  ADR-108 (pairing), ADR-103 (per-project daemon topology).

## Context

The owner wants several machines, people and agents to work on the same files, so
swarming can go horizontal. Gitignored files (models, data, fixtures, generated assets)
should already be in place wherever an agent lands, and a local environment should be able
to work against a remote one.

The owner's direction, 2026-10-29:

- **RustFS on PG is the mesh's S3 storage** for every other node.
- **The bucket model:**
  - a **project bucket** for each project. That is the project directory we have on PG today;
  - a **bucket for each worktree** where possible, so one agent or many can work on a
    project at once;
  - **CI/CD buckets** for release builds;
  - a bucket for **each WeftOS node**, even one that is only a Wasm build on an edge device.
    Other WeftOS processes, given access, can then use it.
- **WeftOS controls these spaces and namespaces them properly.** Design starts at the
  higher level, with nodes and projects, and goes deeper into clients and releases later.
- **Governance:** transfers may not always be stoppable, but every connection and
  operation is logged, and a connection that breaks the rules is removed.
- **Credentials:** for people's machines, Terraform writes them as a 0600 `rclone.conf`.

Building blocks (checked 2026-10-29):

- **RustFS**: Apache-2.0, release 1.0.4. It has IAM, policy, audit, bucket notification,
  replication and erasure coding. It is also a library on crates.io (`rustfs`,
  `rustfs-iam`, `rustfs-audit`, ...).
- **OpenDAL** (`opendal`, Apache Software Foundation) is the Rust client for S3 and other
  backends.
- **rclone** is the desktop mount and sync tool. It has no file locking, and each client
  keeps its own cache.

## Decision

### 1. One store service per account, run by the daemon

RustFS runs as a governed workload under the daemon (ADR-099), not linked into the daemon
binary. Linking it in can come later, behind an off-by-default feature.

- **The weftos account on PG holds the mesh's store.** It is bound to the tailnet and
  loopback only.
- **A client account gets its own instance in a later phase (S5)**, running as the client
  user with its data under `/data1`, so the PG client-isolation rules hold. One instance
  never holds two accounts' data.

### 2. Names first, buckets second

Every store has one `weftos://` name. It is a `store` sub-resource of the thing that owns it
(ADR-114: one thing, one name; sub-resources are kind-specific).

| Owner | Name | S3 bucket |
|---|---|---|
| a node | `weftos://<mesh>/nodes/<node id>/store` | `n-<node id>` |
| a project | `weftos://<mesh>/projects/<ULID>/store` | `p-<ulid>` |
| a worktree of a project | `weftos://<mesh>/projects/<ULID>/worktrees/<slug>/store` | `w-<ulid>-<h12(slug)>` |
| a release pipeline of a project | `weftos://<mesh>/projects/<ULID>/releases/<channel>/store` | `r-<ulid>-<h12(channel)>` |

- **New ADR-114 sub-resources:** this adds `worktrees/<slug>` and `releases/<channel>` to
  `projects`, and `store` to `nodes`, `projects`, `worktrees` and `releases`. ADR-114 is
  updated when this ADR is accepted.
- **Bucket names** are derived from the name, never chosen by hand:
  - lowercase;
  - at most 63 characters;
  - `h12` is the first 12 hex digits of BLAKE3 over the slug.

  The readable slug, the owner and the creation time are kept as bucket tags.
- **The mesh is implicit:** one store instance serves one mesh's account.

### 3. The daemon owns the spaces

Only the daemon creates, grants access to, or deletes a store.
   Amended 2026-11-08 (owner): any other node or person may sign a grant; the daemon verifies and applies it. Creating and deleting a store stay with the daemon.

- **Lifecycle follows the owner:**
  - pairing a node creates its bucket;
  - `weaver project add` creates the project bucket;
  - creating a worktree (agent-git-flow `wt.sh`, or `weaver`) creates its bucket;
  - cleaning the worktree up removes its bucket, after its contents are promoted or archived;
  - removing a project or unpairing a node archives that bucket, then deletes it.
- **A name grants nothing.** Access is a grant of (principal, store, read | write | admin),
  recorded on the chain. Principals are nodes, members, agent instances and cog instances.
  The daemon writes each grant into RustFS IAM as a policy on that principal's access key,
  so RustFS enforces what governance decided.
- **Default grants:**

  | Store | Default access |
  |---|---|
  | node | the node writes; other WeftOS processes on that node read and write; nobody else |
  | project | the project's members and agents read; promotion and owners write |
  | worktree | the agents and people working in it write; read falls through to the project (§4) |
  | release | CI writes once; everyone in the project reads |

### 4. Worktree stores overlay the project store

- **Reads fall through.** A worktree bucket starts empty, and a read checks the worktree
  bucket first, then the project bucket. A new worktree therefore has every gitignored
  asset without a copy.
- **Writes stay in the worktree bucket.**
- **Promotion happens when the worktree lands** (agent-git-flow `land`). The worktree's
  objects under promotable paths are copied into the project bucket, and anything else is
  archived or dropped. Promotable prefixes are `assets/` and `shared/` by default; `agents/<id>/`
  scratch is archived. If a project object changed after the worktree was created and differs
  from the worktree's, promotion fails before writing anything (or keeps the project copy, or
  overwrites, by policy). Deleting a project object in a worktree leaves a tombstone, which
  promotion turns into a delete in the project (skipped if the project object changed since the
  worktree was created, unless the policy is overwrite). An object and a tombstone for the same key
  mean the object wins. "Changed" is decided by etag, not by time: the first write or delete of a
  key in a worktree records the project's etag for it (`.wt/base/<key>`, `-` if the project had
  none), and promotion treats the project as changed when its etag now differs. No clock is
  involved, so neither client nor server skew can hide or invent a conflict; an object written
  around the overlay has no recorded base and is treated as a conflict (fail safe). An overlay
  read that falls through to a project object records the base too (best effort), so a stale edit
  of something read earlier conflicts; a read made around the overlay still records none. A
  project-side delete of a file the worktree edited is a conflict as well. Every project version
  that promotion overwrites or deletes is first copied to the archive under `.wt/displaced/`.
  Promotion copies use S1's `put_if_match` (or an if-none-match create) so a project write between
  the plan and the copy is a conflict, not an overwrite; deletes re-check the etag just before
  (no conditional delete exists). Claims carry a fencing epoch that `promote` can require, and the
  daemon decides claim expiry on silence it observed on its own monotonic clock. Registry signing
  mechanism, the key options and the off-by-default daemon-only registry store
  (`WEFTOS_WT_REGISTRY_STORE=1`): see
  `docs/research/adr126-s2-registry-signing-and-claim-clock.md`. The daemon refuses to `create` a slug that has already landed. The run is idempotent and
  safe to retry after a partial failure; once the report is written, later writes to that
  worktree bucket are ignored.
- **Who does the overlay:** the `weftos-store` client does it for daemons and agents. For a
  desktop mount, a small rclone `union` remote (worktree first, project as read-only)
  gives the same view.
- **Git trees and `target/` stay on local disk as the working copy; the store also carries a
  shared snapshot of the tree.** (Amended 2026-11-07, owner, card f40828fb. This replaces the
  earlier rule "the store holds the gitignored layer only", which is why docs and planning
  written in a never-merged worktree were invisible to everyone else.)
  - **What is shared:** the files git can see (tracked, plus untracked and not ignored) and a
    `git bundle` of the unmerged commits, as content-named objects in the reserved `.wt/obj/` pool
    of the worktree bucket (schema 2, WO-069 lane D3, below; schema 1 kept them at `.wt/tree/<path>`
    and `.wt/git/branch.bundle`); a manifest (`.wt/tree.manifest.json`) listing each file's
    SHA-256 and object, which paths are uncommitted, and an index of docs and planning with titles;
    and a handoff record (`.wt/handoff.json`).
  - **What is not:** `target/`, `node_modules/` and other ignored files (the overlay layer above
    is how those travel), symlinks (or files under a symlinked directory), files over 16 MiB, and
    anything that looks like a secret: names such as `.env*` (not `.env.example`), `*.env`,
    `.envrc`, `*.pem`, `*.key`, `*.jks`, `id_*`, `credentials*`, `.pgpass`, `*.tfstate`,
    `*secret*` (not Markdown), and any file containing `PRIVATE KEY-----`. This is a denylist and
    is incomplete by nature; the writer stays responsible for what is in the tree.
  - **A fetch treats the manifest as untrusted:** it refuses `.git/`, `.gitattributes`,
    `.gitmodules`, `.envrc`, names with `:`, secret-looking names and case-insensitive
    collisions, checks every object's SHA-256, and never overwrites a differing local file
    without `--force`.
  - **A fetch imports the bundle's commits only into a repository root** (a clone's top level, a linked
    worktree, or a bare repo); into a plain directory, even one inside another repository, it writes the tree
    and prints the `git fetch` command instead, so an enclosing repository is never touched (WO-069 lane I1).
  - **Git still lands code.** The store makes the handoff possible across agents and machines: a
    second agent fetches the bundle and the tree, claims the merge, lands it with normal git, and
    marks it merged (which requires holding the claim, and that the published tree is still the one
    claimed: its head and a digest of its files, uncommitted list and bundle; a republish under an
    expired claim is allowed but voids it for merging, so the merger claims again; a claim recorded by
    a daemon from before head fencing must be claimed again too). Promotion (below) then runs. The claim is
    advisory: there is no cross-client locking, two agents racing can both read back their own
    write, so the merger must still land through git's compare-and-swap. While a claim is live,
    only the holder can republish or re-hand-off; a claim older than 30 minutes can be taken over.
    The daemon evaluating the claim compares the claim's stamp (written by the claimer's clock)
    with its own clock. A claim stamped more than 10 minutes ahead of that clock is treated as
    expired, not refused, so a claimer with a bad clock cannot block a merge forever. The claim
    host is never "unknown": macOS reads `LocalHostName` from the system preferences, and the CLI
    requires `--host` when no name is found.
  - **Reserved state never leaks:** everything under `.wt/` is hidden from overlay reads and
    listings and is never promoted as an asset. On promotion the shared tree, bundle, manifest and handoff
    record are archived with the rest.
  - **Control state is daemon-only:** `.wt/` in a worktree store and `worktrees/` in a project
    store are written only by the daemon. The governed path refuses those keys, and the
    per-principal IAM policy carries a `Deny` for the same prefixes. Promotion still trusts
    `.wt/promoted.json`, `.wt/promoting.json` and `.wt/base/*`, which is safe unless someone holds
    the RustFS root credential, because that credential can forge them.
  - **Ceilings and the bundle hash:** the manifest is untrusted, so `fetch`, `sync-docs` and
    `search` enforce limits on the manifest's size, the number of files and docs it lists, each
    file's size, the total selected and the bundle's size, and refuse over-limit input as an error
    rather than truncating. The manifest records the bundle's SHA-256, and a fetch refuses a bundle
    that does not match it.
  - **Slug and bucket rules:** a registry entry must name this project's bucket and the slug it is
    filed under, or it is unreadable and not followed. A landed or archived worktree's slug cannot
    be created again until its store is archived with `--delete`, and a promotion record from an
    earlier life of a slug is ignored. `publish`, `claim` and `promote` refuse a store this daemon
    knows to be archived, a bucket with no metadata, and a bucket whose metadata names another
    slug or project. A second daemon on the same backend can archive and delete a worktree store
    the first created (with `--delete` only); the creating daemon's own record and grants stay
    until it drops them, which is harmless once the bucket is gone.
  - **Path rules for the `store.worktree` verbs:** the daemon reads and writes the paths itself,
    so every path parameter must be absolute and resolved. They must lie outside the daemon's own
    state (`--dest` and `--cache` may not be inside the runtime directory or contain it),
    `--root` must contain `.git`, and the lists and the bundle must be regular files. No verb
    writes through a symlink, and `fetch_bundle` writes a temporary file and renames it.
  - **Discovery:** the daemon writes a small registry entry per worktree into the project bucket
    (`worktrees/<slug>.json`: branch, head, state, writer, merger, doc count). Worktree principals
    cannot write there, so "writes stay in the worktree bucket" still holds for agents.
- **One writer per path.** There's no cross-client locking. Parallel agents in one
  worktree write under `agents/<instance id>/`, and shared paths are read-mostly.

#### Landing controls, quotas and retention (2026-11-08)

- **Preview and conflict policy.** `weaver store worktree promote --dry-run` (`worktree-store.sh land-preview`)
  reports what a land would do with no writes: keys to promote, keep, archive and delete, and each conflict
  with its kind (`both_edited`, `project_deleted`, `deleted_but_project_edited`). `--on-conflict
  fail|keep_project|overwrite` (`WTS_ON_CONFLICT` for `land`) chooses per land; `fail` stays the default.
- **Size cap on a land.** A land that would copy more than 1 GiB into the project is refused before any
  write (`--max-bytes N`, `0` = no cap, `WTS_MAX_BYTES`). Scratch that is archived does not count.
- **Worktree store quota.** `publish` refuses when the store already holds more than 8 GiB
  (`--quota-bytes N`, `0` = none; `retention::DEFAULT_WORKTREE_QUOTA`, twice the 4 GiB tree ceiling, raised from 2 GiB on 2026-11-15). This is a check at publish, not a hard limit: an agent writing straight
  to its bucket with its own key is not stopped (RustFS bucket quotas are not used). `store worktree usage`
  lists objects, bytes, landed time and over-quota per worktree store.
- **Retention.** `store worktree gc` is a dry run unless `--confirmed`. It retires worktree stores landed
  at least `--keep-days` ago (default 14; archived then deleted through the store service) and never an
  unlanded one, however old: that is what `leftovers` reports. With `--archive-keep-days N` it also deletes
  archived worktree objects older than N days (default: never expire; this is recovery data).
- **Orphans (WO-069 lane E; on the local integration branch `w2/wo069-worktree-mirror`, not on `target-0.8.5`, not pushed).** A worktree removed without
  landing keeps its store. The sweep records when the local path first went missing and calls
  `store worktree orphan <slug> --since <ms>`; the daemon marks the registry entry `orphan` (with
  `orphaned_at_ms`, `last_head`, `missing_since_ms`) once the path has been gone `WEFTOS_WORKTREE_ORPHAN_AFTER_HOURS`
  (default 24). Only an `open`, unlanded store can become one; claimed, handed-off, merged and archived stores are
  refused. `gc` lists orphans in its dry run. `gc --confirmed` archives every object of an orphan's bucket
  (`.wt/*` and overlay objects), verifies the copies, deletes the bucket and its registry entry, once it has been an orphan
  `WEFTOS_WORKTREE_ORPHAN_ARCHIVE_AFTER_DAYS` (default 14). It writes nothing outside that bucket, its entry (and
  signature object) and the registry digest. A republish clears the orphan state. Time comes in as `now_ms`; tests drive it.
- Tests: library (`s2_worktree*`, split by topic), daemon (`store_worktree_tests`), script, and a live loopback RustFS run
  (`s2_land_rustfs`, ignored: preview, conflicts, cap, `@`-bearing archive keys, retention listing).

#### Read-modify-write uses `put_if_match` (2026-11-08)

The handoff record (`.wt/handoff.json`) is changed only with a conditional write. `claim_merge`,
`mark_handoff`, `mark_merged_at`, `mark_landed_at` and `publish_tree` read the record's etag, then its body, decide, and write with
`put_cas` (S1 `put_if_match`, or `if-none-match` when creating). If someone else changed it first the
write reports "lost", the caller re-reads and decides again (up to 6 times, then a "being changed by
others" error). Effects: of two claimers that read the same record exactly one write lands and the
other is refused; a writer's late `mark_handoff` cannot erase a claim that landed after it read; a
merge cannot land over a takeover that happened after its epoch check; a first publish cannot
replace a record created between its check and its write. The etag is read before the body, so a
change between the two calls makes the write fail rather than pass on a stale decision. Evidence:
deterministic race tests (`s2_handoff_cas`, they fail on the unconditional code) and a live loopback
RustFS run with 16 simultaneous claimers over 8 rounds (`s2_land_rustfs`; this one passes on the old
code too, because RustFS answered fast enough that the old write-then-read-back rarely interleaved, so
it shows the conditional path is correct on the real backend, not that the old one was wrong).
Not converted: the registry refresh (a last-writer-wins pointer the daemon rewrites from the worktree
bucket), the follower's state file (local; merged under a lock, see 4a), and `publish_tree`'s
many-object write itself.

#### The fence is atomic with publish (WO-069 lane Y, 2026-11-15)

The handoff record is the one object every merge decision commits through. A fence read the manifest
and then wrote the record, and the manifest is a plain PUT, so a publish landing between the two was
not caught, and the daemon's `promote --epoch` even checked (`fence_claim`) before an unfenced land
write. Now:

- `publish_tree` announces the manifest it is about to write (`publishing`: the sha256 of the exact
  bytes, head, tree digest, stamp, writer) with a conditional write on the record version it read at
  its start, immediately before the manifest PUT. It refuses there, before the manifest, when the
  worktree was merged or someone else took a live claim while the files uploaded. A pending
  announcement of another publish does not block it; its stamp is carried as `held_ms`.
- A fenced merge or land (`mark_merged`, `mark_merged_at`, `promote --epoch` through `mark_landed_at`)
  reads the record, reads the manifest, checks epoch, head and tree, refuses while an announcement is
  pending (the manifest is not yet the announced bytes) or a held stamp is live, and writes on the
  version it read. An announcement after that read makes the write fail; one before it is visible.
  So a recorded merge names the tree that is published when its write lands.
- A failed manifest PUT withdraws the announcement (puts back the one it replaced). If that fails too,
  merges wait until the stamp expires and the publish error says so; the writer is never blocked.
- Liveness of a stamp is the claim rule (30 minutes, 10 minutes of skew), so a bad clock holds nothing
  longer than that. The residual this left (design note B1: a publisher stalled longer than that between
  its announcement and its PUT, or with a clock that far behind, landing a manifest after a merge) is
  closed by lane D3 (below): the manifest write is conditional on the version the publish read, and a
  post-check re-reads the record right after it; merged or archived meanwhile, the publish puts the
  previous manifest back (conditionally) and fails without sweeping; otherwise it marks its announcement
  `done` with a conditional write, so a merger that read the record earlier fails its own write and
  re-reads. Unfenced land (`promote` without `--epoch`) is unchanged: git is the truth.
- **A presented epoch asserts a claim (owner decision Q9, 2026-11-15).** A caller that names an epoch
  (`promote --epoch`, `fence_claim`, `mark_landed_at` with an epoch) is refused unless the record is
  `Claimed` at that epoch: an absent, open, handed-off, merged by someone else, or archived record
  refuses with the state it is in and what to do (claim again, or land without an epoch). The one
  pass is a repeat of a land the same actor already recorded at the same epoch, which stays the
  idempotent success. Before this, such a call passed as "nothing to fence". The unfenced land
  (no epoch) is unchanged (design note B6, still open); `scripts/worktree-store.sh land` now exits
  non-zero when a saved or `WTS_EPOCH` epoch is refused.
- Cost: a republish makes two calls more (the record HEAD for its etag, the announcement); a first
  publish none; claim and merge none; `promote --epoch` one fewer. The fields are additive (serde
  defaults); a daemon from before them drops them when it rewrites the record, so the guarantee holds
  when every daemon writing a worktree bucket runs this code (owner decision Q10, gate item 8 in
  section 4a). Design and proof sketch:
  `research/weftos-agent-notes/reports/wo069-fence-atomicity-design-20261115.md`; tests
  `s2_y_fence_atomic`, `s2_y_publish_intent`.

#### Content-addressed objects and the grace sweep (WO-069 lane D3, 2026-11-15)

Owner decision 2026-11-15 ("take the recommendations" of
`research/weftos-agent-notes/reports/wo069-content-addressed-keys-proposal-20261115.md`). Closes
finding 3a at every cut point and the follower-across-a-completed-publish window (drills W1, W2).

- **Objects.** Each distinct content (files and the bundle) is one immutable object,
  `.wt/obj/sha256/<first 2 hex>/<sha256>.g<generation>`. Nothing a manifest names is ever
  overwritten. SHA-256, with the algorithm segment in the key. The generation is a deviation from the
  proposal's plain sha key: a publish only ever PUTs a key of a generation newer than every manifest
  it can see, so a PUT can never re-create a key another publish's sweep is about to delete (with plain
  keys, a publish built on manifest N re-uses or re-PUTs an object N's own sweep deletes next; the
  property test finds that at once when the rule is reverted). Re-use without a PUT: an object the
  base manifest names, one it is retiring that is younger than half its grace, or a stray of a newer
  generation than the base (what a cut or losing publish of the same base left), so a rerun uploads
  nothing twice. Renames and duplicate files cost no upload.
- **Manifest schema 2** (same key). `entries` (renamed from `files` so an rc.1 reader fails closed with
  `missing field files`), `objects: "sha256-fan2-gen"` (a reader refuses any other layout), `generation`,
  each entry's object generation, `bundle_sha256` + `bundle_gen`, `retiring`, `v1_retired_ms`,
  `changes_recorded` (finding 6f). Readers read schemas 1 and 2 and refuse 3 and later with an upgrade
  message; the tree digest a claim fences on is the same for one tree under either schema.
- **Publish.** Upload what is not in the pool; build the manifest with its `retiring` list (every object
  the base named that the new one drops, `since_ms` = now; carried entries keep their stamp; strays
  are recorded on first sight, because the S2 listing carries no usable modification time, it is 0 on
  every S2 backend); announce (lane Y); write the manifest with `put_cas` on the version read at the
  start. A lost race is refused, not retried: nothing is written or swept, the announcement is
  withdrawn, and the rerun builds on the new manifest and re-uses the uploads (key choices, generation
  and retiring list all depend on the base). Then the post-check (B1, above), then the sweep.
- **Sweep.** An object is deleted only when the manifest just written does not name it and its
  `retiring` record is at least the grace old (`now - since_ms`, a stamp from the future counting as
  now). Grace 24 h; bundles: the two newest retiring keep 24 h, older ones go after 1 h. A publish
  whose retiring list (not counting what it deletes) would exceed twice the file ceiling fails loudly
  and writes nothing. The graces and the clock are parameters (`PublishOptions`, `now_ms`); the
  daemon's `WEFTOS_WORKTREE_SWEEP=off` is the kill switch: retire in the manifest, delete nothing.
- **Readers** resolve a path to its object through the manifest (`TreeManifest::object_key`,
  `bundle_object`): browse, fetch, sync-docs, search, overlaps, summarize and the caches. A reader
  holding a manifest replaced less than the grace ago reads it clean. The browse cache inside its TTL
  after a republish now serves the previous version's bytes, consistent with its cached listing (owner
  decision: at most one TTL old, never wrong bytes); nothing used for fencing reads through a cache.
- **Migration, no flag day** (whether a real schema-1 bucket exists outside throwaway stores is
  unknown, so both are handled). The first schema-2 publish of a schema-1 bucket uploads into the pool
  (a bundle it has no file for is copied server-side), stamps `v1_retired_ms`, and the `.wt/tree/`
  objects and the old bundle key are swept a grace later; a follower holding the schema-1 manifest
  reads clean meanwhile. Rollback lever: `WEFTOS_WORKTREE_MANIFEST_SCHEMA=1` makes this build write
  schema 1 again (path-named objects, removed at once as before; the rc.1 shape, with the extra fields an
  rc.1 reader ignores), conditionally and post-checked, and still sweeps the pool. An rc.1 binary meets
  a schema-2 manifest and fails closed: reads error, a publish refuses.
- **Archive.** Orphan retirement and promotion copy every object of the bucket, so an archive holds the
  manifest, every object it names and the retiring ones still inside the grace, and resolves under its
  prefix with the same `object_key`.
- **Residuals.** Publisher clocks more than half a grace apart (half an hour for bundles), or a publish
  stalled that long between taking `now` and reading the manifest; a manifest deleted by hand (re-share)
  while a sweep of the old tree is still running; old binaries writing the bucket (B3: their manifest
  PUT is unconditional; gate item 8 requires none before a client project uses shared worktrees). Each would surface as a loud `missing`, healed by the next publish (an object
  the manifest names that the listing lacks is uploaded again, to a fresh key).
- **Cost.** A no-change republish is 10 store calls in the library (was 7): the manifest PUT is a
  `put_cas` (same count) and the post-check adds HEAD + GET + `put_cas` of the record; 15 through the
  daemon (was 12). No object PUT, no DELETE. The one listing per publish is unchanged.
- Code: `share_schema.rs`, `share_gc.rs`, `share_write.rs`, `share_intent.rs`; gate test
  `s2_d3_gc_prop` (random publish sequences with cuts at every call, a second publisher run inside one
  at a random call or at its first delete, skewed and backward-stepping clocks, quiet periods past the
  grace; every delete call is checked against the current and every recently replaced manifest), and
  `s2_d3_publish`, `share::schema::tests`.

#### Publish hash cache (WO-069 lane E1, owner decision 2026-11-15)

A no-change republish of 50 000 files took 294 to 525 s on a loaded machine because `publish` read and
hashed every file every time (drills report, round 6 addendum). The publisher now keeps a local cache,
one file per project and worktree on the publishing machine's disk under the daemon's runtime directory
(`worktree-hash-cache/`), never in the store and never in the git worktree; the library takes it as an
optional `HashCacheConfig` and reads no environment. It is keyed on the path relative to the worktree
root. An entry holds the size, mtime (ns), on unix the inode and ctime (ns), the SHA-256, the time it was
hashed and the markdown title (the only other fact derived from content; a file with private-key content
is refused and never cached). An entry is used only if every validator matches exactly and its mtime is
strictly more than 2 s older than the time it was hashed (git's racy-timestamp rule; younger files are
hashed again next time). Every publish also re-hashes a rotating sample of the entries (1 %, at least 16),
and one disagreement with a matching entry discards the cache and does a full pass in that same publish,
reported as `mismatch`. A full pass, which ignores the cache and rewrites it, also happens when the file
is missing, unreadable, corrupt, of another format version or for another root, and when the last full
pass is `WEFTOS_WORKTREE_HASH_REHASH_HOURS` old (default 24; 0 = every publish). `WEFTOS_WORKTREE_HASH_CACHE=off`
publishes as before the cache. Writes are temp-and-rename and a failed write never fails the publish. A
change that keeps size and mtime goes unseen off unix (no ctime) until the sample reaches the file or the
next full pass; on unix the ctime moves, so it is seen at once. The manifest for a tree
is byte-identical with the cache off, cold or warm. The publish report and the daemon reply gain
`hash_cache` (files hashed, bytes hashed, files from the cache, sampled, full pass and why).

### 4a. The L1 follower view: seeing unmerged worktrees before integration

(Added 2026-11-07, owner decision: this is required leftover work, not optional. ADR-130 names
L1 as the tier for "unmerged layers that other nodes should see before integration".)

- **Follower:** any node with project read access. It is read-only; it never writes to a worktree
  or project bucket. It keeps one small JSON state file of what it last reported.
- **Source of truth:** the daemon-written registry in the project bucket (`worktrees/<slug>.json`),
  then each worktree's manifest. The daemon refreshes a registry entry on every publish, handoff,
  claim and merge, and removes it after promotion and bucket deletion.
- **What a follower can do** (`weftos_store::follower`, `worktree-store.sh`):
  - `poll` / `follow`: new, head-moved, state-changed, gone and unreadable worktrees since the last
    look. It fetches registry entries eight at a time and reads `worktrees/_digest.json`, a hint the
    daemon rewrites from a fresh listing on every registry change, so only entries whose etag
    moved are fetched. Two daemons refreshing at once can leave the digest one update behind until
    the next refresh; `follow --full` (also accepted by `worktree-store.sh follow`) skips it. A head move and a state change in one poll are both reported; a recreated slug (new
    bucket) is gone-then-new; one corrupt registry entry is reported, not fatal; an empty registry
    when worktrees were known is an error (wrong bucket?), not mass-gone, unless confirmed;
  - `search_docs` / `search`: docs and planning across every unmerged worktree, by path or title;
  - `sync_docs` / `sync-docs`: a read-only local mirror of one worktree's docs, pruned as docs leave;
  - `leftovers`: open worktrees idle 3 days, handoffs unclaimed 1 day, claims past the 30-minute
    TTL, merged entries still in the registry after a day (cleanup stuck), orphans (`why: orphan`, with the
    age counted from when the daemon marked it; they stay listed until `gc` retires them), and
    timestamps in the future (writer clock ahead, so idle time is unknown). This is the
    forgotten-work detector;
  - `overlaps`: files two unmerged worktrees both changed differently, as a merge-conflict hint.
    It names both base commits so a caller can discount a result when they differ, because with
    different bases it over-reports. It also reports a file one
    worktree deleted that the other changed (a rename is a delete plus an add, so editing the old
    name is caught). That needs the manifest's `changes` list, which the script builds from
    `git diff --name-status --no-renames <base>` plus untracked files; a manifest published before
    that field existed gets content comparison only. Paths are compared, never hunks, so absence
    still does not prove a clean merge;
  - **audit:** the S2 verbs a follower's operator runs (create, publish, claim, promote) appear in
    the activity log and on the chain, failures included;
  - **Browse without a checkout** (WO-069 dashboard reader, lane B; on the local integration branch `w2/wo069-worktree-mirror`, not on `target-0.8.5`, not pushed):
    `follower::browse_registry` (worktree list, lenient on a corrupt entry), `browse_ls` (one folder of the
    shared tree, from the manifest, folders first, sizes, SHA-256, doc titles) and `browse_get` (one file,
    capped at `min(max_bytes, 4 MiB)`; over the cap it returns `withheld` with size and checksum, not bytes).
    The slug is resolved through the registry and the bucket's `.wt/meta.json` is cross-checked first; only
    the object the manifest names for a path it lists is ever read (schema 1 `.wt/tree/<path>`, schema 2 its
    `.wt/obj/` pool object), under the same path refusals as `fetch`.
    `weaver store worktree browse` and `worktree-store.sh browse` call them; WO-067's `store.browse.*` RPC
    gates them with `store-browse.json` (`worktrees/` in the project store, `.wt/tree/` per worktree store).
    Since lane D3 `.wt/tree/` there is a logical name checked before `browse_ls` / `browse_get`, not a key
    prefix; no rule may name `.wt/obj/` (a listing of it is hashes and sizes, past the manifest's
    refusals), and the generic `store.browse.ls` must refuse `.wt/` on a worktree store (to be applied
    where that verb lives, `w2/wo067-daemon`; it is not on this branch).
  - **Manifest cache** (WO-069 lane F; on the local integration branch `w2/wo069-worktree-mirror`, not on `target-0.8.5`, not pushed; answers Q14):
    `follower_cache::ManifestCache` (default TTL 5 s, 64 slots, 32 MiB of manifest JSON, caller supplies `now_ms`)
    holds per project and slug the registry entry and its etag, the manifest etag and the parsed manifest.
    Inside the TTL a hit reads nothing, and skips the `.wt/meta.json` cross-check because the slot was made by
    a load that ran it on that same entry; after the TTL one `head` of the manifest decides, and a moved etag
    means a full cold load, cross-check included. A moved entry etag (digest, listing or `poll_fast_cached`)
    drops that slug, and `browse_get_cached` retries once cold if the object no longer matches the cached
    manifest. Measured on a loopback RustFS, 5 worktrees of 200 files, 50 `browse_ls` plus 50 `browse_get`:
    758 ms, 350 gets, 150 heads, 4.3 MB read uncached against 170 ms, 65 gets, 65 heads, 0.25 MB cached; 20
    registry lists from the digest 91 ms and 100 gets against 12 ms and 25 gets. `poll_fast` already reads
    only the digest and changed entries, so the cache does not change its cost (25 gets either way).
  - **What a reader can see late, and by how much** (WO-069 lane J2; the daemon's read cache, `follower_cache_shared`):
    this daemon's own writes show at once (the write arm drops the slot when it ends). Another writer's change:
    - `browse` list, `leftovers`, `search` hit states, the gc dry-run plan: no lateness, they are as of their own registry LIST (an entry is reused only under the etag that LIST returned);
    - `browse <slug>` ls and get, `overlaps`, the doc set `search` finds in a worktree: up to one TTL (5 s) late, labelled `cache: {age_ms, ttl_ms, source: "cache"|"store"}` in the reply; a get returns the bytes the cached manifest names (the previous version) until then;
    - nothing is ever older than `max_age` (60 s), whatever the etags say; a deleted entry is found gone at the TTL, and a miss is never cached;
    - `follow` is digest-driven: an entry written without a digest refresh (a raw write, a writer that died between the two) is not seen until the next refresh; `follow --full` lists and sees it (the separate digest-lag limit).

    Knobs: `WEFTOS_WORKTREE_CACHE_TTL_MS` (default 5000), `WEFTOS_WORKTREE_CACHE_MAX_AGE_MS` (60000), `WEFTOS_WORKTREE_CACHE_MB` (32; `0` or a value that is not a whole number turns the cache off). `--fresh` (`fresh: true`) on `weaver store worktree browse|search|overlaps` and `scripts/worktree-store.sh browse|search` skips the cache for that call and refreshes what it holds; it costs a cold load (one more HEAD than the uncached path on a single worktree). A signed registry bypasses the cache.
  - **a registry entry is a pointer, not authority:** before reading a bucket a follower checks the
    slug is well-formed and that the bucket's own `.wt/meta.json` names the same slug, so an entry
    cannot aim a follower's read grants at another worktree's bucket.
- **Waking and polling** (owner answers on ADR-131, 2026-11-07): head notifications reach devices
  through the mesh relay from one site subscriber (the PG WeftOS daemon). Followers treat a
  notification only as a wake-up and re-read the registry; a 30-60 s poll is the fallback and the
  source of truth. Polling costs one list plus one get per worktree. Registry entries are not
  signed in S2, so a follower trusts its own read grants and the bucket cross-check below; signed
  worktree heads are an open question if followers ever read from an untrusted relay.
- **Spike evidence and signing wiring (2026-11-08):** `docs/research/adr126-s2-l1-follower-view.md`.
  Against a throwaway loopback RustFS with scoped keys, a read-only follower sees, searches, mirrors
  and overlaps unmerged worktrees, and is refused every write. `promote` now removes the registry
  entry (it used to leave a landed worktree showing as an abandoned claim). Daemon signing is built
  and off unless `WEFTOS_WT_REGISTRY_SIGN_KEY` / `_TRUSTED` / `_REQUIRE_SIGNED` are set; the owner's
  remaining decision is which key.
- **Signature binding and rollback (2026-11-08):** a registry signature covers a domain prefix, the registry
  bucket, the slug, a generation and the entry bytes, so it cannot be replayed into another project or name;
  followers remember the highest generation per worktree and refuse an older signed entry. The generation
  strictly increases on every signed write; a writer stamp lifts it at most to `u64::MAX / 2`. A slug
  whose generation an older daemon pinned at `u64::MAX` refuses every signed write; the repair is to rotate
  the signing key (the old entry would otherwise replay and outrank every new one), remove the entry and
  its signature object, and reset that slug's generation in each follower's state. A follower's state file
  merges the generations under a lock on its directory, or on a sibling `.<name>.lock` where the directory
  cannot be locked, so a racing save never lowers one. The locks are the fast path, not the guarantee: every
  save also reads the file back and writes again (bounded rounds, then a clear error; the next save merges
  again) if any generation is lower than it wrote, so a filesystem whose locks fail or do nothing is covered
  too. Savers in one process are fully covered; two processes with no working locks keep one residual, a
  rename landing in a one-syscall gap after the other's save finished, which lowers a generation until the
  next save by a run that holds the higher one. A trusted-key
  file filters by default (`REQUIRE_SIGNED=0` for an audit mode) and verbs return a `trust` field. See
  `docs/research/adr126-s2-l1-follower-view.md`.
- **Per-client stores:** a client project has its own buckets (ADR-131: one seed bucket and one
  inbox bucket per client), so a follower of a client project reads only that client's registry.
- **Clocks:** ages compare the registry's writer-side timestamp with the follower's clock, so the
  thresholds are days. They are a nudge, not a lock. Claims are the exception and are checked as
  described in §4.
- **Writer trigger, WO-069 lane A (on the local integration branch `w2/wo069-worktree-mirror`, not on `target-0.8.5`, not pushed):** `worktree-store.sh register` records this
  machine's worktrees (local state under `$XDG_STATE_HOME/weftos/worktree-store`); `sweep` publishes a registered
  worktree only when its tree digest moved (SHA-256 over HEAD, sorted `ls-files -co` names, size and content hash
  under 1 MiB, size+mtime above), so an idle worktree costs zero store calls and uncommitted docs are shared. A
  registered path that vanished is marked `orphan` (last published head, first-missing time); nothing is archived or
  deleted, which stays the gc policy question. `install-hook` adds a post-commit accelerator running
  `sweep --only <slug>`: per-worktree `core.hooksPath` when `extensions.worktreeConfig` is set, else one shared hook
  that ignores unregistered worktrees. A foreign post-commit is never overwritten (`--chain` keeps it running).
- **Publish cost and fetch of a tree with no bundle (WO-069 lanes L and J; same local branch).**
  `publish_tree` lists the tree prefix once and treats a file as unchanged when the previous
  manifest has the same SHA-256 and the listing has its object at the same size; it no longer
  sends a HEAD per file, and it reuses that listing for the stale sweep. A missing or wrong-size
  object is uploaded again. An object another writer adds between the listing and the sweep is
  swept by the next publish, not this one. Measured on a loopback RustFS (10 000 files, no-change
  republish, release build, loaded machine): 10 002 HEAD calls before, 2 after; wall time 52 to
  120 s before, 3.6 to 18.5 s after (call counts are exact, wall times are noisy). A first
  publish is still one PUT per file. `fetch` of a tree with no commits past its base writes the
  tree, exits 0 and returns a note (`bundle_head: null`); it used to exit 1.
  Source: the WO-069 drills report (`wo069-4a-drills-20261114.md`, round 4 addendum).
- **Not in scope:** an operator UI (card d18e1ab0), automatic merging, and enforcing that anyone
  acts on a leftover.

#### Gate for moving a client project onto shared worktrees

Card 4c4319e0 holds the move until these have passed. S2 supplies the code; the evidence is still
owed and is **not** produced by the in-memory tests.

1. Against real RustFS (not `MemBackend`): publish, poll, search, sync-docs and claim with two
   different principals on two machines; a follower sees a publish within one poll interval.
   - Evidence so far (2026-11-14 and 15, WO-069 drills against a throwaway RustFS 1.0.4, not PG): two
     principals on one machine through one daemon (14 of 14 shell steps); two real S3 keys, a
     read-write writer key and a read-only follower key, which refused all 8 write attempts; the store
     on a second machine (aepod-xpc, reached over one ssh port forward, about 80 ms per request), with
     the writer, follower and daemon on the MacBook. A publish was reported by the first poll after
     it in 10 of 10 trials (median 40 ms on loopback, 392 ms against the second machine). Still
     owed: two daemons on two machines on two clocks (the follower daemon on aepod-xpc needs a Linux
     build of this branch's weaver, an open owner question), and the same run against the real
     store on PG.
   - **Evidence, 2026-11-15 evening (two machines, two clocks, throwaway RustFS 1.0.4 on the second
     host; not PG):** the writer ran on the MacBook and the follower on aepod-xpc, each with its own
     scoped key (writer read-write, follower read-only). The clocks differed by 1.65 to 1.67 s
     (measured before and after, good to about 0.2 s). Library follower with the read-only key: poll,
     search, sync-docs, browse and fetch of the tree and bundle passed; a publish was seen within one
     30 s poll interval in 10 of 10 trials (5.1 to 29.1 s, set by the poll phase); all eight writes
     were refused with `AccessDenied`. A follower daemon on the second host (owner decision Q3: in
     a CI container, own runtime dir, read-only key): follow, search, sync-docs, browse and fetch
     passed, and a publish was seen within one poll in 10 of 10 trials (1.7 to 28.4 s). The code under
     test was the schema-2 build (a39871f3f). Report: `wo069-gate-block3-results-20261115.md`.
     Still owed: a claim made by the second machine (not approved for that run), and the same
     run against the real store on PG.
   - **Evidence, 2026-11-15 night (claim by the second machine; throwaway RustFS 1.0.4 on the
     second host, both daemons at 5d9710806, clocks 1.9 to 2.1 s apart;
     `wo069-gate-block4-results-20261115.md`):** a merger daemon on aepod-xpc, holding a write key
     limited to the throwaway store (owner decision Q3), claimed a worktree the MacBook handed off
     (epoch 1), fetched its tree and git bundle, landed it with plain git in a clone there (head
     equal to the claimed head) and promoted with its epoch. The MacBook saw claimed, then merged.
     A republish from the MacBook under the live claim was refused and left the head alone. Once
     a claim was 30 minutes old by its stamp a republish was accepted, and the second machine's
     `promote --epoch` was then refused. Still owed: the same run against the real store on PG.
2. A Windows node loads a worktree bucket natively (never tested; in scope now on aepod-xpc as the
   bucket-load proof, with the ProjFS drive after macOS D1 and Linux D2). Fetch already refuses
   reserved names (`CON`, `NUL`, `COM1`), trailing dots/spaces, `<>:"|?*`, `git~1` aliases and
   case-insensitive collisions on every platform; long paths and CRLF are still to try.
   - Evidence so far: none. Not touched by the drills. Owed in full.
3. Metadata caching is only partial: measure `list` and `head` latency for a worktree of a client-scale project's
   size (file count, largest file) before assuming one-poll-per-worktree is affordable.
   - Evidence so far (synthetic trees of 1 000, 10 000 and 20 000 files on a loopback RustFS, loaded
     machine): a follower poll that hits the digest is one 72-byte GET and does not grow with tree
     size (under 1 ms to 3 ms); an uncached browse reads the whole manifest (3.2 MB at 20 000 files,
     21 ms); `list` of the tree prefix took 3.97 s at 20 000 files. Publish is the cost that
     scales: a first publish of 20 000 files took 439.7 s (30 to 68 PUT/s). Since lane L a no-change
     republish makes one listing instead of one HEAD per file; that was measured on loopback
     only.
   - **Evidence, 2026-11-15: the client project's real tree.** The project's own session ran the
     counts-only script (`scripts/store/wo069-gate-item3-tree-stats.sh`; it prints no paths) on the
     real tree. In git's view, which is what publish shares: 10 614 files, 141.7 MB in total,
     largest file 4.1 MB, none over 16 MiB, 3 symlinks, 1 105 directories, maximum depth 10; 86 %
     of the files are under 16 KiB. That is 11 % of the file ceiling (100 000) and 3 % of the size
     ceiling (4 GiB), so the tree fits with room. The nearest measured size is the 10 000-file
     synthetic tree (335 MiB, heavier than the real one) on the schema-2 code with a loopback
     RustFS: first publish 139 s, no-change republish 2.4 s (1.6 s with the hash cache), one file
     changed 2.4 s, fetch of the whole tree 16.4 s, follower poll one small GET. The 3 symlinks
     are skipped and make a publish `published-partial` (owner decision Q7: keep).
     The whole directory including ignored files (dependency and build output) is 2 596 002
     files and 104.8 GB with 547 files over 16 MiB: far over every ceiling. Publish therefore has
     to stay on git's view of the tree, as it does today (tracked plus untracked, not ignored).
     Still owed for this item: the same latency measurements against PG's RustFS over the
     network. The two-machine run against a throwaway store on a second host is recorded under
     item 1.
   - **Decided 2026-11-15 20:42 ET (owner):** for the latency over the network, the figure from
     the two-machine run is accepted: about 80 ms per request to a store on a second host. No
     measurement is made against the production store, and the scripts' refusal of its address
     stays. At that figure a no-change publish of the client project's tree (15 calls) costs about
     1.2 s of round trips, a follower poll one round trip, and a first publish of 10 614 files
     about 14 minutes single-threaded. With that, nothing further is owed for this item.
   - **PASSED (owner's word, 2026-11-16 08:26 ET).**
4. Leftover drill: leave a worktree open past the threshold and confirm `leftovers` reports it;
   hand one off and confirm a second agent on another machine lands it with git, then promotion
   and bucket deletion complete and the registry entry disappears.
   - Evidence so far: thresholds crossed by injecting time (a registry entry moved back four days
     is reported `forgotten_open`; orphan marking refused at 23 h and accepted at 24 h; `gc` dry
     run, then due after the orphan age was moved to 15 days). A handoff was claimed, fetched,
     landed with plain git on a second clone on the same machine, promoted and retired, and the
     registry entry disappeared (after the lane J fix; before it `gc --confirmed` left a `merged`
     entry). Still owed: a real leftover over a real day, and a handoff claimed and landed on a
     second machine.
   - **Decided 2026-11-15 20:42 ET (owner):** the 15-day real-clock leftover drill is not run. The
     injected-clock evidence above is accepted for the multi-day thresholds (24 h orphan grace
     and unclaimed handoff, 3 d forgotten open, 14 d archive). Still owed: a handoff claimed and
     landed by a second machine (approved, to run on the second host with a write key limited
     to the throwaway store, once that host has disk space again).
   - **Evidence, 2026-11-15 night (handoff landed by the second machine; same run and report as
     item 1):** the MacBook handed a worktree off; the merger daemon on aepod-xpc claimed it,
     fetched and landed it with git and promoted; the MacBook saw merged;
     `gc --keep-days 0 --confirmed` retired the store, the bucket answered 404, the registry entry
     was gone and `leftovers` did not list it. The multi-day thresholds were not run (decided
     above).
   - **PASSED (owner's word, 2026-11-16 08:26 ET).**
5. Failure drill: kill the writer mid-publish, kill the merger mid-claim, take the daemon down
   during a registry refresh. Nothing may be lost, and a stale claim must be recoverable.
   - Evidence so far: publish cut at four points (before any put, after the first put, at the
     manifest put, during the stale sweep): the previous manifest was served each time, no read
     returned wrong bytes, and the rerun completed with nothing lost; but two cut points leave one
     changed file failing its checksum until the rerun (see the interrupted-publish window
     below). Stale claims (library clock; the daemon's `claim` instead waits 31 minutes of its own observation): a claim 31 minutes old was taken over with a new epoch, a claim 29
     minutes old was not, a claim stamped 9 minutes ahead stayed live, one stamped 11 minutes
     ahead was taken over. Daemon killed with `kill -9` at 0.8 s, 2.5 s and 4.5 s into a 200-file
     publish: restart, rerun, all files fetched. The registry refresh was cut only
     deterministically in a library test (entry written, digest not), not at a random point in
     the daemon. Still owed: killing the merger mid-claim and waiting out the daemon's own
     30 minute observed expiry (a restart resets that watch), and a real writer process kill on a
     machine that publishes directly with its own key.
   - **Evidence, 2026-11-15 evening (real processes and a real clock, one machine, throwaway
     loopback RustFS, schema-2 build 230141d27; `wo069-gate-block2-results-20261115.md`):** the
     writer daemon was killed at seven points inside a 300-file publish; after each kill the
     served tree was the previous complete one, every file read back byte-equal with no checksum
     error, and the rerun completed. The daemon was killed around a registry refresh with the
     digest write dropped: the digest follower stayed one update behind, `follow --full` saw the
     move, and the next refresh healed it. The merger was killed just after its claim reached the
     store: a second merger was refused 30 times over 29 min 03 s of its own observation and took
     over at 30 min 04 s with a new epoch (attempts were one minute apart), and the dead merger's
     epoch was then refused. Still owed: the same with the merger on a second machine, and a
     writer that publishes directly with its own key on another machine.
   - **Evidence, 2026-11-15 night (merger killed on the second machine; same run and report as
     item 1):** a merger daemon on aepod-xpc was killed 0.33 s after its claim write reached the
     store and before it heard the answer. A second merger on the MacBook (another principal, on
     the other clock) was refused 35 times, last at 1 799.8 s of its own observation, and took
     over at 1 802.0 s with epoch 2. The dead merger, restarted, was refused for epoch 1 and
     refused a new claim. Seen in that run: until the takeover, the registry entry still read
     `handoff` with no merger, because refused claims do not refresh it; the record decided
     correctly throughout. Still owed: a writer that publishes directly with its own key killed
     on another machine, and a registry refresh cut with a second-host daemon.
6. Follower drills on real clocks and a real registry: a writer machine a day ahead; a corrupt and
   an unknown-schema entry; an empty or wrong project bucket; a merged entry whose cleanup never
   ran; an interrupted `sync-docs` followed by a deleted doc; `overlaps` on two real worktrees with
   different bases (measure the false-positive rate); two overlapping `follow` runs sharing a state
   file (`FollowState::save` writes atomically via temp+rename; since WO-069 it merges the generations
   under a lock, the rest is last writer wins; `load` errors on a corrupt file rather than resetting).
   - Evidence so far: none of this list on real clocks. Closest case touched: a merged entry
     whose cleanup never ran (the lane J fix removes the one cause found). Still owed: the whole
     list, except as decided below.
   - **Decided 2026-11-15 (owner, Q13):** "a writer machine a day ahead" is accepted on the
     injected-clock drill (`crates/weftos-store/tests/s2_drills_follower.rs`, drill 6a, in memory, on
     the directory backend and on a live RustFS). It cannot be shown on real clocks without
     changing a machine's system clock, which the gate runs never do, and the daemon has no clock
     seam. The other sub-items of this list are still owed on real clocks.
   - **Evidence, 2026-11-15 evening (through the real daemon and CLI, real clock, one machine,
     same report):** sub-items a to g ran, 16 of 16 steps: damaged and unknown-schema entries are
     reported and do not stop a follower; an empty or wrong project bucket is an error, not
     mass-gone; a merged entry whose cleanup lagged is reported; a `sync-docs` killed with 698 of
     1 500 docs mirrored, followed by a second sync, matched the worktree exactly; `overlaps` on
     two worktrees with different bases reported 8 files, all true conflicts, none missed; ten
     rounds of three concurrent `follow` runs on one state file always left a parseable file.
     Still owed: these on two machines; "a writer a day ahead" stays on the injected-clock drill
     as decided above.
   - **PASSED (owner's word, 2026-11-16 08:26 ET).**
7. ADR-131 D1 and D2 pass first (the card's prerequisite).
   - Evidence so far: not touched by the drills.
8. Every daemon that writes a worktree bucket runs a build with the claim fence and the conditional
   manifest write (owner decision Q10, 2026-11-15). The fence (claimed head and tree, the publish
   announcement, the fenced land) and the conditional manifest write with its grace sweep hold only
   when every daemon that writes a given bucket has them. An older daemon drops the new handoff fields
   when it rewrites the record, never announces a publish, writes the manifest unconditionally and
   deletes replaced objects at once, so one such writer voids the guarantees for that bucket however
   many new ones there are. The first build that has them is the build that contains
   `w2/wo069-worktree-mirror` (not released or pushed; no version number exists yet). The operator
   checks, before the client project's first shared worktree: each machine that writes the project's
   worktree buckets reports that build or later in `weaver --version` and its commit; no machine that
   holds a write key for the project runs an older daemon (a stale daemon on a spare laptop with the
   key counts); a read-only follower on an older build is acceptable only if it can read manifest
   schema 2, and an rc.1 reader cannot: it fails closed on schema 2 (reads error), so rc.1 followers
   must be upgraded too or the project kept on `WEFTOS_WORKTREE_MANIFEST_SCHEMA=1` (which does not
   restore the fence for an older writer). A build with the F1 check adds `older_writer` to the `publish`, `claim`, `promote`, `browse`, `leftovers` and `follow` replies and prints one stderr warning per worktree when a bucket's record, manifest or registry entry shows an older daemon wrote it last, and `WEFTOS_WORKTREE_REQUIRE_CURRENT_WRITERS=1` makes `claim` and `promote` (never `publish`) refuse such a bucket.
   It sees only the marks an older daemon leaves (no publish announcement, a claim without its head, a manifest the last announcement does not name, a registry entry without the check), so it cannot see one that has not written since and an unfenced `promote` reads only the record; it supports the operator checks above and does not replace them.
   - Evidence so far: none. Owed in full (2026-11-15). No mixed-version drill has been run.
   - **Decided 2026-11-15 20:42 ET (owner):** each host's operator checks after the branch lands.
     This lane checks the MacBook and aepod-xpc; the production host's operator answers for
     that host. A release tag needs the owner's separate go.
   - **Checked 2026-11-15 night (MacBook and aepod-xpc, before landing; same report):** the
     daemons built from this branch on both machines pass `wo069-gate-item8-writers.sh` against
     the three fence commits, and `older_writer` stayed clean in both machines' views across
     writes by both. The installed daemons do not: the MacBook's is 0.8.4-rc.15 and aepod-xpc's
     is 0.8.4, both older than the fence. Neither machine may hold a write key on a real worktree
     bucket until its installed daemon is replaced; the check is repeated after the branch lands.
   - **Checked 2026-11-16 (after the local landing, all three hosts):** the installed daemons
     were replaced with an unsigned local build of `0992ade32` (0.8.5-rc.1), which contains the
     fence commit `759ccad5a`: the MacBook and aepod-xpc in the afternoon, the production host's
     hub daemon in the evening, each on the owner's go. `wo069-gate-item8-writers.sh
     --min-commit 759ccad5a --include-pg` reports 3 hosts, 0 OLDER, 0 UNKNOWN, 0 skipped. On
     the production host the installed binary is readable only by the daemon's account, so the
     checker ran against a staged copy with the same sha256; the running daemon reports build
     `0992ade3`. A second daemon on the production host (a project daemon under another account)
     still runs 0.8.4-rc.15; the owner decided it does not count as an older writer. The builds
     are unsigned and are to be replaced by a signed rc. No mixed-version drill was run.
   - **PASSED (owner's word, 2026-11-16 21:18 ET).**

Gate items are marked passed only on the owner's word, one at a time (owner, 2026-11-15).
Passed: items 3, 4 and 6 (owner, 2026-11-16 08:26 ET) and item 8 (owner, 2026-11-16 21:18 ET).
Not passed: items 1, 2, 5 and 7.

**Measured in the drills. Each was an open owner question; decisions are recorded in place as they are made.**
Source: the WO-069 drills report (`wo069-4a-drills-20261114.md`; lanes H, I, J and the round 4
addendum). Branch `w2/wo069-worktree-mirror`, local.

- **Size ceiling (decided by the owner, 2026-11-15).** Before: `Limits::default()` was 20 000 files,
  512 MiB in total and 16 MiB per file, and the daemon published with those, so a tree past them had
  its surplus skipped as `OverBudget`. A library caller that raised the limits published 50 000 files
  (1.47 GiB, 730.7 s; manifest 7.9 MB), and then no reader could load that manifest, because
  `parse_manifest` applied `Limits::default().max_files` to every reader. Decision: a worktree may
  hold **100 000 files and 4 GiB** in total; the 16 MiB per-file cap and the 256 MiB bundle cap are
  unchanged. There is **one shared limit**: the writer (`publish_tree`) and every reader take their
  file-count cap from the same `Limits`. In code: `Limits::DEFAULT_MAX_FILES` / `DEFAULT_MAX_TOTAL`
  in `share.rs` are the single source; `parse_manifest_with`, `read_manifest_with`,
  `fetch_tree_with` and `fetch_bundle_with` take a `Limits` (the plain readers use the default, and
  `publish_tree` reads the previous manifest under its own limits). Dependent caps, checked by
  arithmetic: a pretty-printed manifest entry is 157 bytes measured (175 with longer paths), so
  100 000 files is 15.7 to 17.6 MB, and the 32 MiB `MAX_MANIFEST_BYTES` is hit at about 213 000
  files (190 000 with the longer paths), 1.9 to 2.1 times the ceiling; a publish now refuses to write a manifest over that cap instead of writing a tree no
  reader would load. The worktree store quota default rises from 2 GiB to **8 GiB**: a full tree
  (4 GiB) plus a full bundle (256 MiB) plus the manifest (at most 32 MiB) plus control objects is
  about 4.3 GiB, which leaves about 3.7 GiB for overlay objects. The daemon's manifest cache default
  (32 MiB) is unchanged: it holds two full-size manifests, and a manifest bigger than the budget is
  served uncached, never refused. Two per-file scans that were quadratic at this size (the
  uncommitted-path filter in `publish_tree`, the `Select::Paths` match in `fetch_tree_from`) now use
  sets. A 100 000-object listing is 100 pages of 1 000 on S3; it is paged already and is slow, not
  unsafe. Still open: the client project's real file count and size (gate item 3), and a first
  publish of 100 000 files is about 15 to 40 minutes at the measured 30 to 68 PUT/s.
- **Who is trusted.** The `store.worktree.*` verbs and `store grant` run on the daemon's operator
  path and take the principal as a label the caller supplies. On one daemon, a principal that
  holds only read grants ran `share` (a publish), `claim` and `store grant` on itself, and all
  three were accepted. The governed data plane is gated: the same principal's `store put` into the
  project store, the worktree store and a registry key was refused. Read-only held where an S3 key
  with read-only scope was used (8 of 8 writes refused). So read-only followers are enforced by
  the S3 key, not by the daemon, and one daemon is one trust domain. This does not change the two
  machine case where each machine holds its own scoped key; it means the "two principals" of gate
  item 1 on one daemon give no isolation.
  **Decided 2026-11-15 (owner):** for S2, one daemon is one trust domain. Isolation between
  principals comes from a scoped S3 key per machine: a follower machine holds a read-only key, and
  that key is what makes it read-only. Gate item 1's "two different principals" therefore means
  two machines with two keys, not two labels on one daemon. No code changes with this decision. A
  caller identity the daemon can verify, which several principals sharing one daemon would need,
  is separate work (board card 6f0bc9c8) and is not part of the gate.
  A follower daemon never registers the project's stores (the registry is per daemon, and its
  read-only key cannot create one), so its governed `store put` or `delete` of such a store is
  refused with "this node is not a writer of <store>", naming what would change that (the owner
  daemon's grant, or a write key held by this node), not with "unknown store". The refusal comes
  before any backend write, and the bucket probe that tells the two apart runs only on that
  refusal path; a store whose bucket does not exist still says "unknown store". That refusal shows
  the node holds no grant, not that the read-only key refuses writes: the key's own refusal is
  separate evidence (the 8 of 8 above).
- **Interrupted publish (finding 3a, board card 6b58589c). Decided 2026-11-15 (owner), done in lane D3.**
  Was: `publish_tree` overwrote a changed object in place before it wrote the new manifest, so a cut in
  between left the previous manifest naming overwritten bytes (`checksum mismatch`, one file of 20 at
  two of four cut points), and a follower holding the previous manifest through a completed publish hit
  the same mismatches and silently missing files. Now objects are content-addressed and immutable, the
  manifest write is conditional, and dropped objects are swept only a grace period later (section 4,
  "Content-addressed objects and the grace sweep"). Drill 3a reads clean at all four cut points
  (the sweep cut is now in the publish a grace later, the first that deletes); W1a/W1b serve the old
  bundle; W2a/W2b/W2c read every file of the previous manifest clean; the writer-kill drill leaves no
  follower refused.

**Running the drills.** `scripts/store/wo069-drills.sh` runs against a throwaway RustFS it starts on
127.0.0.1 (`WEFTOS_STORE_TEST_RUSTFS_BIN=<rustfs binary>`; a weaver built with
`scripts/build.sh native-debug`, or `WEFTOS_E2E_WEAVER`). `--only d1,d2,d3,lib,scale` picks drills
(default `d1,d2,d3,lib`, about 4 minutes); `--scale` adds the 20 000 and 50 000 file trees (the 50 000 tree now reads back and republishes under the default limits) (about
45 minutes); `--release` builds the library tests in release mode for latency numbers;
`WEFTOS_DRILLS_DATA_DIR` puts the server's data on the internal disk. With
`WEFTOS_DRILLS_RUSTFS_ENV=<0600 env file>` it uses a remote throwaway server instead (it refuses the
PG address); pass credentials by file path only. Never point it at PG or any shared RustFS.

### 5. Edge and Wasm nodes hold no long-lived keys

- **The node bucket is reached through the parent.** A node that is only a Wasm build on an
  edge device still has its node bucket, and reaches it through its parent daemon. The parent
  either proxies the request or hands out short-lived credentials: presigned URLs, or STS
  temporary credentials if RustFS supports them (verify in S0).
- **Long-lived access keys live only on full daemons and on people's machines** (via
  Terraform).

### 6. Governance and logging

- **Exposure:** the store is an ADR-120 exposure on its host, with the on/off switch.
  Switching it off withdraws it from the mesh.
- **Logging:** RustFS audit targets and bucket notifications go into the activity log,
  with the principal, the store name, the operation and the size. Every create, read,
  write, rename and delete is logged. S0 proves this; if a type of operation is missing,
  this ADR is amended.
  **Amended after S0:** the audit stream doesn't cover every operation. Refusals (403 with the
  account) and bad signatures go only to RustFS's own JSON log (`s3_authorization_denied`,
  `SignatureDoesNotMatch`), so the activity log takes both sources. Successful listings
  (ListObjects, ListBuckets) and multipart part uploads appear in neither, so listing a store
  isn't logged.
- **Removing a connection:** a rule hit disables that principal's access key, so its next
  request is refused. The daemon also drops its proxied sessions for that principal. A
  transfer already in progress may finish. That's accepted (owner).
- **Notifications drive cache invalidation:** on nodes and mounts, a bucket event tells
  rclone (`vfs/forget`) or the `weftos-store` client that an object changed.

### 7. The `weftos-store` crate

`weftos-store` is a new crate:

- **Name-to-bucket mapping** (§2) and the overlay resolver (§4).
- **An OpenDAL S3 client** used by the daemon, cogs and agents. Agents and cogs go through
  the daemon, so their operations are governed and logged. The ADR-122 blob store can keep
  large items here.
- **The control-plane client:** creates buckets and access keys and applies policies,
  through the RustFS admin API.
- **Weaver commands:** `weaver store ls|grant|revoke|promote|archive`.

## Phases

| Phase | Scope | Done when |
|---|---|---|
| S0 | Spike: RustFS on PG under the weftos account, through Terraform | verified: audit covers every operation type; time from disabling a key to refusal; RAM and disk on PG; the S3 operations we use (multipart, list v2, copy, conditional put); STS support; on-disk format (can the existing project dir be served in place, or must it be imported?) |
| S1 | `weftos-store` crate, node and project stores, grants, exposure, audit and notifications into the activity log | a node and a project store are created by the daemon; a write by a cog shows in the activity log; disabling a grant refuses the next request |
| S2 | Worktree stores: overlay, promotion on land, agent-git-flow hooks, desktop union mount | two agents on two machines in one worktree see each other's outputs; a new worktree sees the project's gitignored assets with no copy; landing promotes |
| S3 | Edge and Wasm nodes through the parent daemon, short-lived credentials | a Wasm node writes to its node bucket without holding a long-lived key |
| S4 | Release stores: CI writes once, retention, signed artifacts referenced by hash | a release build's artifacts land in its release store, and a rebuild can't overwrite them |
| S5 | Clients: one instance per client account under `/data1` as the client user; cross-mesh grants (ADR-114 N5) | a client project's store lives only under the client user, and a WeftOS-account principal is refused unless granted |

**Phase status (2026-11-09, 13:20 ET; `origin/target-0.8.5` settled at e58899bb0, pushed 13:00):**
- **S0** done (PG spike, "S0 results").
- **S1** landed: `aepod/feat/adr126-s1` 2a8a13272 merged as 68e010acf, macOS `private_dir` fix 905d327a9, the seven Mac `wt/adr126-*` slices, owner-approved diff-gate entries d26c0cbec (card t213s943), fast-forward tip 15dcf72c3. Done-when met against a throwaway RustFS 1.0.4; not yet run against the PG RustFS.
- **S2** landed: aepod's `feat/adr126-s2` d0db74233 rebased onto the landed S1 (`land/adr126-s2-on-s1land` cae1b0b25, reconcile 93a3836b5: Mac wins on overlap, S2 signing, retry, claim epoch, preview, quota, gc and switch replayed on top) and merged onto 15dcf72c3 as 7ba34b071.
- **S3** landed through S5: `feat/adr126-s3` fc9c081f7 was merged into S5's rebased tip (96c78e454), so the S3 code (`crates/weftos-edge-creds`, chain-fed grant ledger, compaction spike) is on `target-0.8.5` with S5; nothing of S3 was left to land on its own. Two S3 option branches stay unlanded on aepod: `prep/adr126-s3-reqwest-client` c9b85e569 (reqwest `ObjectTransport` for the STS minter behind feature `reqwest-client`; needs an owner diff-gate approval for the new dependency, do not merge without it) and `opt/adr126-nodekeys-adapter` 646b57ff5 (S3 `NodeKeys` over S1's `node_key` behind the default-off `edge-creds` feature; owner's 11:40 correction: `weftos-edge-creds` only behind a feature, the daemon runs without it).
- **S4** landed: `feat/adr126-s4` rebased onto 64c547f4a (`rebase/adr126-s4-on-64c547f4a`, 20 commits 35bcc00e8..3d4d52834: `ReleaseBackend` over `weftos-store` behind the `store` feature, Object Lock publish and restore, non-root CI keys, S1 lock read-back, extend-retention, lifecycle and multipart, Windows-host key rules, per-segment path cap, `weaver store release publish`) plus the owner's approvals e58899bb0 (`clawft-weave` depends on `weftos-release` with `store`; the `weftos-store` entry reworded, not test-only). That commit is the settled tip.
- **S5** landed: `feat/adr126-s5-on-s2-15dcf72c3` 64c547f4a on the S2 land (contains d26c0cbec and 15dcf72c3; approvals 3ddb1537e, 72be2ecc4 by weftos-s5).
- **Diff-gate approvals** on the tip: 17 entries (10 before S1, 4 for S1 d26c0cbec, 2 for S5, 1 for S4; the two S5 `weftos-release` entries were merged into the S5/S4 wording at e58899bb0).
- **Test slices on e58899bb0** (2026-11-09 PM, fresh worktree, cold target dir, targeted only): `weftos-store --features rustfs` 343 passed, 47 skipped (293/34 at 15dcf72c3; the S2, S4 and S5 tests account for the rise); `clawft-weave --filter store` 204 passed (94 at 15dcf72c3), one leaky test (`release_cmd::tests::store_names_parse_and_bad_ones_are_refused` left a child process behind; counted as a pass); `weftos-release --features store` 69 passed, 13 skipped; `clawft-weave --filter release_cmd` 7 passed. No live RustFS run and no gate here; both belong to the CI gate on `weftos-ci` (owner: CI is the record).
- **Not landed, waiting on the owner:** `wt/adr126-s1-names` (the bucket rename `n-<id>` to `node-<id>` and the pinned bucket creator; reconciled with a copy-forward migration on `w2/t-names-and-conflicts` 9d90ee210, local, pile T; RustFS user names `node-<id>` were decided on 2026-11-08 and are not what waits); the union mount parking proposal ("S2 status"); the snapshot cadence and where the snapshot event sits ("Chain compaction", switch 2: no cadence is approved and nothing sets one); `feat/adr131-d1` (ADR-131, weftos-d1's lane) lands after this.
- **Not done anywhere:** nothing is pushed beyond `origin/target-0.8.5` itself, nothing is tagged, released or deployed; no store-capable weaver is installed yet (the store-capable install stop itself was lifted by the owner at 19:44 ET, next bullet); the PG RustFS has not run any S1 to S5 code.
- **Found 2026-11-10 by the Tapestry E3 run (item 10); resolved by `feat/store-file-mode` (merge 72a98d05c on target-0.8.5, under ADR-131 Accepted 2026-11-10):** the store keeps no file modes and `drive` materialises every base file 0444 (the view tests at 88b2fb172 assert it), so a root image materialised as a view has nothing executable and a bwrap/Landlock launch fails at `execvp /bin/sh`. With a test-only `chmod +x` on `base/` after open, the full E3 cycle passed (read, write, create, delete under `/work`; publish 2 puts, 1 delete, 0 conflicts; close releases the lease; objects correct, delete tombstoned). Needed: preserve or declare the exec bit. **Owner answer 2026-11-10 17:08 ET** (`instructions/20261110-1708-backend3-exec-bit-object-metadata.txt`, addressed to the Tapestry lead): record each file's mode in the object metadata, not a list in the view spec; store the mode on put, apply it when the view materialises base files (write bits stay off, exec bits preserved); update the 0444 view tests and add exec-bit round-trip tests; targeted per-crate tests only; then rerun E3 on `weftos-ci` with a weaver from that branch without the chmod workaround and record it in `scratch/p3-e3/results.md`; report any diff-gate approvals needed, do not touch the approvals file. Still in force: no push, tag, publish or release; hub branches unmerged (D1); do not merge hub-passkey; R8 waits for ADR-131. **Being implemented by the Tapestry lead** on WeftOS `feat/store-file-mode` off 88b2fb172, worktree `/Volumes/ai-models/weftos-worktrees/store-exec-bit`, local only (legacy objects with no mode stay 0444). This lane does not start a parallel branch; the result arrives as a delta for this row. Minor from the same run: path-guard refusals are unclassified (by message text); `limits` and `bucket` are only sent when `--spec` carries them. Evidence: agent-notes `scratch/p3-e3/results.md`; fork branch `tapestry/p3-item10-real-view` 2461d3ab (local). The run was a test container on `weftos-ci`, not an install. **Fixed 2026-11-10 by the Tapestry lead on `feat/store-file-mode` 367bed4e7 (one commit on 88b2fb172, 20 files; landed as merge 72a98d05c on 7311b9443):** object attributes as above; weftos-store 310/310 and clawft-weave `store_` 161/161 on `weftos-ci`, 7 tests fail with the mode logic reverted; diff gate 0 failed, 0 approved, no approvals needed; **E3 rerun without the workaround passes** (the sandbox runs `/bin/sh` from a 0555 base; publish 2 puts, 1 delete, 0 conflicts). Not covered: refresh ignores a mode-only remote change (same etag and size); conflict copies and `Keep::Remote` drop the attribute; multipart and Object Lock puts drop it; live RustFS (incl. CopyObject metadata) and macOS/APFS untested. **Closed except macOS by `feat/store-attrs-followups` d989f426d** (pile A, 4 commits on 431e3060c, landed in c00fe351b; see the 2026-11-10 pile A Updated line). Landing is an owner call.
- **S2 / E-1 view, file modes (execute bit):** carried as of `feat/store-file-mode` 367bed4e7, landed on target-0.8.5 as merge 72a98d05c under ADR-131 Accepted 2026-11-10.
- **Found 2026-11-10 evening by Tapestry E3 round two (fork `tapestry/p3-item10-real-view` 42039706, evidence agent-notes `scratch/p3-e3b/results.md`), open, verbs owner:** all five owed cases pass on 7311b9443 (overlay open/write/publish, conflict and resolve, refresh with remote changes, lease renew/expiry/takeover, Landlock proxy loopback only), but (1) overlay `lower/` files come out 0444 (`drive/view/tests.rs` asserts it) and copy-up keeps that mode, so editing an existing file in place fails until `chmod u+w`; the fix is `lower/` at 0644/0755 per the stored mode with only `base/` read-only, a code change **approved by the owner 2026-11-10 20:11 ET**, **fixed by the Tapestry lead on `feat/store-overlay-lower-writable` 431e3060c off 7311b9443** (lower dirs 0755, files 0644, executables 0755; base unchanged; weftos-store 313/313; diff gate 0 failed, 0 approved; E3 overlay rerun passes with no chmod; landed in c00fe351b; not established: RustFS, macOS; real overlayfs copy-up into an existing lower directory was established by Tapestry pile C, fork `p3-item10-real-view` 467968ba, 6 of 6 e3b cases on a weaver from 431e3060c and a loud failure on 7311b9443); (2) `weaver store put` has no `--as <principal>` for scripted remote edits (**closed by pile B `feat/store-verbs-polish` 54aaa95aa**, landed in c00fe351b with the fallback removed again by 1e6b9d2ab, owner's call: `--as` existed and was required, the branch adds the `$WEAVER_STORE_AS` fallback, flagged to the owner as an ambient default and **not approved by the owner (2026-11-10 22:02 ET)**, so the fallback must come out before pile B lands; the same branch classifies view refusals and adds the `limits` flags); (3) status JSON `work.open_conflicts` is empty while the top-level `open_conflicts` is filled: **closed, not a bug** (pile A: the E3b note was a mis-read of a grep, there is no top-level key; a shape test pins it). Test container on `weftos-ci`, not an install.
- **Store-capable by default (2026-11-09 PM, checked with git at `origin/target-0.8.5` 88b2fb172):** `clawft-weave` depends on `weftos-store` (`rustfs`) and `weftos-release` (`store`) unconditionally, so since S1 every weaver built from `target-0.8.5` is store-capable and the owner's install stop applies to any build from the tip. Raised with the owner by the Tapestry lead. **Owner decision 2026-11-09 19:44 ET: change the rule, not the code.** Store-capable weavers are OK to install. Context he was given: `origin/target-0.8.5` 88b2fb172 cannot build a weaver without the store code, because since S1 `clawft-weave` always pulls in `weftos-store` and the store-enabled `weftos-release`. He chose to lift the install rule rather than make store support optional; store support is not to be made optional. What this lifts is the store-capable condition only; the other stops recorded here (no tag, no push, no install over the production weaver before the full RustFS plus Tapestry release) were not mentioned and are read as still standing until the owner says otherwise.

## S0 results (2026-10-29)

These come from a spike on PG: RustFS 1.0.4, single drive, run as `weftos`. The Terraform is
branch `tf/rustfs-spike` in `weftos-weavelogic.terraform`, with the runbook in
`docs/rustfs-spike.md`. The client was rclone 1.75.2 on the Mac, using a test user whose
policy covers only `s0-spike`.

| Check | Result |
|---|---|
| Audit coverage | Needs `RUSTFS_AUDIT_ENABLE=true`, which defaults to off. With it on, every event carries the access key, client IP, operation, bucket, object, status code and timing, with auth headers redacted. **Logged:** PutObject (including 412s), GetObject (including presigned), HeadObject, CopyObject, DeleteObject, CompleteMultipartUpload, CreateBucket. A rename shows as copy plus delete. **Not in the audit stream:** ListObjects, ListBuckets and UploadPart, which aren't logged anywhere, plus 403 refusals and bad signatures, which go to `rustfs.log` only. |
| Policy | The test user is refused (403) on any other bucket, for both create and list. |
| Revocation | `admin user disable`: the next GET, about 0.22 s later, gets a 403. A 1 GB multipart upload in progress fails at its next part (403) and is never completed. Re-enabling takes effect at the next request. |
| STS | AssumeRole issues 15-minute temporary credentials, and a GET with them returns 200. §5 can use STS for edge and Wasm nodes. |
| Conditional put | `If-None-Match: *` returns 412 on an existing key and 200 on a new one. |
| On-disk format | RustFS's own (`xl.meta`, as in MinIO). Small objects are inlined, large ones stored as `part.N` files. **Answer to open question 1: the existing project directory can't be served in place; it's imported once.** |
| Footprint | The `rustfs` process: about 310–320 MB RSS, idle and under a 1 GB upload. The service cgroup reached 1.5 GB after the upload, which is mostly the kernel's page cache. The audit sink: 19 MB. An hour-long reading is still to do. |
| Restarts | Objects are intact after `restart rustfs.service` (the md5 of the 1 GB object is unchanged). With the audit sink stopped, events queue on disk and are delivered once it's back. |

**Audit follow-up (2026-10-29, source read of the 1.0.4 tag plus a re-test on PG):**

- **Why the gaps exist.** Audit is emitted per handler, through `OperationHelper`
  (`rustfs/src/storage/helper.rs`). A handler without one emits nothing, and the helper only
  exists for operations that map to a notification event name. About 43 operations map to
  none (`crates/s3-ops/src/lib.rs`). No code comment, doc, issue or PR says this is a
  deliberate scope. It reads as an oversight that comes from tying audit to notification
  events.
- **Not audited, confirmed on PG:**
  - UploadPart and AbortMultipartUpload;
  - PutBucketPolicy and DeleteBucketPolicy;
  - PutBucketVersioning;
  - PutBucketTagging and DeleteBucketTagging;
  - HeadBucket.

  The unaudited policy and config changes are the larger governance gap.
- **CreateMultipartUpload** is emitted but then lost. With a queue directory set, the queued
  event fails to replay ("Invalid event name: s3:ObjectCreated:CreateMultipartUpload") and is
  dropped (`audit_replay_dropped`). That is a RustFS bug.
- **Refusals** are decided in RustFS's `DenialContext::deny` (`rustfs/src/storage/access.rs`),
  which only writes the `s3_authorization_denied` log line. That line has the account,
  bucket, object, action and reason. It's a gap nobody designed, and RustFS owns the hook.
- **Bad signatures** are rejected inside s3s `prepare`, before RustFS sees the request. That
  needs a response-status layer or an s3s change.
- **MinIO**, which RustFS mirrors, audits UploadPart, Abort and middleware rejections, so
  RustFS diverges from it here.

Not tested: bucket notifications (not configured in the spike), and erasure coding and
replication (single drive).

## S1 status (2026-11-07; landed on `target-0.8.5` 2026-11-09)

**Landed.** `aepod/feat/adr126-s1` 2a8a13272 (41 commits, findings through 1153af0a) was merged on 2026-11-08 as
68e010acf on `integ/adr126-s1-land` off `target-0.8.5` f6af34678, followed by a macOS fix 905d327a9 (`private_dir`
read `/proc/self`, which macOS lacks; now a probe-file uid), the Mac slices `wt/adr126-s2-mac`, `-s2-meta`,
`-s2-hooks`, `-s3-drive`, `-s3-harden2`, `-e1-view-verbs` (contains `-e1-scoped-view`), and the owner's diff-gate
approvals for the receiver and the canned test servers (d26c0cbec, card t213s943). `target-0.8.5` was fast-forwarded
to it on 2026-11-09 (15dcf72c3). On that tip: `weftos-store --features rustfs` 293 passed, 34 ignored;
`clawft-weave --filter store` 94; live on a throwaway loopback RustFS 1.0.4: weftos-store 27/27 and the four weave
store live tests; `scripts/diff-gate.sh --base target-0.8.5` 0 S1 rows failed. S2, S5 (with S3) and S4 landed on top
the same day; the settled tip is e58899bb0 ("Phase status"). Nothing deployed; no store-capable weaver installed at the time of writing, and the owner lifted the store-capable install stop on 2026-11-09 at 19:44 ET ("Phase status").
Below is the build record as written on 2026-11-07/08.

Built: name-to-bucket mapping, grants (recorded on the chain, pushed into the backend as access
keys and IAM policies), the exposure per store (`store.<bucket>`, ADR-120), governed
get/put/head/delete/list with every request and refusal in the exposure activity log, parsers for
RustFS audit, `s3_authorization_denied` log lines and bucket notifications, promote, archive, and
`weaver store ls|create|grant|revoke|promote|archive|put|get`. The daemon creates its node store at
start. Tests: `weftos-store` (32) and the `store` tests in `clawft-weave`.

**RustFS transport (2026-11-07, owner approved the HTTP client and the `weftos-store` dependency):**
`RustfsBackend` (feature `rustfs`, `crates/weftos-store/src/rustfs.rs`, `sigv4.rs`) implements both
traits over blocking HTTP with SigV4 signed by the root credential: the S3 data plane (put with
`If-None-Match`, get, head, delete, copy, list v2 with paging, buckets and tags) and the admin API
(`/rustfs/admin/v3`: add-user, set-user-status, add-canned-policy, set-user-or-group-policy,
remove-user, remove-canned-policy). The daemon selects it when `WEFTOS_STORE_RUSTFS_ENV` names an
env file (`MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`, optional `MESH_S3_REGION`);
each principal's key id is `wk-<16 hex of blake3(principal)>` and its secret is kept 0600 under
`<runtime dir>/store-secrets/`. It is called from blocking contexts only (the weave verbs already
use `spawn_blocking`; start-up runs it on a plain thread).

Verified against a throwaway RustFS 1.0.4 on loopback (not PG), by `cargo test -p weftos-store
--features rustfs -- --ignored` with `WEFTOS_STORE_TEST_RUSTFS_ENV` set:
- object and bucket operations, paging past 1000 keys, tags, a 412 on a conditional put;
- a scoped key is refused outside its bucket, and **a disabled key's next request is a 403**;
  re-creating a key keeps its secret and its disabled state; no grants becomes a deny-all policy
  (RustFS rejects a policy with no statements: HTTP 500);
- the S1 done-when end to end: node and project stores, a cog's write in the activity log, and
  disabling the grant refuses the next request at the service and at RustFS.

Findings that shaped the code: creating a bucket the root already owns returns 200 (so
`create_bucket` checks first); a bucket name needs 3 or more characters; headers such as
`If-None-Match` must be signed; `ObjectMeta.etag` is the backend's etag (MD5 on RustFS, BLAKE3 on
the local backends).

Audit parsers: the `s3_authorization_denied` line was captured from RustFS 1.0.4 and the parser fixed
to match (`action` is `S3Action(GetObjectAction)`, `object` can be `"None"`). **Audit stream and notifications verified (2026-11-08)** against a throwaway loopback RustFS 1.0.4, and the
parsers match the captured records. The earlier "no record" runs failed because RustFS rejects loopback webhook endpoints unless
`RUSTFS_OUTBOUND_ALLOW_ORIGINS=http://127.0.0.1:<port>` is set at start; targets must be named
(`RUSTFS_AUDIT_WEBHOOK_ENABLE_<NAME>`, `..._ENDPOINT_<NAME>`, and the same for `NOTIFY`), and `RUSTFS_AUDIT_ENABLE=true` is
still needed. The admin `set-config-kv` call silently drops `enable` and `endpoint` for audit targets, so it is not a way to
configure them. Notifications additionally need a `PutBucketNotification` pointing at `arn:rustfs:sqs::<name>:webhook`. An
audit post is `{"EventName","Key","Records":[{"event","trigger","api":{"name":"s3:PutObject","bucket","object","status_code",...},"accessKey",...}]}`
with a plain object key and no size. A notification post is a standard S3 event record, also with `EventName` and `Key`, whose object key is
percent-encoded. `parse_audit` was wrong before the capture (it returned `ObjectCreated:Put`, not `PutObject`) and now reads
`Records[0].api`; `parse_notification` decodes the key. Not yet verified: batched posts (`batch_size` above 1), multipart and
versioned operations, and `auth_token`. **Not yet done:** the daemon does not launch or configure RustFS (it is an external
service), so setting those env names and issuing `PutBucketNotification` belongs to whoever deploys RustFS (Phase A):
`docs/guides/rustfs-audit-deploy.md` is the runbook and `scripts/store/rustfs-audit-plan.sh` a dry run that prints it
(`RustfsBackend::put_bucket_notification` is the call). **Conditional write (for S2).** `ObjectBackend::put_if_match(bucket, key, data, etag)` is a compare-and-swap (S3 `If-Match`): a
changed key is `BackendError::Stale`, a missing key is `NoObject`. The service has `StoreService::put_if_match(who, store,
key, data, etag)` (gated and logged like `put`), `svc.store` takes `"if_match": "<etag>"` on `put` (`store.v1`, not with
`if_none_match`), and RustFS gets `If-Match`. Take the etag from the last `head` or `put` result of the same backend
(opaque: MD5 on RustFS, BLAKE3 locally), and on `Stale` read again and retry. The local backend serialises swaps within the
daemon process; across processes only RustFS is atomic. S2 should use this instead of unconditional `put` for any
read-modify-write.

**CI and sweeper roles (release stores).** Two narrow access levels exist besides read, write and admin: `append` (get, head,
list, and a put that must carry `if_none_match`, so nothing is overwritten; no delete or copy) and `sweep` (get, head, list and
delete; no put). A new release store gets the well-known principals `agent:ci` as `append` and `agent:store-sweeper` as
`sweep` by default (`weaver store grant <name> <principal> append|sweep`). Proven live on a throwaway RustFS 1.0.4: the `append` key can read and list, and is refused put, copy and
delete when used straight against RustFS; the `sweep` key reads, lists and deletes and is refused put. RustFS IAM cannot state
write-once (it rejects the `s3:if-none-match` condition key, and a plain PutObject allow lets a key overwrite or copy over an
object), so CI's writes go through the daemon, which makes them `If-None-Match: *` with its own key and refuses an overwrite,
copy or delete for an `append` principal. A CI job therefore writes artifacts through `store.put`/`svc.store` or a daemon-side
client, not with an S3 SDK and its own key. Immutability on the backend itself and retention are S4's. Readers of a release store (the project's
members) are named when the store is made.

**Object Lock (for S4), proven live on a throwaway RustFS 1.0.4 (2026-11-08).** `AdminBackend::create_locked_bucket` makes a bucket with
`x-amz-bucket-object-lock-enabled` (versioned; chosen only at creation; the local backends answer `Unsupported`).
`ObjectBackend::put_locked(bucket, key, data, &PutOpts{if_none_match, retention: Some(Retention{mode: Governance|Compliance, until_unix_s}), legal_hold})`
sends the retention and legal-hold headers and returns the written `version_id`; `delete_version(bucket, key, version_id, bypass_governance)`
and `put_legal_hold(.., on)` act on one version. Seen on RustFS: a bucket made without lock refuses lock headers; a locked
version's delete is refused with `AccessDenied` under COMPLIANCE (bypass does not help), under GOVERNANCE (without bypass), and
under a legal hold until it is lifted; a past retain-until date is refused before sending. At the service: `create_locked_store(name,
actor, parties)`, `is_locked(name)` and `put_locked(who, store, key, data, &opts)` (gated and logged like `put`; an `append` principal must
set `if_none_match`; a store not made locked refuses it). **Listing versions (for S4's replaced-manifest check, live 2026-11-08):**
`ObjectBackend::list_versions(bucket, prefix)` and `StoreService::list_versions(who, store, prefix)` (needs read, logged as a list)
return every version and delete marker as `ObjectVersion{key, version_id, is_latest, is_delete_marker, size, etag, last_modified}`, sorted
by key and newest first. A manifest put once with `if_none_match` shows one entry; a later plain put under the same key shows a
second, newer entry with a different `version_id` and `etag` (the first keeps the etag its put returned), so S4 compares the
newest entry to what it recorded. Local backends answer `Unsupported`. Paging was checked by forcing a page of 7 over 30 keys.
Read keys now carry `s3:ListBucketVersions`, and a Read key listed versions at RustFS. A plain delete (no version id) in a locked bucket,
as the sweeper does, adds a delete marker: the listing shows the marker newest and the retained version beneath it, and a get answers
not found. **Not covered:** default bucket retention (`PutObjectLockConfiguration`) and `PutObjectRetention` on an existing version are not
implemented; `archive(delete)` of a locked store will fail on its retained versions; the key policy for `sweep` still allows only
`s3:DeleteObject`, not `s3:DeleteObjectVersion`/`s3:BypassGovernanceRetention`, which is what you want for retained versions.

**Owner decisions on the release roles (2026-11-08). Each is ready as a switch, tested both ways; the defaults are the safe choice.**

| Decision | Switch | Off (default) | On | Cost of turning it on |
|---|---|---|---|---|
| Sweeper may put | `WEFTOS_STORE_SWEEPER_PUT=1` (`PolicyOpts.sweeper_put`) | sweeper key: read, list, `s3:DeleteObject` | adds `s3:PutObject` and multipart actions | the key can overwrite or copy over any artifact; in a locked store the overwrite adds a version and the retained one stays (live: 2 versions, delete of the original still refused) |
| CI uploads straight to RustFS | `WEFTOS_STORE_CI_DIRECT_PUT=1` (`PolicyOpts.ci_direct_put`) | CI key: read only; uploads go through the daemon (`store.put` / `svc.store`), which makes them write-once | adds `s3:PutObject` to the `append` key | RustFS IAM cannot express write-once (it rejects `s3:if-none-match`), so the key can overwrite and copy as well as create; delete stays refused; again only a locked store keeps the original |
| Receiver diff-gate approvals | none; owner appends `docs/guides/rustfs-store-ingest-approvals.draft.toml` to `config/diff-gate-approvals.toml` | gate fails on `net` (listener, test sockets) and `process` (live test spawns RustFS) | all store_ingest findings become "approved" (checked on a scratch copy) | the entries are narrow (this file, these patterns), no other finding changes |

The switches widen only the direct S3 key; the service gate is unchanged (a sweeper `put` through the service is still refused, and an
`append` put still has to be write-once). Set the env var in the daemon's environment, restart, and the daemon pushes the new policy to
existing keys at start (`reconcile`); `StoreService::set_policy_opts` then `reconcile()` does the same at run time. Tested: policy unit
test (off, each switch alone, back off), service test (backend key policy after reconcile, both ways), and a live test on a throwaway
RustFS 1.0.4 that flips both switches over a locked release store. Recommendation: leave both off. A locked release store with CI
uploading through the daemon gives write-once, retention and no overwrite path; each switch trades that for convenience.

**Transport hardening (2026-11-08).** `BackendError::Unavailable` (with `is_transient()`) now means the backend, not the request, was the
problem: no connection, or HTTP 408/429/500/502/503/504 after retries. 401 maps to `Forbidden`; a 400 stays `Io` and is not transient.
`RustfsBackend` retries 3 tries in all with waits of 100 ms then 400 ms (`Retry{attempts, base}`, `with_retry`; waits grow fourfold, cap 5 s).
It repeats only what is safe: a connection that never opened (any request), a 503/429/408 (the server says it did not act), and for
reads and plain deletes also 500/502/504 or a dropped connection. A timed-out or dropped write is not repeated, nor a delete of a
named version, since a repeat could add a second version in a locked store or turn a conditional create into a false "exists".
Tested without sockets (decision table, wait schedule, unreachable port, status-to-error map). Not done: no circuit breaker, so a
dead RustFS costs 3 quick tries per call; a timeout still holds a caller for the full 30 s.

**S3/S4 pile (2026-11-08), one commit per slice, each with unit tests and a live test on a throwaway loopback RustFS 1.0.4.**

| # | What | API | Seen on RustFS |
|---|---|---|---|
| 1 | Read back Object Lock config | `ObjectBackend::get_lock_config(bucket) -> LockConfig{enabled, default}`, `put_default_retention`, `StoreService::lock_config(store)` | plain bucket: `enabled:false` (404 `ObjectLockConfigurationNotFoundError`); locked: `enabled:true`; a default retention reads back and a put with no headers inherits it |
| 2 | Lifecycle rules | `AdminBackend::put_lifecycle(bucket, &[LifecycleRule])` (empty list removes), `get_lifecycle`, `StoreService::set_lifecycle(store, rules, actor)` (chain `store.lifecycle`) / `lifecycle(store)`; rule = id, prefix, enabled, `expire_days`, `noncurrent_days`, `abort_multipart_days` | written and read back identically, no Content-MD5 needed; Object Lock still wins over an expiry |
| 3 | Extend retention after commit | `get_retention`, `put_retention`, `extend_retention(bucket, key, version, until, mode)` (only lengthens, keeps the current mode), `StoreService::extend_retention(who, ..)` (the `append` role may call it: it only adds protection) | an object put with no hold is stamped COMPLIANCE, lengthened, read back; RustFS refuses to shorten it even with bypass |
| 4 | Multipart upload with lock headers | `create_multipart`/`upload_part`/`complete_multipart`/`abort_multipart`, `put_stream(bucket, key, reader, part_size, &PutOpts)` (8 MiB parts, one part in memory), `StoreService::put_stream(who, store, key, reader, &opts)` | 11 MiB in three parts, retention applied to the finished object, bytes identical, `If-None-Match` enforced at complete, abort leaves nothing; the 512 MiB whole-body limit no longer applies; local backends still read it whole and refuse lock options |
| 5 | Retire a node on unpair | `StoreService::retire_node_store(node_store, actor)` and RPC `store.node.retire`: archives the store, records the id as retired (persists), `create_store`/`ensure_store` refuse it | data lands in the archive bucket, grants off, id refused |
| 7 | Dead RustFS | `Breaker{threshold 5, cooldown 10 s}` (`with_breaker`, `breaker_open`), `with_timeout(d)` view sharing the breaker | after 5 failed-to-connect requests in a row calls fail at once with `Unavailable("circuit open")` for the cooldown, then one probe; any HTTP answer (403, 404, 500) closes it |

Slice 6 (S3's ask) is in `clawft-kernel`, not the store: the exposure activity log (`ActivityLog`) did `next_seq + 1` unchecked, a panic in
debug and a wrap to 0 in release at `u64::MAX`, and `open()` did `last.seq + 1` on a file, so a log whose last record was `u64::MAX` could not be opened. Now `u64::MAX` is a
usable last number; after it, `append` keeps nothing, returns the record with `seq` `u64::MAX` and an empty `hash`, counts it in `dropped()`
and logs once; `is_exhausted()` says so; `open()` of such a file opens exhausted; and `verify` rejects a sequence that repeats or steps back.
Reaching it takes 1.8e19 appends, so this is a guard against a damaged or hostile file, not an expected state.

Notes: `put_stream` through the service logs the real size. A multipart part PUT is retried (same part number replaces the part); nothing
else that writes is. The breaker counts only "could not reach RustFS" (no connection, timeout, 502-504), not a 500 or 429.

**Follower wake hook in the receiver (S2's ask, 2026-11-08).** `store_ingest::WakeSink::notified(store, bucket, body)` is called after a bucket
notification has been logged, once per store of this daemon whose bucket the body names (other buckets, the audit stream, bad bodies and wrong
tokens never reach it; a panic in the sink is caught and does not change the answer to RustFS). Pass one with `serve_with` /
`start_from_env_with`; the daemon passes none yet. `weftos_store::wake::registry_hints` lives on S2's branch, not here, so the sink is the
seam: after the two branches merge, S2's implementation is `fn notified(&self, _store, bucket, body) { let h = registry_hints(body, bucket);
if !h.is_empty() { /* publish a wake on the project's relay topic */ } }`. The relay hop itself is the mesh transport card and is not started.

**Grant event sequence: where it is stamped and what bound applies (decision for the chain owner to confirm, 2026-11-08).** S3's review found
that the `seq = u64::MAX` freeze was not closed: my earlier guard covered the exposure activity log, a different sequence, and the ledger that
S3 feeds from the chain (`GrantLedger::apply` ignores any event with `seq <=` the last applied) would stop applying all later events,
revocations included, after one event claiming a huge `seq`. The decision, in `weftos_store::grant_seq` and `StoreService`:

1. **Stamped by `StoreService`, the one writer of grant changes**, not by the chain feed and not by the consumer. A counter `grant_seq` is
   part of the saved state and goes up by exactly one for each event that changes what a principal may do: `store.grant`, `store.revoke`,
   `store.grant.enable`, `store.grant.disable`, `store.principal.suspend`, `store.principal.resume`, `store.close` (all grants of a store
   went off at the start of an archive), `store.archive`, `store.node.retired`. It starts at 1, is gapless, is saved before the event is
   recorded, and is never reused after a restart.
2. **Stamped under the same lock as the change, and recorded to the chain before that lock is released**, so the order of the numbers is the
   order the changes were made and the order the chain sees them (two threads cannot record 6 before 5). The `ChainSink` therefore must be quick
   and must not call back into the service. A test makes 200 grants from 8 threads and requires the chain to see 1..=200 in order; moving the record out of
   the lock fails it.
3. **Upper bound on issuing: `CEILING = 2^62`.** At the ceiling every grant-affecting call is refused with a clear error before it changes anything; a state
   file that claims a `grant_seq` above it is refused at open as damaged.
4. **Bound on accepting, for every consumer: `grant_seq::admit(last, seq)`** is `last < seq <= last + MAX_GAP` and `seq <= CEILING`, with
   `MAX_GAP = 1_000_000`. A forged or corrupt `seq` far ahead is refused without moving `last`, so the next honest event (`last + 1`) still applies.
   The gap is allowed because a consumer may legitimately miss events (a feed filtered to its own principals, or an outage it repairs); 10^6
   is far above any real burst and far below the ceiling. The bound limits the harm of a forgery that slips under it to at most `MAX_GAP` honest events being treated as stale, not a permanent freeze. A consumer that is refused must resync from a snapshot, not widen the bound.
   For S3: `MaxSeqJump(MAX_GAP)` is the same rule except for the ceiling, so install `set_seq_guard` with a guard that calls `admit`, by default,
   and again after every `restore`. S3's edge-creds crate does not depend on `weftos-store`, so the two numbers may be copied; they are public constants.

What is NOT closed here: S1 stamps and bounds; whoever builds the chain feed must carry `seq` through (the field is in the chain payload now),
and the ledger must install the guard. Events recorded before this change have no `seq`. Reasoning for the chain owner to confirm: a bound only
at the consumer is not enough (a forged value just under it moves `last` far ahead and the ledger then accepts nothing honest until it passes that
value), so the stamp must come from one trusted writer and be gapless, and the consumer's bound is then a check that the writer's promise holds.

**S3 review of the S1 commits, fixes (2026-11-08).** HIGH 1 (retire after a cut-short archive reported success): `archive` now records when it
started and whether it finished (`archive_ts`, `archive_done`, `archive_count` on the store record), so a retry resumes under the same archive keys, and
`retire_node_store` returns `Ok` only when every object is copied, with the real count on a repeat. HIGH 2: see the grant event sequence above.
Also fixed from the same review: the breaker no longer counts a timeout from a short `with_timeout` view (the caller's impatience is not news
about RustFS), frees a probe slot after a local fault without closing the breaker, and gives up on a probe that never reports back after one cooldown;
the retired-id check in `create_store` is repeated under the lock that records the store (a retire landing in between wins, and the bucket just made is
removed); an `append` principal's `extend_retention` is capped at `APPEND_MAX_RETENTION_S` (5 years), since a Compliance hold cannot be undone by
anyone (write and admin are not capped; the cap is a policy number for Mathew to change); retain-until dates past year 9999 are refused before sending and `parse_iso`
rejects years outside 1970..=9999 and impossible days (Feb 31); `put_stream` on a backend without multipart refuses over 512 MiB and clamps `part_size` to 5 MiB..1 GiB.
Not changed: a failed `abort_multipart` after a failed upload is still silent (the upload is orphaned until the `abort_multipart_days`
lifecycle rule removes it, so set that rule on stores that stream large objects); the review's default-build note stands (`--features rustfs` is needed to
compile and test `rustfs.rs`).

**Archive retention (S1, for node, project and release stores; 2026-11-08).** Worktree archives are S2's (`store worktree gc --archive-keep-days`). For the rest,
`StoreService::archive_gc(&ArchiveKeep{node_days, project_days, release_days}, confirmed, actor)`, RPC `store.archive.gc`, CLI `weaver store archive-gc
[--node-days N] [--project-days N] [--release-days N] [--confirmed]`:
- **Nothing expires unless a number of days is given for that kind**: an archive is recovery data, as with S2's default. Zero days is refused (it
  would delete an archive the moment it was made).
- **A dry run unless `confirmed`**; the report lists each expired set (bucket, kind, age in days, objects, bytes), what was kept, and what was left alone.
- **Whole sets only.** The archive bucket keys are `<bucket>@<archive ms>/<key>`; a set is one archive of one store. A set is deleted when its age is at least
  the days for its kind, never partly, and a half-deleted set is finished by the next run. Worktree sets (`w-`), unknown prefixes and keys that are not archive keys are
  never touched (counted as `not_ours`).
- **An archive that is still being written is never deleted**, however old: a store record with `archived` set and `archive_done` false protects the set
  whose start time it holds (found by a mutation test).
- A deleting run records `store.archive.gc` on the chain (not a grant event, no `seq`). The store record, and the live bucket of a store archived but not deleted,
  are not the archive's to remove and are untouched.
Not done: no schedule (an operator or a timer runs it), and no per-store override. The one question left for Mathew is the numbers; none is set.

**Archive retention, scheduled and per store (S1, 2026-11-08).** Both are off and carry no day numbers; the numbers stay the owner's call.
- **Per-store override.** `set_archive_keep(store, Some(Days(n)) | Some(Forever) | None)`, RPC `store.archive.keep {name, days? | forever?}` (neither clears it), CLI
  `weaver store archive-keep <name> [--days N | --forever]`. It beats the kind's limit in both directions (shorter, longer, or exempt) and works when the kind has no limit.
  Zero days, an unknown store and worktree stores (S2's) are refused. It lives on the store's record (saved with the rest) and is a chain event, `store.archive.keep`.
- **Schedule.** Off unless `WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS` is set (at least 1). The days come only from `WEFTOS_STORE_ARCHIVE_{NODE,PROJECT,RELEASE}_DAYS` (a kind left out
  keeps for ever). Unless `WEFTOS_STORE_ARCHIVE_GC_CONFIRM` is `1`/`true`/`on`, a scheduled run only reports (to the daemon log). The first run is one interval after start
  (the last-run time is in memory); a failed run is retried at the next interval, not in a loop; a due run with no day numbers and no override does nothing, not even a listing.
  A bad value is logged and leaves the schedule off, it does not stop the daemon. Scheduled deletes are chain events with `actor: "schedule"`.

**Owner decision, 2026-11-08 11:21 ET: nothing expires yet.** Archive retention stays at its default (no expiry). The schedule stays off, no day numbers are set anywhere, and nothing is scheduled to delete archives; `archive_gc` and the per-store override exist for when the owner sets numbers.

**The daemon now has the receiver** (`clawft-weave/src/store_ingest.rs`, 2026-11-08): a loopback HTTP listener, off unless
`WEFTOS_STORE_INGEST_ADDR` is set, POST only, 1 MiB cap, authenticated by a 256-bit secret in the URL path
(`store-ingest-token`, 0600; the URLs are written to `store-ingest-urls`), because RustFS's webhook `auth_token` is unverified.
It feeds `ingest_audit_line` and `ingest_notification`, which now drop records for buckets that are not the daemon's stores and
the daemon's own root-key requests (already logged on the governed path), so a record reaches the activity log only if it came from a
principal's own key. Notifications still invalidate caches whoever wrote. Verified end to end against a throwaway
RustFS (`live_a_request_straight_to_rustfs_reaches_the_activity_log`, `--ignored`, `WEFTOS_STORE_TEST_RUSTFS_BIN`): a put made
straight to RustFS with a cog's key shows as that cog's `PutObject` (audit) and a notification event. Not wired yet: attaching
`PutBucketNotification` to buckets automatically when a store is created (the runbook does it by hand). Reading `rustfs.log` for
refusals is wired (see the close-out findings below).

**Adversarial review of S1 (2026-11-08), fixed:** a grant change saved but not pushed (RustFS down, a crash between the two)
is now made good by `StoreService::reconcile()` at daemon start, and backend pushes are serialised per service so an older snapshot
cannot undo a newer policy; a bucket made but never recorded is adopted when its tags are the store's own (a foreign bucket still
refuses); an archived store's grant cannot be switched back on; `svc.store get` checks the size before reading the object; the
cog service startup no longer lists objects on the async runtime; `weaver store put|get --as` now leaves a `store.as` chain
record naming the operator. **Known and not fixed:** `archive` can lose an object put by a request that passed the gate just
before it; listings and `FsBackend` etags read whole files (the local backend is the development path); a mesh-id change
renames the stores' `weftos://` names (the bucket names do not move; the migration is F17 below); `project add`'s store hook
joins a blocking thread; (the receiver now authenticates and drops unknown buckets; see above).

**Close-out findings F2, F9, F15, F16, F17, F18 (2026-11-08, owner order of 15:00), fixed on `feat/adr126-s1`:**
- **F2** Every grant change (grant, revoke, enable, disable, suspend, resume, archive's close) runs under one lock with its
  backend push. The chain event and activity lines are recorded only after the push succeeded; a change whose push failed
  stays saved (the service enforces it at once) with its events pending, and doing the same change again or `reconcile`
  (at start, and covering suspended and pending principals) finishes it and records it once. A repeated revoke no longer
  answers "holds no grant" while its push is outstanding. `ensure_store` on an existing store re-pushes its principals.
- **F9** The audit and notification receivers (path-secret authenticated, `WEFTOS_STORE_INGEST_ADDR`) were already called by
  the daemon; new: the daemon follows RustFS's own log (`WEFTOS_STORE_RUSTFS_LOG`) into `ingest_log_line`, which now reads both
  `s3_authorization_denied` and `SignatureDoesNotMatch` / `InvalidAccessKeyId` (the latter carry no account or bucket in
  RustFS 1.0.4, so they are logged as an unattributed `Authenticate` refusal and cannot suspend anyone). A principal whose own
  key is refused 20 times in 60 s (`WEFTOS_STORE_DENIAL_RULE=off|<max>/<s>`) is suspended through `suspend_principal`.
  Field names: the denial and signature lines are real RustFS 1.0.4 lines; the audit and notification bodies are exercised by
  the live test against a real RustFS (`live_a_request_straight_to_rustfs_reaches_the_activity_log`). Not validated: a
  production (photo-gallery) log, which was not read.
- **F15** `RustfsConfig` `Debug` redacts both root keys; the secrets directory is 0700 (a loose one of ours is tightened, another
  user's is refused); the secret's temp file is `create_new` 0600; plain `http://` is accepted only to a loopback host.
- **F16** SigV4 is checked against four AWS-documented canonical requests and signatures; every bucket string goes through
  `check_bucket`; a CopyObject `200` with an `<Error>` body is an error and the body must be a `CopyObjectResult` that agrees with the
  destination. Integrity: puts send `x-amz-checksum-sha256` (RustFS refuses a wrong one); a read is checked against Content-Length and
  the stored SHA-256, else the single-part ETag (MD5), else reported `Unverified` (multipart ETags prove nothing about content).
- **F17** The bucket is the store's identity. A second name for one bucket is refused; `ensure_store` and the daemon at start move
  a `local` store to its mesh-id name (record, grants, retired mark, pending events, exposure) with one stamped `store.migrate`
  event. Node store tokens must be the clean id (`pg.1` and `pg-1` no longer share `n-pg-1`). This is not the `n-<id>` to `node-<id>`
  bucket rename, which is implemented on `w2/t-names-and-conflicts` (local, pile T) and waits for owner confirmation.
- **F18** `store.promote` refuses an empty, unclean or reserved (`.wt/`, `worktrees/`) prefix, never copies a reserved-area object,
  and refuses a worktree that holds overlay state under `.wt/`, so S2's landing path and its conflict check cannot be bypassed.

**S1 done-when rerun at the tip after F2/F9/F15-F18 (2026-11-08, `1153af0a`).** Against a throwaway
RustFS 1.0.4 container (`docker.io/rustfs/rustfs:1.0.4`, podman, loopback `127.0.0.1` only, tmpfs data, root
key made for the run; not the PG or photo-gallery RustFS). Commands: `cargo test -p weftos-store --features
rustfs --lib -- --ignored --test-threads=1` and `cargo test -p clawft-weave --features weftos-store/rustfs
--lib live_ -- --ignored --test-threads=1`, with `WEFTOS_STORE_TEST_RUSTFS_ENV` naming the throwaway env file.
- `weftos-store`: 16 of 16 live tests pass in 27.05 s (34 s wall including the test-binary build).
  This includes `live_service_done_when` (node and project stores; a disabled grant refused at the service and
  at RustFS), the scoped-then-disabled key (a disabled key's next request is a 403), `put_if_match`, the
  release roles, Object Lock, versions, multipart, and the F16 checksum and CopyObject check.
- `clawft-weave`: `live_rustfs_backs_the_daemon_service` (node store through the daemon service) and
  `live_a_cog_uses_svc_store_on_rustfs_and_a_disabled_grant_is_refused` (a cog's write appears in the
  activity log; disabling its grant refuses the next request at the service and at RustFS) pass, 0.81 s for both
  (72 s wall, mostly compiling).
- Not part of this run: `live_a_request_straight_to_rustfs_reaches_the_activity_log` needs a local `rustfs`
  binary (`WEFTOS_STORE_TEST_RUSTFS_BIN`), not a container, so it was not run here (it passed earlier on
  the binary); `conv_postmortem::live_postmortem_distills_durable_fact` needs Hermes :8090 and is not S1; it
  failed because that endpoint is not available.

**Touch point with the Mac-side S3/drive work.** The additive `put_if_match` / `delete_if_match` default
methods on the backend trait (`wt/adr126-s3-drive`, `2c43a95c5..1cade20f8`) touch S1's `backend.rs`.
`ObjectBackend::put_if_match` already exists here as a *required* method (`backend.rs`, implemented by the
RustFS, local-directory and in-memory backends, and by the two test backends in `backend_tests.rs` and the
`Flaky` backend in `service_tests.rs`); there is no `delete_if_match`. Signature identical (confirmed against
`1cade20f8`, per the owner's check of `backend.rs` in the Mac repo). At merge: one declaration of
`put_if_match` (S1's, required), S1's implementations stand, and the `delete_if_match` default comes in
unchanged. One detail to settle at merge: the Mac defaults return `BackendError::Unsupported` bare, while
S1's `BackendError::Unsupported` carries a message (`Unsupported(String)`), so the `delete_if_match` default
needs `Unsupported("conditional delete".into())` here.
**Resolved at the merge (2026-11-08):** S1's `Unsupported(String)` and required `put_if_match` won; the Mac `delete_if_match`
default returns `Unsupported("conditional delete".into())`; a second touch point not in the notes, the Mac
`BackendError::PreconditionFailed(String)`, was replaced by S1's `Stale(String)` at 16 sites, and etag quoting is
tolerated (`trim_matches('"')` in the fs and memory backends, quoted on the wire for RustFS).

## S2 status (2026-11-07, branch `feat/adr126-s2`; landed on `target-0.8.5` 2026-11-09)

- **Landed (2026-11-09):** the Mac slices `wt/adr126-s2-mac` (path rules, `.wt/` control state refused to principals,
  Deny policy statement), `-s2-meta` and `-s2-hooks` (agent-git-flow hooks, `scripts/tests/worktree-store.test.sh` 63/63)
  went in with S1 (15dcf72c3). aepod's `feat/adr126-s2` d0db74233 was rebased onto that (`land/adr126-s2-on-s1land`
  cae1b0b25; reconcile commit 93a3836b5: Mac wins on overlap, S2's registry signing, follower retry, claim epoch, land
  preview, quota, gc and switch replayed on top, stricter check kept) and merged onto 15dcf72c3 as 7ba34b071, which is on
  `origin/target-0.8.5` (e58899bb0). The union mount stays in the code and out of the done-when, pending the owner
  (below).

- **Verified caller identity, card 6f0bc9c8 (2026-11-16, branch `w2/card-6f0bc9c8`, local; D1-1 and D1-2 close only under one OS uid per principal, not yet shown end to end):**
  until now the principal on `store.worktree.*` and `store grant` was a label the caller supplied (`WEFTOS_AGENT`),
  and the verbs ran on the daemon's operator path, so on one daemon a read-only principal could publish, claim and
  grant itself write. The owner's 2026-11-15 decision stands: one daemon is one trust domain unless this is switched
  on, and read-only followers are enforced by a scoped S3 key per machine.
  - **Design.** The daemon derives the caller from the socket peer's uid, read at accept from the unix socket's peer
    credentials (tokio's `peer_cred`, `SO_PEERCRED` / `LOCAL_PEERCRED`, a safe API; no unsafe code, no new
    dependency), never from the request. The uid is mapped to a store principal by a daemon-side file,
    `store-principals.toml` in the runtime directory: `[uids]` then `1001 = "agent:follower"`. The map is read once
    at store start. The mode is off unless `WEFTOS_STORE_PRINCIPALS` is on in the daemon's environment; a map file
    that exists with the switch off is ignored (a warning is logged). With the switch on, the file must exist, be a
    regular file owned by the daemon's uid and not be writable by group or other; a file that is missing, mis-owned,
    too writable or does not parse stops the store and leaves the deny-everyone-but-the-operator map installed, so a
    missing file never means open mode.
  - **Rules, only with the switch on.** The daemon's own uid is the operator (mapping it is a configuration error).
    A mapped uid is that principal and each verb is checked against its grants: `create` needs write on the project
    store; `claim` and `orphan` need write on the worktree store; `promote` needs write on the project store and on
    the worktree; `browse`, `search`, `leftovers`, `buckets`, `usage` and `overlaps` need read on the project store;
    `publish`, `fetch`, `follow` and `sync-docs` are the operator's, because they open a path the caller names as the
    daemon's uid (a confused deputy: a mapped uid could write or read files it cannot reach itself), and any
    principal call that carries a host-path field (`root`, `dest`, `cache`, `state`, `bundle`, `bundle_out`,
    `paths_z`, `uncommitted_z`, `changes_z`) is refused whatever the verb. Moving that file I/O to the client is a
    follow-up; `store grant` and `store revoke`
    need admin on the named store; `store put` and `store get` must name the verified principal. Everything else
    (`gc`, `store ls|create|promote|archive|...`, `store.drive.*`, `store.view.*`, `store.client.*`) is the
    operator's. An unmapped uid, a supervised child and a peer with no readable uid are refused. A mapped
    principal's call is decided by the grants alone, replacing the Admin capability check, so it needs no admin
    token. The `agent` label is required and must equal the full principal (`agent:name`, kind and id), so the registry,
    chain and claim records name the verified identity; an absent, bare or other-kind label is refused. The client
    sets it with `WEFTOS_AGENT=agent:name`. The `host` half of the claim identity is still caller-supplied (follow-up).
  - **Fail-closed.** No grant, a disabled grant, a suspended principal and the narrow `append` and `sweep` roles
    all fail a write check.
  - **Not closed as drilled (D1-1 and D1-2).** The drill ran `WEFTOS_AGENT=follower` under the same uid as the daemon,
    which is always the operator here, so `wo069-drills.sh --only d1` would print both FINDING lines unchanged. The
    findings close only under the deployment rule of one OS uid per principal, and no end-to-end run with a second
    uid exists yet. Do not claim them closed until it does.
  - **Honest limit.** A uid is all the daemon can verify about a same-machine peer. Two principals that must be told
    apart must run as two uids; two processes of one uid are one principal. A signed request was not added: the
    existing kernel auth has tokens (scope-bound, not principal-bound) but no per-principal signature. Across
    machines the scoped S3 key per machine remains the control.
  - **Code.** `crates/clawft-weave/src/store_caller.rs` (map, `identify`, `check`, `gate`), the one call in
    `authorize_caller` (`daemon.rs`), the install in `store_rt::start`, `StoreService::holds_at_least` and
    `Access::covers` in `weftos-store`. The drill (`wo069-drills.sh --only d1`) needs a second uid and the map file
    to show the two FINDING lines gone; see the card's results.
  - **Owner questions.** How principals map to uids on a real machine (one OS user per agent role?); whether `gc`
    should be open to a principal holding admin on the project store (it is the operator's today); whether
    `store.view.*` and `store.drive.*` need the same gate (the operator's today, so refused to mapped uids).

- **Caller identity follow-up, card 5e5b5d23 (2026-11-16, branch `w2/card-5e5b5d23`, local, off by default like the rest):**
  the owner answered the four questions of card 6f0bc9c8 on 2026-11-16 (18:50 ET).
  - **Q4, unchanged.** The OS-user mapping is enough; a mapped uid needs no admin token.
  - **Q2, `gc`.** A mapped principal holding admin on the project store may run `store.worktree.gc` (dry run or confirmed);
    write, read, no grant, a disabled grant and admin on a worktree store alone are refused. The label and host-path rules still apply.
  - **Q3, `store.view.*` and `store.drive.*`.** They take the same agent-label and host-path checks and are decided by the grants
    on the store named in `name` (required for a principal): `view.open` (write; read when `read_only` is literally `true`),
    `view.status`, `view.refresh`, `drive.materialise`, `drive.sync-down`, `drive.watch` need read; `view.publish`, `view.resolve`,
    `view.renew`, `view.close`, `drive.sync-up`, `drive.resolve` need write; a verb not in that table stays the operator's.
    Every one of these verbs works on a directory the caller names (`root`, or `dest`), which the daemon opens as its own uid, and
    that is the confused deputy of the worktree verbs. So the host-path rule leaves a principal none of them today: they stay
    unusable until the file I/O moves to the client (a test pins that each verb fails without its directory). Nothing more is
    needed to open them then; the grants are already in place.
  - **Review r2 Lows.** N1: `create` of a slug whose worktree already has its meta also needs write on that worktree. N2: a
    verified principal's `store.grant`, `store.revoke`, `store.put` and `store.get` are recorded under that principal (chain
    `actor`, `store.as`), not as `cli`. N3: the map file is opened with `O_NOFOLLOW | O_NONBLOCK`, so a symlink is refused and a
    FIFO cannot block startup (the macOS ACL check was not added: it needs a new dependency or FFI). N4: a principal's params
    that are not a JSON object are refused outright.
  - **Q1 and what it does to D1-1 and D1-2.** The owner says one OS user per agent role is not the intended deployment: several
    roles may share one uid. The daemon can verify only the peer uid of a same-machine caller, so under that model it cannot tell
    two roles of one uid apart, and every such caller is the operator or one mapped principal. **D1-1 and D1-2 stay open** under
    the owner's model; the uid map closes them only for roles that run as different uids.
  - **What could separate roles under one uid (a proposal, nothing built).** Under one uid a hostile role can read another role's
    files and `/proc/<pid>/environ` and can ptrace it, so anything a role holds as a secret (a key, a token, a socket path) is
    readable by the others; only a boundary the OS enforces separates them. Options, cheapest first:
    1. A principal-bound token: `weft token issue --principal agent:x`, which the launcher hands to that role only (an fd or a
       0600 file in the role's own working directory). The daemon maps token to principal. It stops a role that mislabels itself or
       is confused by a prompt; it does not stop a role that reads a sibling's token.
    2. One socket per role, each bind-mounted into that role's sandbox only (bubblewrap or a container, both already on the CI
       image), the socket path naming the principal. File modes alone do not do this under one uid; the mount namespace does.
    3. Per-role signing keys held outside the uid's reach (macOS Keychain with a per-app ACL or the Secure Enclave, a TPM, or a
       separate signer process under another uid), with signed, nonce-bound requests. Strongest and most work.
    **Owner question:** is the threat a confused or prompt-injected role of your own (option 1 is enough), or a role that is
    hostile to its siblings (option 2 or 3, which also means each role runs sandboxed)?

- **WO-069 lane D3 (2026-11-15, branch `w2/wo069-d3-content-addressed`, card 6b58589c):** manifest
  schema 2 and content-addressed objects with a grace sweep (section 4), the conditional manifest write
  and post-check (closes B1), `changes_recorded` (finding 6f), the daemon switches
  `WEFTOS_WORKTREE_MANIFEST_SCHEMA` and `WEFTOS_WORKTREE_SWEEP`. Not pushed.

- **WO-069 additions (2026-11-15):** lanes A (sweep trigger), B (browse), E (orphan marking and gc), F
  (manifest cache), the file splits (`follower*`, `share_*`, `store_worktree_*`), the drill harness and
  the drill fixes (`gc --confirmed` removes a landed worktree's registry entry, `leftovers` reports
  orphans, `fetch` of a tree with no bundle succeeds with a note, `worktree-store.sh follow --full`)
  and one tree listing per publish are together on the local branch `w2/wo069-worktree-mirror`
  (df4f723d1, from 40ff68976). It is not on `target-0.8.5` and not pushed. Verified on that
  commit: `weftos-store` 371 tests, `clawft-weave store_` 168, clippy clean on both, shell suites 64
  and 81, live drills 59 passed and 0 failed against a throwaway loopback RustFS. The §4a gate is
  not passed.
- **Where the code is:** `crates/weftos-store/src/{overlay,promote,share,share_fetch,follower,testing,worktree_backend}.rs`
  and `tests/s2_{worktree,follower,done_when}.rs`. The Rust uses only `std`, `serde`, `serde_json` and
  `sha2` and spawns no processes. `worktree_backend.rs` implements `overlay::ObjectBackend` for every
  S1 `ObjectBackend` (local directory, RustFS, in-memory), so the S2 code runs unchanged on any of
  them; S1's backends report no modification time, which S2 no longer needs. Git work and the
  desktop mount are in `scripts/worktree-store.sh` (tests: `scripts/tests/worktree-store.test.sh`,
  `scripts/tests/worktree-union-mount.test.sh`). `sha2` is now a non-optional dependency of the crate.
- **Merged with S1** (`feat/adr126-s1`, 2026-11-07): the modules are wired into `lib.rs`.
- **The `weaver store worktree` verbs exist** (`crates/clawft-weave/src/store_worktree.rs`, CLI in
  `commands/store_cmd.rs`, RPC `store.worktree.*`, Admin like every `store.*` verb): `create`,
  `buckets`, `publish` (`--paths-z`, `--uncommitted-z`, `--changes-z`, `--bundle`, `--handoff`),
  `fetch`, `claim`, `promote`, `follow`, `search`, `leftovers`, `sync-docs`, `overlaps`. They run on
  the daemon's raw data plane (`StoreService::operator_objects`, added for this, as is the now-public
  `ensure_archive`), the same level as S1's own promote and archive, so they skip per-principal
  grants. `create`, `publish`, `claim` and `promote` are recorded in the activity log and on the
  chain, failures included. The daemon refreshes the project registry entry after publish and
  claim, which is the only writer of `worktrees/<slug>.json`. Paths in params (`--root`, `--dest`,
  `--cache`, the lists) are read and written by the daemon itself, so the CLI and daemon must share
  a host and a user. Promotion archives into `weftos-archive` under `<bucket>@<ms>/`. Two S1 files
  got small additive edits for this (`service.rs`, `service/lifecycle.rs`).
- **Verified (in `weftos-store` and `clawft-weave`, clippy `-D warnings` clean; daemon test `store_worktree::tests` drives every verb from create through claim and land on an `FsBackend`):** 56 S2
  integration tests plus `tests/s2_done_when.rs`, which runs the three done-when checks on S1's
  `FsBackend` with two machines sharing only the store: publish, hand off, follow, search, fetch,
  claim and merge across machines (a claimed merge cannot be overwritten by the writer); a new
  worktree reads project assets with nothing copied into its bucket; landing promotes once and
  archives the unmerged docs. Shell: 37 assertions including a real `git bundle` round trip, and 8
  more in `worktree-union-mount.test.sh`, which does a real `rclone mount` (FUSE, rclone 1.75.2)
  over the local-directory store layout: project files read through with no copy, new and edited
  files land only in the worktree bucket, `.wt/` is hidden, deleting a project-only file is
  refused, the project bucket is untouched. **Not verified:** a real RustFS (the same scenario is
  `two_machines_share_a_worktree_on_a_real_rustfs`, ignored until `WEFTOS_STORE_TEST_RUSTFS_ENV`
  names a throwaway server), the mount over an S3 remote, macOS or Windows, two physical machines,
  or `agent-git-flow` calling the script (it has no hook seam; the repo wraps `wt.sh` and `land.sh`).
- **Known limits:** claims are advisory; claim expiry compares the claimer's stamp with the
  evaluating daemon's clock (§4); `rclone union` cannot
  express tombstones, so deleting a project-only file through the mount is refused; rclone does not
  flush its write cache on unmount, so `worktree-store.sh umount` waits for the write-back delay;
  doc search matches path and title only, not content.

- **After the adversarial review (2026-11-08):** the findings fixed on `wt/adr126-s2-mac` are
  described in §4 and §4a (path rules, control state, slug and bucket checks, ceilings, claim
  host and skew, audit, digest). Evidence and per-finding tests are in
  `docs/research/adr126-s2-evidence-macos-2026-11-08.md`.
- **Still open:**
  - ~~F2, grant and revoke locking, and F9, RustFS audit ingestion.~~ Fixed on `feat/adr126-s1` (b93ca7079, 1153af0a9;
    "Close-out findings" above) and on `target-0.8.5` since 2026-11-09.
  - F7, F13, F14, F19 and F20 are fixed in the S2 layer (a land closes the handoff as merged
    whatever the claim state; a wider secret denylist; publish reads one open handle checked
    against the path; fetched manifests cannot name the follower's control files; `FsBackend`
    refuses keys that differ only by case). ~~Still open in the S1 layer, for the S1 session: F15
    (`RustfsConfig` derives `Debug` with the root key; secrets dir mode), F16 (no SigV4 known-vector
    test; bucket strings unvalidated), F17 (the bucket name omits the mesh id, so `local` and
    `<hex>` names alias one bucket), F18 (S1 `store.promote` accepts `""`, `.wt/` and `worktrees/`
    prefixes).~~ F15 a035b18ee, F16 4f439fcef, F17 4a63ad565, F18 4826568ac, all fixed on `feat/adr126-s1`
    on 2026-11-08 and on `target-0.8.5` since 2026-11-09 (the F18 test plants `.wt/` state at the backend because the
    S2 guard now refuses it through the service). See `docs/research/adr126-s1-s2-adversarial-review-2026-11-08.md`.
  - `slug_of` in `worktree-store.sh` is now injective (`/` becomes `--`; branches containing `--`, `-/` or `/-` are refused and `WTS_SLUG` overrides). Existing stores created under the old slashed-branch slugs (`feat/s2` was `feat-s2`, now `feat--s2`) are not found by the script until renamed or re-created; none are known to exist outside drills.
  - The owner rejected the rclone mount. The union mount (`mount-config`, `mount`, `umount`) is
    proposed to be parked: kept in the code, dropped from the S2 done-when and from any gate, and
    marked parked in the script help and §4 (proposal in §1 of
    `docs/plans/mesh-store-next-steps-2026-11-08.md`, awaiting the owner). The desktop access
    path is the materialise-and-watch route in ADR-131.
## S3 results (2026-11-07)

S3 is built in `crates/weftos-edge-creds`, separate from S1's `weftos-store`. It does no I/O and
reads no clock, so the host supplies the parent link, the HTTP client and the time. It depends on
S1 only through two traits, `StsMinter` (RustFS AssumeRole and user disable/enable, card b76c74a7)
and `GrantCheck` (the chain-recorded grant).

- **Parent side (`EdgeBroker`).** For a node the link has authenticated, it checks the revoked set,
  a per-node rate limit (30 per minute) and the grant. It derives the bucket `n-<node id>` (`node-<node id>` once the names branch lands) from the
  node id, never from the request. It then asks RustFS for an STS session as the node's own RustFS
  user, with an inline policy naming only that bucket. The user's long-lived key stays on the
  parent. For a presigned URL it mints a session scoped to one object and one action, and signs
  with that, so the edge can hold no credentials at all.
- **Edge side (`EdgeNodeStore`).** It is built with no key. It signs with SigV4 and the session
  token, refreshes inside the last 60 s, and retries once with a fresh lease on a 403. It refuses
  any credential with no session token or a lifetime above 15 minutes plus 60 s of skew, so a
  parent that leaked a long-lived key is rejected.
- **Revocation.** `revoke` records the node, then disables its RustFS user. A live session is
  refused at its next request (S0: about 0.22 s), and the parent refuses a new lease.

Verified against an in-memory store that checks SigV4, the session token, expiry, the user's
enabled flag and the session policy (`cargo test -p weftos-edge-creds`, 21 tests):

- the SigV4 header and presign code reproduces AWS's two published S3 example signatures;
- a Wasm-style node writes, reads, conditionally creates and deletes in its own bucket, and the
  store only ever saw STS access keys;
- a session aimed at another node's bucket is refused, and a presigned URL is bound to its key,
  bucket and host;
- expiry is refreshed transparently; a node revoked mid-session is refused on its next request;
- planted defects (revoke without disabling the user, a wildcard session policy, a removed TTL
  cap, skipped key validation) each fail a test.

**Live minter and grants (card b76c74a7).** `RustfsMinter` implements `StsMinter`:

- **Mint:** a signed (SigV4, service `sts`) `AssumeRole` form POST using the node user's own
  long-lived key, with the inline session policy and `DurationSeconds` fixed at 900. STS
  intersects the session policy with the user's attached policy, so a session never exceeds its
  user. The 2048-byte policy limit is checked before anything is sent.
- **Disable and enable:** an admin-signed `PUT <prefix>/set-user-status`. The prefix defaults to
  `/rustfs/admin/v3` and is configurable.
- **Pairing and unpairing:** `provision_node` creates the node's policy, user, policy attachment and
  bucket; `deprovision_node` removes the user and policy.
- **Keys:** the per-node user keys sit behind `NodeKeys` on the parent, are redacted in `Debug`
  and zeroed on drop, and never appear in anything sent to the edge.

`GrantLedger` implements `GrantCheck` from chain `GrantEvent`s (grant, revoke, revoke-all, with a
sequence number so a replayed or out-of-order event is ignored). A node is allowed only on its own
`weftos://<mesh>/nodes/<id>/store`, and write implies read. `apply_and_enforce` applies an event
and, when the node lost write, revokes it at once, rather than waiting for its session to expire.
The native HTTP client is the host's: a `reqwest` client here would trip the diff gate's `net`
rule, which only the owner can approve.

Tested end to end (37 tests) against a store that verifies signatures, session tokens, expiry and
the user-policy intersection, and serves STS and admin calls with the 900 s minimum and the
2048-byte limit. Seven planted defects in the new code each fail a test. Acceptance for the card:
a node gets a session for its own bucket only, another node is refused, and revocation refuses
the next request.

**Live run (2026-11-07, RustFS 1.0.4 in a loopback container, not PG).** The real broker, minter and
edge store ran against it from a throwaway harness kept outside the repo (its raw-socket client would
trip the diff gate's `net` rule). Results:

| Check | Result |
|---|---|
| Pairing: policy, user, attach, bucket | all four calls work with plain JSON bodies on `/rustfs/admin/v3`. The `/minio/admin/v3` prefix wants an encrypted body and refuses plain JSON |
| STS lease, edge write, read, delete | work; the session lasts 900 s |
| Conditional create | `If-None-Match: *` gives 412 on the second write |
| Presigned PUT | works, **only with `Host` as the sole header**. RustFS refuses a presigned request that carries an `x-amz-*` header outside the signed set (`presigned_unsigned_amz_header`). The first live run caught this; the edge is fixed and the mock now enforces it |
| Cross-node | node A's session writing to node B's bucket: 403. A's user asking for a session policy that names B's bucket, then writing: 403 |
| Revocation | `set-user-status` disabled: the first request after the call, on a session still 14 min from expiry, got 403, about 7 ms after the revoke started; the edge's next write was refused by the parent. Restore works |
| Removal | after `remove-user`, AssumeRole as that user: 403 |
| Session floor | RustFS accepted `DurationSeconds=60`. The 900 s minimum is MinIO's, so the floor is a config field defaulting to 900 |

Also checked: the crate compiles for `wasm32-unknown-unknown`.

**Not yet verified:** the spike's gate C6 on PG itself (this run used a local container); real edge
hardware; the link between a node and its parent (cogbus, mesh-local or a WebSocket), which is the
host's. Since built: key generation (`keygen`). Prepared, awaiting the owner: the native HTTP client (see below). Still not built: moving archived
buckets (S1's archive-on-unpair). `node_bucket` is the one-line ADR rule and should be replaced by S1's mapping when S1
lands.

### Adversarial review fixes (2026-11-08)

A read-only review of the crate found four defects, all fixed with a test that fails when the fix is removed:

- The 900 s cap was enforced only on the edge. The broker now refuses any session whose expiry is above `now + 900 + 60`, whatever the store returned.
- Revoke could fail open: disabling the RustFS user looked up the node key first, so a missing key left existing sessions alive. The disable now names the principal directly and signs with the admin key.
- `Debug` on `HttpRequest`, `HttpResponse` and `Presigned` printed session tokens, the node secret on the add-user path and presigned URLs. They now print shape only.
- Presigned keys containing `*`, `?` or `$` are refused, since they would widen the one-object session policy.
- Refusals sent to the edge carried store error text. The edge now sees "credential service unavailable"; the detail goes to the audit sink.

Node secrets come from `keygen` (160 bits of host entropy, constant input refused; `generate_node_key` behind the `keygen` feature).

Open from the review, not fixed: a grant event with `seq = u64::MAX` freezes the ledger, so only chain-authenticated events may reach `apply`; reusing a node id after unpair inherits the old bucket unless S1 archives it.

### Handoffs to other lanes (2026-11-08)

The two review items this crate cannot fix are now ignored regression tests in `crates/weftos-edge-creds/src/tests_live.rs`. Run `cargo test -p weftos-edge-creds --lib -- --ignored` to see both fail today. Remove the `#[ignore]` when the owner lands the fix.

| Gap | Test | Owner | What the fix needs |
|---|---|---|---|
| A grant event with `seq = u64::MAX` makes every later event `Stale`, so a later revoke is dropped | `a_far_future_seq_cannot_freeze_later_revocations` | S1 (weftos-store) and whoever feeds chain events to `GrantLedger::apply` | Only chain-authenticated events reach `apply`, or `apply` bounds a jump in `seq` |
| Reusing a node id after unpair inherits the old node bucket (`n-<id>` today; `node-<id>` after the rename, `n-<id>` for stores made before it) | `a_reused_node_id_does_not_inherit_the_previous_nodes_objects` | S1 (archive on unpair, section 3) | Archive and empty the bucket before the id can be paired again, or never reuse an id |

Ed25519 verification audit: the S3 crate and ledger verify no signatures (grant events arrive already chain-authenticated), and there is no `weftos-store` crate in this worktree yet. S1 and S5 must use `verify_strict` for any signature they check. In the mesh paths here, `node_facts_advert`, `placement_vocabulary`, `capability_claim`, `key_rotation` and `seed_bind` moved from `verify` to `verify_strict`; `clawft-mesh-local` cert checks already used it. Small-order key rejection tests: `cert_tests::small_order_machine_key_is_rejected` and `node_facts_advert::tests::small_order_key_with_a_trivial_signature_is_refused`. Still on plain `verify`, outside the mesh scope, for their owners: `http_facade`, `wasm_runner/types`, `workload_ctl/msg`, `workload_ctl/host_journal`, `clawft-delegation`, `exo-resource-tree` delegation, `tree_manager`, `weftos-cos` `mcp_catalog`, `clawft-weave` `project_forward`.

## Chain compaction (proposal, 2026-11-08)

**Status: proposed. A spike exists for the grant ledger (`crates/weftos-edge-creds/src/compaction.rs`), exercised against an in-memory loopback chain; nothing is wired to a live chain and no schedule is on.**

**Problem.** The grant ledger (S3) and the S5 gate state both rebuild by replaying every event they ever wrote. Start-up time and the cost of a new follower grow with the length of the chain, and revoked history is carried for ever.

**Design: snapshot plus tail.**

1. *Snapshot.* The daemon periodically folds the table into a `Snapshot`: `version`, `scope` (for example `grants/<mesh>`, or `gate/<account>/<mesh>` for S5), `through_seq`, the folded `state` in canonical order, `state_hash` (SHA-256 of the canonical state) and `events_head`.
2. *`events_head`.* A hash chain over the events folded so far: `head' = SHA256(head || "\n" || canonical_event_json)`, starting from a fixed genesis value. Events the sequence rule ignores do not extend it.
3. *Publication.* The snapshot is appended to the chain as one signed event (`store.snapshot`) by the same signer that signs grant events, using the domain `weftos/grant-ledger/snapshot/v1`. The signature covers the canonical snapshot JSON, so `through_seq` and `scope` cannot be moved.
4. *Restore.* A follower takes the newest snapshot it trusts and replays only the events with `seq > through_seq`. A tail event at or before `through_seq` is an error: it means two histories were mixed.
5. *Archive before prune.* The segment up to `through_seq` is written to cold storage and its range hash recorded before any chain segment is pruned. The snapshot alone cannot answer who held what, when. Pruning is a separate, owner-run step, never part of taking a snapshot.

**How a follower verifies a snapshot.**

- *Fresh follower (no old events).* Checks `version`, `scope`, `state_hash` against the state, then the signature with `verify_strict` under a key it already trusts for chain events. This trusts the signer for the past. A compromised signer could sign a snapshot that omits a revoke.
- *Follower that kept the events.* Also calls `verify_against_history`: re-fold the old events and compare `through_seq`, `events_head` and the state. This catches a validly signed but false snapshot, and a state that matches by a different history (the head differs).
- *Cross-check.* Two followers that disagree on `events_head` for the same `(scope, through_seq)` raise an alert; the archived segment settles it.

**What a snapshot must not do.** It carries no tokens or keys. A revoke is carried by the absence of the grant, and the existing sequence rule drops stale pre-snapshot events. The first snapshot after a signer rotation is signed by the new signer and must also chain to the old one by the chain's own key-rotation event, which the chain layer verifies, not this crate.

**S5 gate state.** The gate's `store.client.gate.<kind>` events (hold, grant, revoke, trust, cross-mesh install or revoke) fold the same way. S5 implements the fold and canonical bytes for its own state and reuses `head_extend`, the hashing rules and the verification steps above. Two points are S5's to settle: expiry (a cross-mesh grant that expired must not survive into a snapshot, so the snapshot is taken at a stated time and drops expired entries), and the per-account scope, since events for other accounts are ignored on replay.

**Open.**

- Cadence: every N events or every T hours, and whether a snapshot may be taken without a quiet point (the spike folds under the ledger lock).
- Where the signed snapshot lives on the chain and how chain-level pruning refers to it; this belongs to the chain owner.
- Whether `events_head` should be anchored in the existing chain anchor so a fresh follower has a second source besides the signer.

**Spike results.** The ledger now keeps `events_head`; `GrantLedger::snapshot`, `signed_snapshot`, `restore` and `verify_against_history` exist. Seven tests: snapshot plus tail equals full replay; a pre-snapshot event in the tail is refused; a follower refuses an edited, re-hashed, wrong-scope, rewound or foreign-signed snapshot; a follower with history catches a signed but false snapshot; the head catches a different history with the same state; the head depends on order and content; 1000 pair and unpair events fold to an empty snapshot. Each check was removed in turn and a test failed.

**Loopback wiring and schedule (2026-11-08).** `loopback::LoopbackChain` (feature `loopback-chain`, or tests) is an in-memory, append-only stand-in for the chain: grant events and signed snapshots, no I/O, no clock, no live chain. It adds `append`, `snapshot_now`, `recover` (newest snapshot that passes the follower checks, then an older one, then a full replay while nothing is pruned), `archive` (copies the events to cold storage and requires them to reproduce the signed snapshot) and `prune` (only after `archive`, only up to the newest usable snapshot). `SnapshotSchedule` is the scheduled-snapshot option: **off by default**, `every_events(n)` and/or `every_secs(t)`, never due when nothing is new; the chain takes a snapshot on `append` only if the host built it with an enabled schedule. Nothing in the crate turns it on. Twelve tests; a forged newest snapshot falls back, a forged one after a prune is an error rather than a silent partial ledger, prune is refused without a matching archive, and the schedule defaults off. Five planted defects (schedule always due, prune without the archive check, unchecked newest snapshot, ignoring the pruned bound, archive without the history check) each fail a test. `tick(now)` lets a host timer take a snapshot when only the time rule is due, and `compact(verifier, now)` is the owner-run snapshot, archive, prune sequence (the schedule never calls it; each stage refuses to go on if the one before did not hold). A repeat `snapshot_now` with nothing new returns the existing snapshot. A property test runs seven cadences against every compaction point and checks recovery equals a full replay. Still the chain owner's: the real signer, where the snapshot event sits, and the cadence to use (see the switches below).

**Bucket name parity with S1 (2026-11-08).** S1's `StoreName::Node` (weftos-store `name.rs`) derives `n-<id>` with an id of lowercase `[a-z0-9-]`, at most 61 characters (`node-<id>` with a 58-character id cap on `w2/t-names-and-conflicts`, not landed). This crate's `node_bucket` accepts a strict subset (at most 48, no leading or trailing dash), so every id it accepts maps to the bucket S1 derives; `node_bucket_agrees_with_s1s_store_name_rule_and_is_never_looser` pins that. When S1's crate lands as a dependency, replace `node_bucket` with `StoreName::bucket` and drop the extra restriction only if the owner wants ids S1 allows.

**Native HTTP client, prepared and unmerged (2026-11-08).** Branch `prep/adr126-s3-reqwest-client` (commit c9b85e56, on top of this branch) adds `reqwest_transport::ReqwestTransport`, an `ObjectTransport` over `reqwest` behind the off-by-default feature `reqwest-client` (host side only, not wasm-clean). It sends the already-signed request with the signer's own percent-encoding, never follows redirects (a signed request must not be replayed elsewhere), and returns generic errors with no URL (a presigned query would leak). Four tests against a loopback listener; three planted defects (redirects followed, error text with the URL, URL encoding) fail a test. The diff gate rejects it, as expected: `new-dep` for `reqwest` in `crates/weftos-edge-creds/Cargo.toml` (already a workspace dependency, optional here) and `net` for `crates/weftos-edge-creds/src/reqwest_transport.rs` (the client and the test listeners). **The owner's choice is one step:** add the two approvals for those paths in `config/diff-gate-approvals.toml` and merge the branch; or decline, and the host keeps supplying its own `ObjectTransport`. Nothing on this branch is needed by the rest of the crate.

**Consume side of S1's archive-on-unpair and seq guard (2026-11-08).** S1 had not posted either call when this was written (its branch has no unpair archive and no seq guard), so S3 holds the contract as traits and tests it against mocks. When S1 posts commit ids, implement the traits over the real calls; nothing else changes.

- **`lifecycle::NodeLifecycle`** with `NodeArchiver` (S1: `archive_node` copies the bucket to the archive and empties it, idempotently; `bucket_is_empty`) and `NodeProvisioner` (`RustfsMinter` implements it). `unpair` applies the event and cuts the node off, seals the id, archives, and only then deletes the user and policy. A failed archive leaves the user disabled, not deleted, and the id sealed; calling `unpair` again finishes. `check_pairable` refuses a sealed id until its bucket is empty, and fails closed when the probe errors. The sealed set is memory only: after a restart `seal_from_history` rebuilds it from the chain's unpair events, and the empty-bucket probe is what unseals.
- **`grants::SeqGuard`** (`GrantLedger::set_seq_guard`; `MaxSeqJump(n)` as a stand-in). A refused event returns `Applied::OutOfRange` and changes nothing, head included. Default is no guard, because the chain's `seq` is shared with other events and a legitimate gap size is the chain owner's call. A ledger rebuilt by `restore` has no guard until the host installs it again. The two `#[ignore]` regression tests still describe the unguarded default; the guard is covered by separate tests.
- Eight tests; five planted defects (delete before archive, no sealing, probe fails open, guard ignored, state changed before the guard refuses) each fail a test.

**What gate C6 needs.** The repository does not define C6; the card (b76c74a7) states it as: a WeftOS principal gets a scoped STS session for one folder, another principal is refused, and revocation refuses the next request within 1 s, all with WeftOS-minted credentials, on the PG RustFS from S0. Everything but the PG instance is built and was run against a loopback RustFS 1.0.4. What is needed from the owner: (1) permission to run a harness against the PG RustFS (barred to agents today); (2) its endpoint and an admin key supplied at run time, not committed; (3) an HTTP client for the minter, either the owner-approved `reqwest` branch or the throwaway harness kept outside the repo; (4) two throwaway node ids and a bucket prefix agreed so the run cannot touch real stores; (5) the run window and who removes the test users afterwards. Expected result is the loopback table above, with the revoke time measured on PG.

**Chain-authenticated feed (2026-11-08).** S5 found that `GrantLedger::apply` trusts whatever it is handed. `authchain::ChainFeed` is now the way events reach the ledger: `ingest(&ChainGrantEvent)` accepts an event only if (1) its scope (a mesh, `grants/<mesh>`) is the feed's, (2) the signature verifies through the same `SnapshotVerifier` interface the snapshots use, under its own signing domain `weftos/grant-ledger/event/v1`, with the signature covering scope, event (so `seq` and `op`) and `prev_head`, (3) its `seq` is after the last applied one (otherwise `Replay`, harmless and safe to drop), and (4) `prev_head` equals the ledger's current `events_head` (otherwise `BrokenLink`: reordered, or an earlier event is missing, and nothing after the gap applies). The link reuses the head kept for compaction, so it survives a snapshot restore. Ingests are serialised, a refused event changes nothing, and `ingest_and_enforce` revokes at once when a node lost write, returning the failure if the RustFS disable fails. Ten tests; seven planted defects (no signature check, no link check, no scope check, signature without scope, signature without `prev_head`, no replay check, guard refusal returned as success) each fail a test; the `prev_head` case needed a new test (a relay rewriting `prev_head` to skip a revoke).

What this settles and what it does not: a forged, edited, relabelled, replayed, reordered or gap-spliced event is refused, and a relay cannot add a far-future `seq` (it is signed). It does not stop a trusted signer from signing `seq = u64::MAX`; the seq guard does that (`OutOfRange`), and a feed that refuses such an event cannot follow a chain that moved on, which is deliberate: an operator has to look. Still the chain owner's: the real signer and its key trust, how the chain wraps a grant event (this crate defines `ChainGrantEvent` as the shape it needs), who is allowed to sign grants for which meshes, and the snapshot cadence and placement.

### Chain-owner switches for S3 (2026-11-08)

Three decisions are the chain owner's. Each is written as one switch with its options and S3's recommendation. S3 has decided none of them and no code depends on a choice: the crate takes the answer through the existing interfaces (`SnapshotVerifier`, `ChainGrantEvent`, `SnapshotSchedule`, `LoopbackChain`).

**Switch 1. Who may sign grant events for which mesh. ANSWERED (owner, 2026-11-08 11:21): grants may be signed by any other node or person.** Built as `authchain::SignerRule::AnyOther` (the default): any identity the host's verifier trusts may sign, except that nobody may sign a `Grant` of access to themselves (`FeedError::SelfGrant`; their own revoke is allowed, it gives them nothing); `SignerRule::Anyone` switches that check off. Which identities the verifier trusts is still the host's. Cost to know about: with open signing, trust rests on the verifier's set of identities and on the self-grant check, which two colluding or sybil identities can pass, so the audit trail and the seq guard carry more weight; per-mesh key limits (options A to C below) remain available if the owner wants them back. The original options follow.

 Today the verifier only answers "did this signer sign these bytes". The feed also checks the scope, so a signer is currently trusted for every mesh the host's `SnapshotVerifier` accepts it for.

| Option | What it means | Cost |
|---|---|---|
| A. One signer key per mesh | The verifier maps `scope` (`grants/<mesh>`) to exactly one trusted key. | Smallest blast radius. Key rotation is per mesh. |
| B. The account's owner key signs all its meshes | One trusted key per account; scope must start with that account's meshes. | One key to protect. A leak affects every mesh of the account. |
| C. Delegated signers | The owner key signs a short-lived delegation to a signer for named meshes; the verifier checks the delegation. | Most flexible. Needs a delegation event, expiry and revoke on the chain. |

Recommendation: A to start. It needs no new chain event, and the `scope` field is already signed. Move to C when a mesh needs more than one signer.
Flip: the owner names A, B or C; the change is in the host's `SnapshotVerifier` implementation, not in this crate.

**Switch 2. Snapshot cadence. STILL OPEN (owner, 11:21): no cadence is approved; the recommendation below is not a decision and nothing sets one.** `SnapshotSchedule` is off by default and supports `every_events(n)`, `every_secs(t)` or both.

| Option | Setting | Effect |
|---|---|---|
| A. Stay off | No schedule; a person runs `snapshot_now` or `compact`. | No new load. A follower replays the whole chain. |
| B. By count | `every_events(n)` | Tail length stays under `n`. The cost follows write rate. |
| C. By count and time | `every_events(n).or_every_secs(t)` | Bounded tail and bounded staleness on a quiet mesh. |

Recommendation: C, with `n` near the number of events a follower can replay in about a second and `t` of one day. S3 sets no numbers; they are the owner's. Pruning is never scheduled: `compact` (snapshot, archive, prune) is owner-run in every option.
Flip: build the chain with `with_schedule(...)`; nothing else changes.

**Switch 3. Where the signed snapshot lives. STILL OPEN (owner, 11:21): not approved.**

| Option | Where | Cost |
|---|---|---|
| A. On the chain as an event | `store.snapshot`, signed by the grant signer. | One source of truth. Chain-level pruning needs a rule that refers to it. |
| B. In the mesh store, with a hash on the chain | The snapshot is an object; the chain holds its hash and `through_seq`. | Keeps the chain small. Followers need store access to bootstrap. |
| C. A and anchored `events_head` | As A, plus `events_head` in the existing chain anchor. | A fresh follower has a second source besides the signer. Needs the anchor to carry one more field. |

Recommendation: A now, and C when the anchor can carry the field. B has a bootstrap loop (a follower needs a snapshot to learn which store to read), so S3 advises against it for grants.
Flip: the owner names A, B or C; the wiring is `GrantLedger::signed_snapshot` on the chain side.

## S5 status (2026-11-08; landed on `target-0.8.5` 2026-11-09)

**Landed.** `feat/adr126-s5` was rebased onto the S2 land as `feat/adr126-s5-on-s2land` 72be2ecc4 and carried onto
15dcf72c3 as `feat/adr126-s5-on-s2-15dcf72c3` 64c547f4a (S5 re-applies an S1 commit, so it had to be rebased rather
than merged); `feat/adr126-s3` fc9c081f7 and `feat/adr126-s4` were merged into it on the way (96c78e454, 3fca8c95c), so
the S3 code landed with S5. Its `weftos-release` dev-dependencies (`weftos-store` with `rustfs`, `weftos-edge-creds`)
carry owner-approved diff-gate entries (3ddb1537e, 72be2ecc4, reworded at e58899bb0 for S4). The record below is
as written on 2026-11-08; "not done" items that name the RustFS backend are closed by the "On RustFS" bullet.

Built on `feat/adr126-s5`, tested on local directories only; nothing is installed on any machine and RustFS is not yet behind it.

- **Per account:** `ClientStoreSpec` pins one instance to `/data1/<account>/store`, the client uid (2102 to 59999, no system names), a loopback or tailnet listener, and a systemd unit that runs as the client user and sees only its own tree. A daemon opens a `ClientStore` only when `<runtime dir>/client-store.json` names the account and the daemon's own euid is that account's.
- **Two layers:** `StoreGate` decides first (held project, local grant, WeftOS-account grant, cross-mesh grant); S1's `StoreService` then applies its own grants, exposure and audit. Every refusal reads `refused`; the cause goes to the audit line.
- **Gate state is the chain's:** each change (hold, grant, revoke, trust, cross-mesh install or revoke) is appended as `store.client.gate.<kind>` with the account and mesh it was written for; opening replays them in order through the same checks as a live change. Events for another account or mesh, or from another source, are ignored. An event for this account that does not parse stops the store opening. A cross-mesh grant that expired while the daemon was down is not restored. A daemon with no chain keeps the gate in `gate.json` (0600, atomic) under the client root.
- **Operator surface:** `store.client.*` verbs (Admin, by the `store.` prefix) and `weaver store client <verb>`. `xmesh-sign` signs a cross-mesh grant locally from a 0600 key file and writes the JSON that `xmesh-install` reads.
- **Compaction (S3's design, applied):** the daemon folds the gate into a `GateSnapshot` (scope `gate/<account>/<mesh>`, `through_seq`, `event_count`, `events_head`, canonical state, `state_hash`, `taken_at`), signs it with the chain key (Ed25519, domain `weftos/gate-state/snapshot/v1`, `verify_strict`) and appends it as a `store.snapshot` event. Opening restores from the verified snapshot with the greatest `through_seq` and replays the events after it. Where the chain still holds every event up to `through_seq` the history is re-folded and `events_head` and the state must match, so the signer is not trusted for the past; a snapshot that fails hash, scope or history refuses the open; one signed by a key we no longer hold is passed over. If any snapshot covers events that are no longer on the chain beyond the one we can verify, the open is refused rather than rebuilt smaller. Differences from the grant-ledger spike that S3's section should absorb: `through_seq` is the chain's sequence (gate events carry none of their own); `event_count` is in the snapshot so a follower can tell whether it holds the whole history; `events_head` covers every well-formed event for the scope, applied or not, because whether an event applies depends on the clock; a cross-mesh install event records the second it was checked at (`at`) so replay does not depend on the clock; expired cross-mesh grants are left out of the state and revoked ids and replay counters are kept. A change and its record happen under one hold of the gate lock, so a snapshot never sees one without the other.
- **Archive before prune:** `store.client.archive` writes the segment the newest verified snapshot covers to `<root>/archive/gate-<through_seq>.jsonl` (0600 in a 0700 directory): a header with scope, `through_seq`, `event_count`, `events_head` and `range_sha256`, then the events in canonical form. It refuses to write a partial segment (the chain must reproduce the snapshot's count and head) or to overwrite. `store.client.archive_verify` checks a file on its own (order, canonical lines, head, range hash) and against a verified snapshot's range, head and count. Pruning stays the chain owner's step, after that check.
- **Key rotation:** a snapshot signed by a chain key that was later rotated out (`identity.key_rotation`, dual-signed, read from the chain) verifies only if its record is before that rotation, so a stolen retired key cannot forge later history. The daemon's `ChainKey` reads the current key and rotation history at each use.
- **Signatures:** cross-mesh grants and snapshots are checked with `verify_strict` (a small-order trusted key does not accept a trivial signature); tests build such a key and assert plain `verify` would have accepted it.
- **Not done:** the RustFS backend behind `ClientStore` (S1's transport), a live install as a client user on PG, the owner's diff-gate approval for the `weftos-store` dependency, the chain-side prune itself, and two `ClientStore`s on one log and account (not supported; each counts only its own events).
- **On RustFS (2026-11-08):** `ClientStore::open_rustfs` runs the service on S1's `RustfsBackend` against this account's own instance. The root credential comes from the client's `store.env` (`MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`; `RUSTFS_ACCESS_KEY` and `RUSTFS_SECRET_KEY` for the unit can live in the same file). It must be a regular file owned by the client uid with no group or other access, and its endpoint must be exactly `http://<spec.listen>`, so a client store cannot be pointed at a RustFS it shares with another account. Per-principal keys are kept under `<root>/secrets/` (0700). `client-store.json` takes `"backend": "rustfs"` and an optional `"env"` (default `/data1/<account>/store.env`); the default stays `fs`. In the gate, S1's `append` and `sweep` roles (release stores) count as write: a client project store has read, write and admin. Live tests (`client_store::tests::live`, `#[ignore]`): `WEFTOS_STORE_TEST_RUSTFS_BIN=<rustfs> scripts/store/client-live-test.sh` starts two throwaway RustFS instances on loopback (random ports and credentials, data under `target/`), runs them with `scripts/build.sh test clawft-weave --filter client_store::tests::live --ignored`, and stops and removes them afterwards (the Rust code starts no process and binds no socket, so the diff gate has nothing of S5's to approve). Run 2026-11-08 against RustFS 1.0.4, 4 passed: two accounts on two instances share nothing (the bucket does not exist on the other instance, acme's cog key and root credential are refused by rival's RustFS, the bytes are under acme's data directory only); the WeftOS account is refused until granted, can read but not write after a read grant, and after revoke the same member key is refused by RustFS itself (403) with no gate or service in the way; a cross-mesh grant becomes a real RustFS policy for the receiving mesh's member, and revoke and the expiry sweep remove it there; `open_rustfs` refuses another instance's env file, group-readable or symlinked env files, and a file for another uid.
- **One instance per client account, enforced:** every store opens by claiming its instance (`client_store_instance::claim_instance`): the marker bucket `weftos-instance` carries the owning account as a tag, the first to create it owns the instance (a race has exactly one winner), and an account opening on an instance marked for another is refused with `<place> already belongs to client account '<owner>'; account '<you>' needs its own RustFS instance (one instance per client account, ADR-126 S5)`. A marker with no owner tag is not shared. The point is concrete: a principal's RustFS key has a single policy owned by whichever service granted it last, so two services on one instance overwrite each other (live test `two_services_on_one_instance_overwrite_each_others_key_policy`). For the same reason, on a client daemon whose S1 service (`WEFTOS_STORE_RUSTFS_ENV`) is on this account's instance, `ClientStore::open_rustfs_shared` rides that one service instead of starting a second (`client_store_rt::shared_service`); if the env names this instance but the service is not running that is an error. Limits: the marker stops a different account, not a second service of the same account in another process, and an unmarked instance (for example the shared WeftOS one) would be claimed by whichever account opens it first, so the listener check (`http://<spec.listen>`) stays the first line and the shared instance should be pre-marked.
- **Not done:** a live install as a client user on PG, the chain-side prune itself, and two `ClientStore`s on one log and account (not supported; each counts only its own events).

## S3 and S4 status (landed 2026-11-09)

- **S3** `feat/adr126-s3` fc9c081f7 (edge and Wasm nodes through the parent daemon, short-lived credentials; "S3
  results" and "Chain compaction" above) reached `target-0.8.5` inside S5's rebased tip (merge 96c78e454, in 64c547f4a),
  so nothing of S3 was left to land on its own. The chain-authenticated grant feed test `test/adr126-s3-chain-feed-e2e`
  628d0b5a2 and the rebase rehearsal `feat/adr126-s3-on-s1land` 35a9e8ca2 are superseded by that. Still unlanded, on
  aepod-xpc: `prep/adr126-s3-reqwest-client` c9b85e569 (reqwest `ObjectTransport` for the STS minter behind the
  `reqwest-client` feature; it adds a dependency, so it needs an owner diff-gate approval before any merge) and
  `opt/adr126-nodekeys-adapter` 646b57ff5 (S3 `NodeKeys` over S1's `node_key`, default-off `edge-creds` feature, four
  tests with the flag on; an option, not a plan). The owner chose readable `node-<id>` RustFS user names and one lane creating each RustFS user
  (2026-11-08, c66b9f90c); the bucket prefix rename and the pinned bucket creator went beyond that decision and are a
  separate owner call, still open (section 3); the Mac `wt/adr126-s1-names` slice, reconciled on `w2/t-names-and-conflicts`, waits for it.
- **S4** `feat/adr126-s4` (release stores) was rebased onto 64c547f4a as `rebase/adr126-s4-on-64c547f4a` (plan a37fb4b5b,
  rehearsed clean; merge-ready note 13772cab2) and is on `origin/target-0.8.5` as 35bcc00e8..3d4d52834 plus the owner's
  approvals commit e58899bb0: `ReleaseBackend` over `weftos-store`'s `ObjectBackend` behind the `store` feature;
  sweeper set-aside by server-side copy; `weaver store release publish`; Object Lock on release stores (locked publish
  and restore, `--lock`); non-root CI keys on locked buckets with a direct CI policy; replaced-manifest detection from
  `list_versions`; S1 lock read-back, extend-retention, lifecycle and multipart with a live proof on RustFS; Windows-host
  key rules and a key-portability test; the S4 key and path length budget (255-byte segments, planted-defect proofs);
  the retention and refused-delete test reads through S1's calls, not `curl` (9b65e506e). The approvals commit also
  covers `clawft-weave` depending on `weftos-release` with `store`. Not done: S4 wired into `release.sh`, and any release
  store on the PG RustFS.
- **Land order as it happened:** S1 (15dcf72c3), S2 (7ba34b071), S5 with S3 (64c547f4a), S4 (e58899bb0).
  `feat/adr131-d1` (ADR-131) lands next, by weftos-d1; `wt/adr126-s1-names` when confirmed. Recorded in
  `docs/plans/combined-release-readiness.md`.

## Consequences

- **Gains:**
  - one governed place for everything that isn't in git;
  - agents scale out across machines without copying assets;
  - releases get a store that can't be rewritten.
- **PG becomes a dependency for the store.** While PG is down, nodes keep what they have
  cached, and writes queue or fail. A second RustFS node with replication is future work,
  not part of this ADR.
- **New external dependency:** RustFS reached 1.0 on 2026-09-16, so S0 validates the S3
  operations we use before anything relies on them.
- **No POSIX semantics.** Through a mount, a rename is a copy plus a delete, and there's no
  locking. §4's one-writer rule is how we live with that.

## Open questions for the owner

1. ~~Can the existing project directory be served in place?~~ Answered by S0: no. It's
   imported once and the bucket becomes the source.
4. ~~Listing a store isn't logged by RustFS (S0). Is that acceptable?~~ Answered by the owner on
   2026-10-29: yes. Anyone with read access is assumed to see all of a store's metadata with a
   single read, so listings don't need an audit line. Multipart part uploads do, and so do refusals
   and bad signatures. Whether RustFS can be extended to cover them is under study.
2. ~~How long do archived buckets (from a removed worktree, project or node) live before
   they're deleted?~~ Answered by the owner 2026-11-08: archives do not expire. S1 carries it as a
   setting, not a number (see "Archive retention"): the default is for ever; the operator names days
   per kind of store.
3. Do cloud agent sessions get write access to worktree stores, or only read?
