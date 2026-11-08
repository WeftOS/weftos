# RustFS audit and notification wiring (ADR-126 S1) — runbook for whoever deploys RustFS

Nothing here is applied by WeftOS. `scripts/store/rustfs-audit-plan.sh` is a dry run: it checks
its inputs and prints the env block, the `PutBucketNotification` body and the commands below. Run
the real steps by hand, on the RustFS host, during the Phase A window the owner approves.

Verified against a throwaway loopback RustFS 1.0.4 on 2026-11-08 (ADR-126, "S1 status").

## What WeftOS needs from RustFS

| Stream | Carries | Does not carry |
|---|---|---|
| Audit webhook | Put, Get, Head, Copy, Delete, CreateBucket, CompleteMultipartUpload, with the access key | List, ListBuckets, UploadPart, 403 refusals, bad signatures; no object size |
| Bucket notification | Created/Removed events with object size and etag (the object key is percent-encoded) | Reads |
| `rustfs.log` | `s3_authorization_denied` lines (the 403 refusals) | — |

The activity log joins the three (`parse_audit`, `parse_notification`, `parse_denial` in
`crates/weftos-store/src/audit.rs`). The access key is mapped back to a principal by the daemon.

## Steps

1. **Receiver.** The daemon has one (`crates/clawft-weave/src/store_ingest.rs`). It is **off unless**
   `WEFTOS_STORE_INGEST_ADDR` is set in the daemon's environment (for example `127.0.0.1:7788`;
   loopback only, anything else is refused), and it needs the RustFS backend (`WEFTOS_STORE_RUSTFS_ENV`).
   On start it writes `store-ingest-urls` (0600) in the daemon's runtime dir: two lines,
   `http://127.0.0.1:<port>/store/ingest/<secret>/audit` and `.../notify`. The secret is in the URL
   because RustFS's webhook `auth_token` is not verified on 1.0.4; treat the file like a key
   (`store-ingest-token` holds the secret and is stable across restarts). Use those two URLs as the
   audit endpoint and the notify endpoint. A wrong secret is a plain 404; bodies over 1 MiB are refused.
   What it logs: requests made with a principal's own key, for buckets that are this daemon's stores.
   Not repeated: the daemon's own root-key requests (the governed path logged them) and any other bucket.
2. **Plan.** `scripts/store/rustfs-audit-plan.sh --receiver <audit url> (the notify URL is the same with /notify) --queue-dir <dir> --bucket n-<node> --bucket p-<ulid> --out <dir>`
3. **Env.** Add the printed block to the RustFS unit and restart it. The names that matter:
   - `RUSTFS_OUTBOUND_ALLOW_ORIGINS=<origin>` — RustFS refuses webhook endpoints (loopback too)
     unless the origin (scheme://host:port, no path) is listed. Without it the targets fail
     silently; this is why three earlier attempts wrote nothing.
   - `RUSTFS_AUDIT_ENABLE=true`, then **named** targets: `RUSTFS_AUDIT_WEBHOOK_ENABLE_<NAME>=on`,
     `..._ENDPOINT_<NAME>`, `..._QUEUE_DIR_<NAME>`. The unnamed form is read as a comment and
     never delivers.
   - The same three under `RUSTFS_NOTIFY_WEBHOOK_*_<NAME>` plus `RUSTFS_NOTIFY_ENABLE=true`.
   - The queue dirs must exist and belong to the RustFS user before it starts.
   - Do not use the admin `set-config-kv` call for audit targets: it drops `enable` and `endpoint`.
4. **Check the target.** `GET /rustfs/admin/v3/audit/target/list` (SigV4, service `s3`) must show
   the target with `source: "env"` and online.
5. **Notification, per bucket.** `PutBucketNotification` to `arn:rustfs:sqs::<NAME>:webhook` for
   `s3:ObjectCreated:*` and `s3:ObjectRemoved:*`, for every store bucket (`n-<node>`, `p-<ulid>`,
   `w-…`, `r-…`). It returns HTTP 500 if the target does not exist yet, so do it after step 4.
   Rust: `RustfsBackend::put_bucket_notification(bucket, "<NAME>")`; or the `aws s3api` command
   the plan prints. New stores need this too, so repeat it when `weaver store create` or
   `project add` makes a bucket (the daemon does not do it yet).
6. **Verify.** Put one object as a cog through `svc.store`, then confirm the `PutObject` entry
   appears with the cog's principal; disable the grant and confirm the next call is refused and
   logged. Direct 403s show up from `rustfs.log` only.

## Known limits

- Batched posts (`batch_size` above 1), multipart, versioned operations and `auth_token` on the
  webhook are not verified; the parser reads only `Records[0]`.
- Write-once for release stores (`append` grant) holds on the governed path only; RustFS IAM has
  no condition for it.
- Secrets: the RustFS root key stays in the secret store; never put it in the env snippet or a
  shell history. Per-principal keys are written by the daemon as 0600 files.
