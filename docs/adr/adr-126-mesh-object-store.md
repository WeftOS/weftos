# ADR-126: Mesh object store: WeftOS-governed buckets for nodes, projects, worktrees and releases

- **Status**: Proposed (2026-10-29; the owner asked for a meshed S3 store and set the bucket model)
- **Updated**: 2026-10-29 (§4's worktree overlay is re-expressed as RVF layers and branch metadata in ADR-128)
- **Updated**: 2026-10-29 (S0 spike applied on PG and run; results in "S0 results"; open question 1 answered: import, not in place; §6 amended: the activity log must take RustFS's own log as well as its audit stream)
- **Updated**: 2026-10-30 (ADR-130 adds a GCS seed bucket as the tier above this store; this store becomes the optional L1 staging tier)
- **Updated**: 2026-11-08 (chain compaction proposal and grant-ledger spike; review fixes and handoffs). Earlier: 2026-11-07 (live run against RustFS 1.0.4 in a loopback container: results in "S3 results"; edge presign fixed)
- **Updated**: 2026-11-07 (card b76c74a7: live STS minter and chain-fed grant ledger added to `weftos-edge-creds`, still tested against a mock store)
- **Updated**: 2026-11-07 (S3 implemented in `crates/weftos-edge-creds` against a mock store; see "S3 results". The ADR stays Proposed: S1, S2, S4, S5 are not landed)
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
  archived or dropped.
- **Who does the overlay:** the `weftos-store` client does it for daemons and agents. For a
  desktop mount, a small rclone `union` remote (worktree first, project as read-only)
  gives the same view.
- **Git trees and `target/` stay on local disk.** The store holds the gitignored layer only.
- **One writer per path.** There's no cross-client locking. Parallel agents in one
  worktree write under `agents/<instance id>/`, and shared paths are read-mostly.

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

## S3 results (2026-11-07)

S3 is built in `crates/weftos-edge-creds`, separate from S1's `weftos-store`. It does no I/O and
reads no clock, so the host supplies the parent link, the HTTP client and the time. It depends on
S1 only through two traits, `StsMinter` (RustFS AssumeRole and user disable/enable, card b76c74a7)
and `GrantCheck` (the chain-recorded grant).

- **Parent side (`EdgeBroker`).** For a node the link has authenticated, it checks the revoked set,
  a per-node rate limit (30 per minute) and the grant. It derives the bucket `n-<node id>` from the
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
| Reusing a node id after unpair inherits the old `n-<id>` bucket | `a_reused_node_id_does_not_inherit_the_previous_nodes_objects` | S1 (archive on unpair, section 3) | Archive and empty the bucket before the id can be paired again, or never reuse an id |

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

**Loopback wiring and schedule (2026-11-08).** `loopback::LoopbackChain` (feature `loopback-chain`, or tests) is an in-memory, append-only stand-in for the chain: grant events and signed snapshots, no I/O, no clock, no live chain. It adds `append`, `snapshot_now`, `recover` (newest snapshot that passes the follower checks, then an older one, then a full replay while nothing is pruned), `archive` (copies the events to cold storage and requires them to reproduce the signed snapshot) and `prune` (only after `archive`, only up to the newest usable snapshot). `SnapshotSchedule` is the scheduled-snapshot option: **off by default**, `every_events(n)` and/or `every_secs(t)`, never due when nothing is new; the chain takes a snapshot on `append` only if the host built it with an enabled schedule. Nothing in the crate turns it on. Twelve tests; a forged newest snapshot falls back, a forged one after a prune is an error rather than a silent partial ledger, prune is refused without a matching archive, and the schedule defaults off. Five planted defects (schedule always due, prune without the archive check, unchecked newest snapshot, ignoring the pruned bound, archive without the history check) each fail a test. Still the chain owner's: the real signer, where the snapshot event sits, and the cadence to use.

**Bucket name parity with S1 (2026-11-08).** S1's `StoreName::Node` (weftos-store `name.rs`) derives `n-<id>` with an id of lowercase `[a-z0-9-]`, at most 61 characters. This crate's `node_bucket` accepts a strict subset (at most 48, no leading or trailing dash), so every id it accepts maps to the bucket S1 derives; `node_bucket_agrees_with_s1s_store_name_rule_and_is_never_looser` pins that. When S1's crate lands as a dependency, replace `node_bucket` with `StoreName::bucket` and drop the extra restriction only if the owner wants ids S1 allows.

**Native HTTP client, prepared and unmerged (2026-11-08).** Branch `prep/adr126-s3-reqwest-client` (commit c9b85e56, on top of this branch) adds `reqwest_transport::ReqwestTransport`, an `ObjectTransport` over `reqwest` behind the off-by-default feature `reqwest-client` (host side only, not wasm-clean). It sends the already-signed request with the signer's own percent-encoding, never follows redirects (a signed request must not be replayed elsewhere), and returns generic errors with no URL (a presigned query would leak). Four tests against a loopback listener; three planted defects (redirects followed, error text with the URL, URL encoding) fail a test. The diff gate rejects it, as expected: `new-dep` for `reqwest` in `crates/weftos-edge-creds/Cargo.toml` (already a workspace dependency, optional here) and `net` for `crates/weftos-edge-creds/src/reqwest_transport.rs` (the client and the test listeners). **The owner's choice is one step:** add the two approvals for those paths in `config/diff-gate-approvals.toml` and merge the branch; or decline, and the host keeps supplying its own `ObjectTransport`. Nothing on this branch is needed by the rest of the crate.

**Consume side of S1's archive-on-unpair and seq guard (2026-11-08).** S1 had not posted either call when this was written (its branch has no unpair archive and no seq guard), so S3 holds the contract as traits and tests it against mocks. When S1 posts commit ids, implement the traits over the real calls; nothing else changes.

- **`lifecycle::NodeLifecycle`** with `NodeArchiver` (S1: `archive_node` copies the bucket to the archive and empties it, idempotently; `bucket_is_empty`) and `NodeProvisioner` (`RustfsMinter` implements it). `unpair` applies the event and cuts the node off, seals the id, archives, and only then deletes the user and policy. A failed archive leaves the user disabled, not deleted, and the id sealed; calling `unpair` again finishes. `check_pairable` refuses a sealed id until its bucket is empty, and fails closed when the probe errors. The sealed set is memory only: after a restart `seal_from_history` rebuilds it from the chain's unpair events, and the empty-bucket probe is what unseals.
- **`grants::SeqGuard`** (`GrantLedger::set_seq_guard`; `MaxSeqJump(n)` as a stand-in). A refused event returns `Applied::OutOfRange` and changes nothing, head included. Default is no guard, because the chain's `seq` is shared with other events and a legitimate gap size is the chain owner's call. A ledger rebuilt by `restore` has no guard until the host installs it again. The two `#[ignore]` regression tests still describe the unguarded default; the guard is covered by separate tests.
- Eight tests; five planted defects (delete before archive, no sealing, probe fails open, guard ignored, state changed before the guard refuses) each fail a test.

**What gate C6 needs.** The repository does not define C6; the card (b76c74a7) states it as: a WeftOS principal gets a scoped STS session for one folder, another principal is refused, and revocation refuses the next request within 1 s, all with WeftOS-minted credentials, on the PG RustFS from S0. Everything but the PG instance is built and was run against a loopback RustFS 1.0.4. What is needed from the owner: (1) permission to run a harness against the PG RustFS (barred to agents today); (2) its endpoint and an admin key supplied at run time, not committed; (3) an HTTP client for the minter, either the owner-approved `reqwest` branch or the throwaway harness kept outside the repo; (4) two throwaway node ids and a bucket prefix agreed so the run cannot touch real stores; (5) the run window and who removes the test users afterwards. Expected result is the loopback table above, with the revoke time measured on PG.

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
