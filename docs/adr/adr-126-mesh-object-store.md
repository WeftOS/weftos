# ADR-126: Mesh object store: WeftOS-governed buckets for nodes, projects, worktrees and releases

- **Status**: Proposed (2026-10-29; the owner asked for a meshed S3 store and set the bucket model)
- **Updated**: 2026-10-29 (§4's worktree overlay is re-expressed as RVF layers and branch metadata in ADR-128)
- **Updated**: 2026-10-29 (S0 spike applied on PG and run; results in "S0 results"; open question 1 answered: import, not in place; §6 amended: the activity log must take RustFS's own log as well as its audit stream)
- **Updated**: 2026-10-30 (ADR-130 adds a GCS seed bucket as the tier above this store; this store becomes the optional L1 staging tier)
- **Updated**: 2026-11-07 (follower view §4a, Windows-safe fetch, notification/poll answers from ADR-131; S2 implemented on `feat/adr126-s2`, not yet landed: overlay, promotion, agent-git-flow hooks, union mount; §4 amended by the owner extension, board card f40828fb: the worktree tree itself is shared, so unmerged docs stay visible and another agent can merge)
- **Updated**: 2026-11-07 (S1 built on `feat/adr126-s1`: `weftos-store` crate, `weaver store`, daemon wiring, local-directory backend; RustFS S3 + admin transport added and verified against a throwaway RustFS 1.0.4, audit-stream and notification field names still unverified, see "S1 status")
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
  around the overlay has no recorded base and is treated as a conflict (fail safe). Reads do not
  record a base, so a project edit between a worktree's read and its first write goes unnoticed.
  The run is idempotent and
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
    the next refresh; `follow --full` skips it. A head move and a state change in one poll are both reported; a recreated slug (new
    bucket) is gone-then-new; one corrupt registry entry is reported, not fatal; an empty registry
    when worktrees were known is an error (wrong bucket?), not mass-gone, unless confirmed;
  - `search_docs` / `search`: docs and planning across every unmerged worktree, by path or title;
  - `sync_docs` / `sync-docs`: a read-only local mirror of one worktree's docs, pruned as docs leave;
  - `leftovers`: open worktrees idle 3 days, handoffs unclaimed 1 day, claims past the 30-minute
    TTL, merged entries still in the registry after a day (cleanup stuck), and timestamps in the
    future (writer clock ahead, so idle time is unknown). This is the forgotten-work detector;
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
  thresholds are days. They are a nudge, not a lock. Claims are the exception and are checked as
  described in §4.
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
to match (`action` is `S3Action(GetObjectAction)`, `object` can be `"None"`). **Still unverified:**
the audit-stream record (successful operations) and bucket-notification field names. Audit did not
start from the environment variables I tried (`RUSTFS_AUDIT_ENABLE` with a webhook target named `_A` or
unnamed, plus a queue dir, on a fresh data dir; RustFS 1.0.4 wrote no record to a loopback sink in
three tries, and its own log for those runs was not updated, so the reason is unknown; it may need the
audit target set through the admin config API instead), so no record was captured; the parser still reads field names from the S0 write-up. Cogs reach the store through the cog data plane: the daemon serves `svc.store/<project ULID, lower
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
  - F2, grant and revoke locking, and F9, RustFS audit ingestion. Both are in the S1 layer and
    belong to the S1 session.
  - F7 and F13 to F20, not yet addressed (see
    `docs/research/adr126-s1-s2-adversarial-review-2026-11-08.md`), including S1 `store.promote`
    prefix validation.
  - `slug_of` in `worktree-store.sh` is not injective: `feat/x` and `feat-x` map to one store.
  - The owner rejected the rclone mount. The union mount (`mount-config`, `mount`, `umount`) is
    proposed to be parked: kept in the code, dropped from the S2 done-when and from any gate, and
    marked parked in the script help and §4 (proposal in §1 of
    `docs/plans/mesh-store-next-steps-2026-11-08.md`, awaiting the owner). The desktop access
    path is the materialise-and-watch route in ADR-131.

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
