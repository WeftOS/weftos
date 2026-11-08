# ADR-126: Mesh object store: WeftOS-governed buckets for nodes, projects, worktrees and releases

- **Status**: Proposed (2026-10-29; the owner asked for a meshed S3 store and set the bucket model)
- **Updated**: 2026-10-29 (§4's worktree overlay is re-expressed as RVF layers and branch metadata in ADR-128)
- **Updated**: 2026-10-29 (S0 spike applied on PG and run; results in "S0 results"; open question 1 answered: import, not in place; §6 amended: the activity log must take RustFS's own log as well as its audit stream)
- **Updated**: 2026-10-30 (ADR-130 adds a GCS seed bucket as the tier above this store; this store becomes the optional L1 staging tier)
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

**The daemon now has the receiver** (`clawft-weave/src/store_ingest.rs`, 2026-11-08): a loopback HTTP listener, off unless
`WEFTOS_STORE_INGEST_ADDR` is set, POST only, 1 MiB cap, authenticated by a 256-bit secret in the URL path
(`store-ingest-token`, 0600; the URLs are written to `store-ingest-urls`), because RustFS's webhook `auth_token` is unverified.
It feeds `ingest_audit_line` and `ingest_notification`, which now drop records for buckets that are not the daemon's stores and
the daemon's own root-key requests (already logged on the governed path), so a record reaches the activity log only if it came from a
principal's own key. Notifications still invalidate caches whoever wrote. Verified end to end against a throwaway
RustFS (`live_a_request_straight_to_rustfs_reaches_the_activity_log`, `--ignored`, `WEFTOS_STORE_TEST_RUSTFS_BIN`): a put made
straight to RustFS with a cog's key shows as that cog's `PutObject` (audit) and a notification event. Not wired yet: attaching
`PutBucketNotification` to buckets automatically when a store is created (the runbook does it by hand), and reading
`rustfs.log` for 403 refusals (`ingest_log_line` has no caller).

**Adversarial review of S1 (2026-11-08), fixed:** a grant change saved but not pushed (RustFS down, a crash between the two)
is now made good by `StoreService::reconcile()` at daemon start, and backend pushes are serialised per service so an older snapshot
cannot undo a newer policy; a bucket made but never recorded is adopted when its tags are the store's own (a foreign bucket still
refuses); an archived store's grant cannot be switched back on; `svc.store get` checks the size before reading the object; the
cog service startup no longer lists objects on the async runtime; `weaver store put|get --as` now leaves a `store.as` chain
record naming the operator. **Known and not fixed:** `archive` can lose an object put by a request that passed the gate just
before it; listings and `FsBackend` etags read whole files (the local backend is the development path); a mesh-id change
renames the stores' `weftos://` names (the bucket names do not move) and needs a migration; `project add`'s store hook
joins a blocking thread; (the receiver now authenticates and drops unknown buckets; see above).

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
   they're deleted?~~ Answered as a setting, not a number (S1 2026-11-08, see "Archive retention"): the
   default is for ever; the operator names days per kind of store.
3. Do cloud agent sessions get write access to worktree stores, or only read?
