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
| Lifecycle (expiration, noncurrent-version expiration) | The configuration is accepted and read back. Whether RustFS acts on it was not observed (it needs days). |

Our code on a locked, versioned bucket: publish, retry, refused rebuild, retention sweep with quarantine copy, listings, retired-id check and audit all pass unchanged (`live_release_store_on_a_locked_versioned_bucket`).

## What each mechanism buys us

- **Conditional put** already stops a second writer from replacing a live key. It says nothing about a key that was deleted.
- **Versioning** means no overwrite or delete destroys data: the bytes remain recoverable by version id. It costs storage and needs a purge path.
- **Object Lock** is the only thing that makes a version undeletable for a fixed time even for a principal that holds DeleteObjectVersion. COMPLIANCE cannot be undone by the owner either; GOVERNANCE can, by anyone granted the bypass.
- **Neither** stops a logical delete. CI must still be denied DeleteObject.

## Recommendation

1. **Create release buckets with Object Lock on.** It cannot be added later, so this is a creation-time decision. S1's `create_bucket` needs a `lock` option that sends the header. Release buckets are new, so there is nothing to migrate.
2. **Set a bucket default retention** as a floor (GOVERNANCE, N days). Every version then carries at least that lock, including anything written with no headers. A copy gets only this default, never the source's stronger lock, so a restore by the sweeper (copy-back from `quarantine/`) must be re-stamped by the sweeper before it counts as protected.
3. **Stamp manifests explicitly** with `retain-until = created_at + floor`. COMPLIANCE for the `stable` channel, GOVERNANCE elsewhere. This is the owner's call: COMPLIANCE cannot be undone, but a mistaken publish is already fixed by a new build id, so the cost is storage.
4. **Blobs outlive every release that names them.** After the manifest commits, the publisher extends each blob's retention to the manifest's retain-until (extension is allowed under both modes; it needs the version id from the put or head response). Never shorten.
5. **Retention then has two phases.** The sweeper's decision stays logical (tombstone, delete marker). Physical removal happens only after retain-until, either by lifecycle noncurrent expiration or by the sweeper deleting specific versions. Today `delete` only makes markers, so storage is never reclaimed; the S3 adapter needs a version-aware delete or a lifecycle rule, and which of those RustFS enforces needs to be observed first.
6. **Grants** (S1 owns them; this is the requirement):
   - CI: PutObject, GetObject, PutObjectRetention (extend); no DeleteObject, no DeleteObjectVersion, no BypassGovernanceRetention, no bucket-level configuration (versioning, lock, policy, lifecycle).
   - Sweeper: DeleteObject and DeleteObjectVersion, copy and put on `blobs/*` and `quarantine/*`, tombstone put; not BypassGovernanceRetention.
   - Bypass: the owner only.
7. **Audit.** RustFS does not audit PutBucketVersioning or bucket policy changes (ADR-126 S0). Whether it audits lock and lifecycle configuration was not checked. Lock settings are only as trustworthy as the log of who changed the bucket.

## Not verified

- That RustFS expires noncurrent versions or delete markers from a lifecycle rule.
- Bucket policy conditions that exempt the sweeper principal from a Deny.
- Object Lock with multipart uploads (the streaming path).
- Audit coverage of lock and lifecycle calls.
- Behaviour at scale (many versions of one key, listing cost).
- Whether S1's transport can send the lock headers (`x-amz-object-lock-mode`, `retain-until-date`) on a put; today it cannot.

## Order of work

1. S1: `create_bucket` with lock; transport support for the lock headers and for versioned deletes. (Not in this worktree.)
2. S4: a `Retention` stamp on publish (headers on manifest and blob puts, extension after commit); a version-aware delete in `DeletableBackend`.
3. Observe lifecycle enforcement on a throwaway RustFS before relying on it.
4. Owner: pick the modes and floors per channel.
