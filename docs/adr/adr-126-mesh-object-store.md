# ADR-126: Mesh object store: WeftOS-governed buckets for nodes, projects, worktrees and releases

- **Status**: Proposed (2026-10-29; the owner asked for a meshed S3 store and set the bucket model)
- **Updated**: 2026-10-29 (§4's worktree overlay is re-expressed as RVF layers and branch metadata in ADR-128)
- **Updated**: 2026-10-29 (S0 spike applied on PG and run; results in "S0 results"; open question 1 answered: import, not in place; §6 amended: the activity log must take RustFS's own log as well as its audit stream)
- **Updated**: 2026-10-30 (ADR-130 adds a GCS seed bucket as the tier above this store; this store becomes the optional L1 staging tier)
- **Updated**: 2026-11-07 (live run against RustFS 1.0.4 in a loopback container: results in "S3 results"; edge presign fixed)
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
host's. Still not built: key generation for a node's secret (the caller supplies it), the native HTTP
client for the minter (a `reqwest` one needs an owner diff-gate approval), and moving archived
buckets. `node_bucket` is the one-line ADR rule and should be replaced by S1's mapping when S1
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
