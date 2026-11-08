# ADR-126: Mesh object store: WeftOS-governed buckets for nodes, projects, worktrees and releases

- **Status**: Proposed (2026-10-29; the owner asked for a meshed S3 store and set the bucket model)
- **Updated**: 2026-10-29 (§4's worktree overlay is re-expressed as RVF layers and branch metadata in ADR-128)
- **Updated**: 2026-10-29 (S0 spike applied on PG and run; results in "S0 results"; open question 1 answered: import, not in place; §6 amended: the activity log must take RustFS's own log as well as its audit stream)
- **Updated**: 2026-10-30 (ADR-130 adds a GCS seed bucket as the tier above this store; this store becomes the optional L1 staging tier)
- **Updated**: 2026-11-07 (follower view §4a, Windows-safe fetch, notification/poll answers from ADR-131; S2 implemented on `feat/adr126-s2`, not yet landed: overlay, promotion, agent-git-flow hooks, union mount; §4 amended by the owner extension, board card f40828fb: the worktree tree itself is shared, so unmerged docs stay visible and another agent can merge)
- **Updated**: 2026-11-07 (S1 built on `feat/adr126-s1`: `weftos-store` crate, `weaver store`, daemon wiring, local-directory backend; RustFS S3 + admin transport added and verified against a throwaway RustFS 1.0.4, audit-stream and notification parsers fixed from captured records, `put_if_match`, release-store `append`/`sweep` roles; see "S1 status")
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
  Promotion has no conditional write, but re-checks each project etag right before its copy or
  delete and aborts with a conflict if it moved (the window is one head call; closing it needs an
  if-match write from S1). Registry signing and the claim epoch: see
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
  - **What is shared:** the files git can see (tracked, plus untracked and not ignored), under
    the reserved `.wt/tree/` prefix of the worktree bucket; a `git bundle` of the unmerged
    commits (`.wt/git/branch.bundle`); a manifest (`.wt/tree.manifest.json`) listing each file's
    SHA-256, which paths are uncommitted, and an index of docs and planning with titles; and a
    handoff record (`.wt/handoff.json`).
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
  - **Git still lands code.** The store makes the handoff possible across agents and machines: a
    second agent fetches the bundle and the tree, claims the merge, lands it with normal git, and
    marks it merged (which requires holding the claim). Promotion (below) then runs. The claim is
    advisory: there is no cross-client locking, two agents racing can both read back their own
    write, so the merger must still land through git's compare-and-swap. While a claim is live,
    only the holder can republish or re-hand-off; a claim older than 30 minutes can be taken over
    (compared with the claimer's clock).
  - **Reserved state never leaks:** everything under `.wt/` is hidden from overlay reads and
    listings and is never promoted as an asset. On promotion the shared tree, bundle, manifest and handoff
    record are archived with the rest.
  - **Discovery:** the daemon writes a small registry entry per worktree into the project bucket
    (`worktrees/<slug>.json`: branch, head, state, writer, merger, doc count). Worktree principals
    cannot write there, so "writes stay in the worktree bucket" still holds for agents.
- **One writer per path.** There's no cross-client locking. Parallel agents in one
  worktree write under `agents/<instance id>/`, and shared paths are read-mostly.

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
    look. A head move and a state change in one poll are both reported; a recreated slug (new
    bucket) is gone-then-new; one corrupt registry entry is reported, not fatal; an empty registry
    when worktrees were known is an error (wrong bucket?), not mass-gone, unless confirmed;
  - `search_docs` / `search`: docs and planning across every unmerged worktree, by path or title;
  - `sync_docs` / `sync-docs`: a read-only local mirror of one worktree's docs, pruned as docs leave;
  - `leftovers`: open worktrees idle 3 days, handoffs unclaimed 1 day, claims past the 30-minute
    TTL, merged entries still in the registry after a day (cleanup stuck), and timestamps in the
    future (writer clock ahead, so idle time is unknown). This is the forgotten-work detector;
  - `overlaps`: files two unmerged worktrees both changed differently, as a merge-conflict hint.
    It reports whether both share a base commit; if not, it over-reports. It also reports a file one
    worktree deleted that the other changed (a rename is a delete plus an add, so editing the old
    name is caught). That needs the manifest's `changes` list, which the script builds from
    `git diff --name-status --no-renames <base>` plus untracked files; a manifest published before
    that field existed gets content comparison only. Paths are compared, never hunks, so absence
    still does not prove a clean merge;
  - **a registry entry is a pointer, not authority:** before reading a bucket a follower checks the
    slug is well-formed and that the bucket's own `.wt/meta.json` names the same slug, so an entry
    cannot aim a follower's read grants at another worktree's bucket.
- **Waking and polling** (owner answers on ADR-131, 2026-11-07): head notifications reach devices
  through the mesh relay from one site subscriber (the PG WeftOS daemon). Followers treat a
  notification only as a wake-up and re-read the registry; a 30-60 s poll is the fallback and the
  source of truth. Polling costs one list plus one get per worktree. Registry entries are not
  signed in S2, so a follower trusts its own read grants and the bucket cross-check below; signed
  worktree heads are an open question if followers ever read from an untrusted relay.
- **Per-client stores:** a client project has its own buckets (ADR-131: one seed bucket and one
  inbox bucket per client), so a follower of a client project reads only that client's registry.
- **Clocks:** ages compare the registry's writer-side timestamp with the follower's clock, so the
  thresholds are days. They are a nudge, not a lock.
- **Not in scope:** an operator UI (card d18e1ab0), automatic merging, and enforcing that anyone
  acts on a leftover.

#### Gate for moving a project (Sansone) onto shared worktrees

Card 4c4319e0 holds the move until these have passed. S2 supplies the code; the evidence is still
owed and is **not** produced by the in-memory tests.

1. Against real RustFS (not `MemBackend`): publish, poll, search, sync-docs and claim with two
   different principals on two machines; a follower sees a publish within one poll interval.
2. A Windows node loads a worktree bucket natively (never tested; in scope now on aepod-xpc as the
   bucket-load proof, with the ProjFS drive after macOS D1 and Linux D2). Fetch already refuses
   reserved names (`CON`, `NUL`, `COM1`), trailing dots/spaces, `<>:"|?*`, `git~1` aliases and
   case-insensitive collisions on every platform; long paths and CRLF are still to try.
3. Metadata caching is only partial: measure `list` and `head` latency for a worktree of Sansone's
   size (file count, largest file) before assuming one-poll-per-worktree is affordable.
4. Leftover drill: leave a worktree open past the threshold and confirm `leftovers` reports it;
   hand one off and confirm a second agent on another machine lands it with git, then promotion
   and bucket deletion complete and the registry entry disappears.
5. Failure drill: kill the writer mid-publish, kill the merger mid-claim, take the daemon down
   during a registry refresh. Nothing may be lost, and a stale claim must be recoverable.
6. Follower drills on real clocks and a real registry: a writer machine a day ahead; a corrupt and
   an unknown-schema entry; an empty or wrong project bucket; a merged entry whose cleanup never
   ran; an interrupted `sync-docs` followed by a deleted doc; `overlaps` on two real worktrees with
   different bases (measure the false-positive rate); two overlapping `follow` runs sharing a state
   file (`FollowState::save` writes atomically via temp+rename but does not lock, so the last writer
   wins; `load` errors on a corrupt file rather than resetting).
7. ADR-131 D1 and D2 pass first (the card's prerequisite).

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

## S1 status (2026-11-07)

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
service), so setting those env names and issuing `PutBucketNotification` belongs to whoever deploys RustFS (Phase A).

**Conditional write (for S2).** `ObjectBackend::put_if_match(bucket, key, data, etag)` is a compare-and-swap (S3 `If-Match`): a
changed key is `BackendError::Stale`, a missing key is `NoObject`. The service has `StoreService::put_if_match(who, store,
key, data, etag)` (gated and logged like `put`), `svc.store` takes `"if_match": "<etag>"` on `put` (`store.v1`, not with
`if_none_match`), and RustFS gets `If-Match`. Take the etag from the last `head` or `put` result of the same backend
(opaque: MD5 on RustFS, BLAKE3 locally), and on `Stale` read again and retry. The local backend serialises swaps within the
daemon process; across processes only RustFS is atomic. S2 should use this instead of unconditional `put` for any
read-modify-write.

**CI and sweeper roles (release stores).** Two narrow access levels exist besides read, write and admin: `append` (get, head,
list, and a put that must carry `if_none_match`, so nothing is overwritten; no delete or copy) and `sweep` (get, head, list and
delete; no put). A new release store gets the well-known principals `agent:ci` as `append` and `agent:store-sweeper` as
`sweep` by default (`weaver store grant <name> <principal> append|sweep`). RustFS IAM has no condition for write-once, so the
key's policy for `append` allows PutObject and a direct S3 call with that key could still overwrite: write-once holds on the
governed path only. Immutability on the backend itself and retention are S4's. Readers of a release store (the project's
members) are named when the store is made.

Cogs reach the store through the cog data plane: the daemon serves `svc.store/<project ULID, lower
case>` (default store: that project's) and `svc.store/<node>` (the node's store), schema `store.v1`
(`contracts/cogs/README.md`), with ops put, get, head, list and delete. The caller becomes the principal
`cog:<instance id>` and needs a grant, so an ungranted cog is refused and a disabled grant refuses the
next call; one provider per project keeps a call inside its project, because a cross-project call
needs a policy grant and the node's own bucket cannot be granted. A reply is limited to 64 KiB, so a
larger object is an error, not a truncation. `store.put` and `store.get` on the weaver socket remain. Remote (mesh) callers
are modelled (`Caller::mesh`, exposure check) but no mesh transport calls it yet. `project add` (the daemon's
install action) creates the project store when the daemon runs an object store, and says so in its
report; a failure there is a note, never a failed add. Other project stores come from
`weaver store create`. Not done for PG:
nothing here was deployed, and `WEFTOS_STORE_RUSTFS_ENV` is unset by default (local-directory backend).

## S2 status (2026-11-07, branch `feat/adr126-s2`, not landed)

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
  grants and the activity log; the daemon refreshes the project registry entry after publish and
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
- **Known limits:** claims are advisory; claim expiry uses client clocks; `rclone union` cannot
  express tombstones, so deleting a project-only file through the mount is refused; rclone does not
  flush its write cache on unmount, so `worktree-store.sh umount` waits for the write-back delay;
  doc search matches path and title only, not content.

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
2. How long do archived buckets (from a removed worktree, project or node) live before
   they're deleted?
3. Do cloud agent sessions get write access to worktree stores, or only read?
