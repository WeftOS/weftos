# ADR-126 S4: release bucket immutability and retention on RustFS

Date: 2026-11-08. Status: plan plus a measured spike. Nothing here is deployed.
Governs: ADR-126 §S4 ("a rebuild can't overwrite" and "retention applies"). ADR-126 is still Proposed and is not changed by this note.
Spike: `scripts/store/release-lock-spike.sh` against a throwaway loopback RustFS 1.0.4, plus the ignored live tests in `crates/weftos-release/tests/store_live.rs`. Never run either against PG.

## Where we are

`weftos-release` already holds "cannot overwrite" in three places: the API has no unconditional put or delete, the server answers 412 to `If-None-Match: *`, and CI's grant has no DeleteObject. Retention is logical and done by the daemon's sweeper (tombstone, delete manifest, collect blobs after a grace period). What the bucket itself guarantees is nothing yet: anyone with a delete grant, or the root key, can still remove or replace an object. This note asks what RustFS can add underneath.

## What RustFS 1.0.4 does (measured)

| Probe | Result |
|---|---|
| Conditional put, plain, versioned, and locked buckets | 412 on an existing key in all three. After a delete marker, `If-None-Match: *` succeeds (the key counts as absent). |
| Versioning | Works. An unconditional overwrite keeps the old version. A delete adds a delete marker; reads 404, the version stays readable by `versionId`. |
| Object Lock | Must be requested when the bucket is created (`x-amz-bucket-object-lock-enabled: true`). It turns versioning on. It cannot be enabled on an existing bucket (409), cannot be disabled (400), and versioning cannot be suspended (409). |
| COMPLIANCE retention | Deleting the locked version: 403, with or without the governance bypass header. Retention cannot be shortened (403) or switched to GOVERNANCE (403). It can be extended (200). |
| GOVERNANCE retention | Deleting the locked version: 403. With `x-amz-bypass-governance-retention` the same caller succeeds (204). |
| Legal hold | Works per version. A held version cannot be deleted (403). |
| Default bucket retention | Accepted. New objects get it. It can be removed again. |
| Copy | A copy does **not** carry the source's lock. A COMPLIANCE +3d source copied to a new key came back GOVERNANCE for the 1-day bucket default. |
| Plain DELETE (no `versionId`) on a locked object | 204: it adds a delete marker. The locked version survives but the key reads 404. Object Lock does **not** stop a logical delete. |
| Bucket policy `Deny s3:DeleteObject` | Stored, and enforced against the root key too (403). So it works as a backstop only if it can exclude the sweeper, which was not tested. |
| Multipart upload with Object Lock | Retention set on CreateMultipartUpload carries to the completed object (COMPLIANCE, date kept) and its delete is refused (403). `If-None-Match: *` on CompleteMultipartUpload is enforced: 412 when the key existed before Create, and 412 when a plain PUT created it between Create and Complete. Abort leaves nothing listed; a 412'd Complete leaves the upload listed. |
| Lifecycle, with the scanner sped up (`RUSTFS_ILM_DEBUG_DAY_SECS=5`, first action about 20 s in) | Expiration removes an expired plain object. On a versioned bucket it makes a delete marker. `NoncurrentVersionExpiration` removes noncurrent versions. An `ExpiredObjectDeleteMarker` rule removes an orphan marker. **A current version under COMPLIANCE, GOVERNANCE or a legal hold was skipped entirely** (no marker, nothing removed), and a retained noncurrent version stayed readable. Retention dates were real wall-clock +1 day, so retention won against the sped-up clock. |

### What the audit stream shows (RustFS audit webhook to a local sink)

The multipart, audit and lifecycle probes were run by hand (curl SigV4 calls against a throwaway RustFS with `RUSTFS_AUDIT_ENABLE` and a python sink); only the versioning, lock and policy probes are scripted in `scripts/store/release-lock-spike.sh`. The sink log is not committed.

| Call | Event? |
|---|---|
| CreateBucket with the lock header | Yes (`s3:BucketCreated:*`) |
| PutObjectRetention, PutObjectLegalHold | Yes (`ObjectCreated:PutRetention`, `...:PutLegalHold`) |
| CompleteMultipartUpload | Yes, no versionId; the 412'd ones arrive too, named `ObjectCreated:CompleteMultipartUpload` with **no status**, so the event name alone looks like a success |
| DeleteObject creating a delete marker | Yes (`ObjectRemoved:DeleteMarkerCreated`, status 200) |
| DeleteObject with `versionId` refused by a lock or hold (403) | Yes (`ObjectRemoved:Delete`), no status, no versionId. S0 said 403 refusals are not in the stream; these are, because the handler (not the authoriser) refuses them. |
| **PutBucketVersioning, PutObjectLockConfiguration, PutBucketLifecycleConfiguration, DeleteBucketLifecycle, PutBucketPolicy, DeleteBucketPolicy** | **No event** |
| CreateMultipartUpload, UploadPart, AbortMultipartUpload, ListMultipartUploads | No event |
| Deletions done by the lifecycle scanner | No event |

No event carries a `versionId`. Failed operations have no `status` field, so a consumer must not treat a bare event name as success.

Our code on a locked, versioned bucket: publish, retry, refused rebuild, retention sweep with quarantine copy, listings, retired-id check and audit all pass unchanged, with S1's locked-bucket creation and locked puts, and the server really holds the COMPLIANCE retention and refuses to delete it (`live_release_store_on_a_locked_versioned_bucket`).

## What each mechanism buys us

- **Conditional put** already stops a second writer from replacing a live key. It says nothing about a key that was deleted.
- **Versioning** means no overwrite or delete destroys data: the bytes remain recoverable by version id. It costs storage and needs a purge path.
- **Object Lock** is the only thing that makes a version undeletable for a fixed time even for a principal that holds DeleteObjectVersion. COMPLIANCE cannot be undone by the owner either; GOVERNANCE can, by anyone granted the bypass.
- **Neither** stops a logical delete. CI must still be denied DeleteObject.

## Recommendation

1. **Create release buckets with Object Lock on.** It cannot be added later, so this is a creation-time decision. S1 now has `create_locked_bucket` and `create_locked_store`; `weftos_release::s3::create_locked_release_bucket` wraps it for provisioning. `weaver store create` does not expose it yet. Release buckets are new, so there is nothing to migrate.
2. **Stamp what a publish writes.** `ReleasePublisher::with_lock(LockPolicy { mode, floor_secs })` locks the manifest and every new blob until `now + floor` (from the publisher's clock, not the claimed `created_at`). The verb takes `--lock governance|compliance --lock-days N`. COMPLIANCE for the `stable` channel, GOVERNANCE elsewhere is the suggestion; the owner decides. COMPLIANCE cannot be undone, but a mistaken publish is already fixed by a new build id, so the cost is storage. A policy on a backend without lock fails the publish rather than writing unprotected objects.
3. **A blob shared by a later release keeps the lock it was first written with.** There is no way to extend it: S1 has no PutObjectRetention call and a second put is refused by `If-None-Match`. The logical rule (the sweeper never deletes a referenced blob) still holds; only the physical floor for the later release can be shorter than its own. Ask S1 for an extend-retention call, then stamp after commit.
4. **A copy takes only the bucket default**, never the source's lock. The sweeper's quarantine copy is therefore unlocked, and a restore must write the bytes back locked: `Sweeper::with_lock` does that on the rare restore path (the normal delete still never reads the bytes). A bucket default retention (PutObjectLockConfiguration) would also cover this, but S1 does not set it yet.
5. **Physical removal is the lifecycle scanner's job.** The sweeper's delete stays logical (delete marker). A rule with `NoncurrentVersionExpiration` plus `ExpiredObjectDeleteMarker` removed noncurrent versions and orphan markers, and skipped anything still under retention or a hold. That makes lifecycle the reclaim path, and makes the lock the thing that decides when. Only seen on RustFS 1.0.4 with a sped-up scanner.
6. **Grants** (S1 owns them; measured with non-root keys in `crates/weftos-release/tests/store_ci_key.rs`):
   - **S1's CI `append` role cannot write at all** at RustFS (read, list only; a conditional create is a 403 too). Its puts go through the daemon's service with the daemon's key, which is the only way write-once is a server-side fact, because RustFS IAM rejects the `s3:if-none-match` condition key.
   - **A key that writes straight to the bucket** (what `weaver store release publish` does with an `--s3-env`) needs `s3:PutObject`, and to send lock headers also `s3:PutObjectRetention`: `PutObject` alone is a 403 on a locked put. `weftos_release::s3::direct_ci_policy` is that role plus a Deny on any put without a lock mode (condition `Null s3:object-lock-mode`, which RustFS accepts and enforces), so every version it writes is locked, and a copy (no lock headers) is refused. No DeleteObject.
   - **The direct role can still replace a key it can write.** An unconditional put with lock headers over a COMPLIANCE-locked key succeeded and read back as current; the locked original survives, readable by version id. Only the publisher's own `If-None-Match` stops that, which a hostile or buggy CI step can skip. Use the direct role only where CI is trusted not to; otherwise use S1's daemon-mediated append role. A reader catches a replaced blob (hash mismatch) but not a replaced manifest signed with CI's own key; seeing more than one version of a key would, which needs a list-versions call S1 does not have.
   - Sweeper: DeleteObject, copy and put on `blobs/*` and `quarantine/*`, tombstone put; not BypassGovernanceRetention. (S1's `sweep` role has delete only and cannot put, so a locked restore needs the put added.)
   - Bypass: the owner only.
7. **Audit.** Changes to versioning, lock configuration, lifecycle and policy leave no event, and neither do the lifecycle scanner's deletions. The log therefore cannot show who weakened a bucket or what the scanner removed. Lock retention itself is only as trustworthy as the control of who holds bucket-level admin; keep that to the owner and have the daemon read the configuration back and compare it to what was provisioned.
8. **Streaming.** Retention headers on CreateMultipartUpload carry to the completed object and `If-None-Match: *` works on Complete, so a multipart path can keep both guarantees. S1's transport has no multipart yet.

## Replaced-manifest detection and the CI route switch (2026-11-08)

**Detection.** The current view cannot show a replaced object: a key written twice reads as its newest
bytes, and a validly signed replacement verifies. `ReleaseReader::history()` lists every version through
`ReleaseBackend::versions` (S1 `list_versions`) and reports:

- `Replaced { key, contents }` when a manifest, tombstone or blob has more than one distinct content
  (compared by etag). The sweeper's restore writes the same bytes again, so it is not a finding.
- `ManifestDeleted { key }` when a manifest's newest entry is a delete marker and no tombstone retired the
  build id (retention deletes the manifest only after writing the tombstone).
- `checked: false` when the store keeps no history (a bucket without Object Lock). An empty report then
  proves nothing, and the type says so.

Proven on RustFS 1.0.4 (`live_release_store_on_a_locked_versioned_bucket`): after publishes, a retirement
and a collection history is clean; after an unconditional put of a second signed manifest the plain read
accepts it and history reports it. The reader needs a grant that lists versions (S1's read policy carries
`ListBucketVersions`); it reports, it does not repair. Old versions are never removed by this crate.

**The two CI routes, one switch** (`weftos_release::route`, `DEFAULT_CI_ROUTE`):

| | Daemon (`append` through `svc.store`) | Direct (`direct_ci_policy`) |
|---|---|---|
| Write-once enforced by | the service: an `append` put without `If-None-Match` is refused | the publisher only; the key can replace what it can write |
| Overwrite by a rogue CI key | refused and logged | succeeds; found afterwards by `history()` |
| Lock headers | service uses root's key, so no IAM change | key needs `s3:PutObject` + `s3:PutObjectRetention` |
| Needs | the weaver daemon in the publish path | S3 reachable from CI |
| Audit | every put in the store's activity log by principal | RustFS audit stream only (no version ids) |
| Today | library + tests; **no CLI client yet** | works end to end (`weaver store release publish`) |

Both pass the same publish scenario (`tests/ci_route.rs`). The default is `Direct` because it is the only
route a standalone CLI can run; flip the one constant to `Daemon` when the owner picks it. The verb then
refuses with a message saying it has no daemon client yet, rather than quietly using the other route.
`--ci-route direct|daemon` overrides per run.

**Sweeper put, not yet granted.** S1's `sweep` role is read + delete. A sweep needs more than that, in
order: a tombstone put, a quarantine copy (the service has no copy call, so the service route does get +
put), and for a locked store a locked put on restore. Measured through the service
(`the_sweep_role_cannot_run_a_sweep_and_deletes_nothing`): under `sweep` the first write (the tombstone) is
refused with `PutObject needs append`, before any delete, so nothing is lost; under `write` the whole sweep
runs (`the_write_role_runs_the_whole_sweep_including_quarantine`). Options for the owner: (a) give the
sweeper `write` on the release store (it can then overwrite too, which weakens the sweeper's own
separation from CI), (b) add a `sweep+put` level that may put under `tombstones/*`, `quarantine/*` and
`blobs/*` only, plus a service copy call. (b) is the narrower grant and is what S1 is asked to prepare.

## S4 side of S1's lock read-back, lifecycle, extend-retention and multipart (prepared on the mock, 2026-11-08)

Each seam is a defaulted method on `ReleaseBackend` (or `LifecycleBackend`), implemented on `MemoryBackend`
and tested there. `StoreBackend` (S3) keeps the defaults until S1 lands the call; the switch is one `impl`
override per row, no publisher change.

| Seam | S4 behaviour (tested on the mock) | S1 call the S3 adapter will use | Until it lands |
|---|---|---|---|
| Lock read-back | `lock_config()`; a locked publish reads it first and refuses an unlocked bucket with one error, before any write | read of the bucket's Object Lock configuration | default `None`: the first locked put is the test |
| Extend retention | `retention_of` + `extend_lock`; a reused blob is lengthened to the later floor and mode, never shortened or weakened (mock refuses both) | PutObjectRetention on the current version plus a read of its retention | default: a reused blob keeps the lock it was written with |
| Lifecycle | `provision_lifecycle(rules)` sets then reads back; zero days, a mismatch and a silent server are errors | put and get of the bucket lifecycle configuration | not callable on S3 |
| Multipart | `publish_files`: hashes in chunks, uploads through `put_file_if_absent` (mock reads 1 MiB parts), manifest identical to the bytes path | multipart create/part/complete with the lock headers on create and `If-None-Match` on complete (both measured on RustFS) | default reads the whole file |

**The 512 MiB limit stays in `weaver store release publish` (`MAX_FILE_BYTES`) until multipart lands**,
because without it the file path still holds the whole file in memory. When S1's multipart call exists,
override `StoreBackend::put_file_if_absent` with it and set `MAX_FILE_BYTES` to `u64::MAX`.

## Not verified

- Whether lifecycle respects a retention date at real day scale (only a sped-up scanner against wall-clock dates was seen), and whether it expires a version once its retention has lapsed.
- Why an unlocked object's delete marker disappeared in the lock bucket without an `ExpiredObjectDeleteMarker` rule.
- Bucket policy conditions that exempt the sweeper principal from a Deny.
- Behaviour at scale (many versions of one key, listing cost).
- Multipart through our own code: only the raw S3 behaviour was probed, since S1's transport has no multipart call.

## Order of work

1. S1: decide the CI route (daemon-mediated append, or the direct role above); a put for the `sweep` role (locked restore); list-versions; extend-retention call; default bucket retention; multipart (with lock headers on create); expose `--locked` on `weaver store create`.
2. S4 (done in this branch): lock policy on publish, locked restore, locked-bucket helper, `--lock` on the verb, the direct CI policy, live tests with non-root keys, version-history audit (`history()`), both CI routes behind `DEFAULT_CI_ROUTE`.
3. S4 next: stamp after commit once S1 has extend-retention; read-back check of lock configuration; lifecycle rule provisioning.
4. Owner: pick the modes and floors per channel.
