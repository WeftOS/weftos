#!/usr/bin/env bash
# ADR-126 S1: dry-run plan for wiring RustFS audit + bucket notifications into the WeftOS
# activity log. This script DEPLOYS NOTHING and talks to NOTHING: it validates its inputs,
# then prints (and writes into --out) the env block, the PutBucketNotification body and the
# commands the person deploying RustFS runs by hand. See docs/guides/rustfs-audit-deploy.md.
set -euo pipefail

usage() {
  cat <<'USAGE'
Usage: rustfs-audit-plan.sh --receiver URL --queue-dir DIR [--target NAME] [--bucket B]... [--out DIR]
  --receiver URL   the AUDIT url from the daemon's store-ingest-urls (ends /audit; the notify url is derived)
  --queue-dir DIR  RustFS-side directory for undelivered records (must exist, owned by RustFS)
  --target NAME    webhook target name, letters/digits/_/- (default: primary)
  --bucket B       a bucket to attach the notification to (repeatable; e.g. n-<node>, p-<ulid>)
  --out DIR        write env.snippet, notification.xml, notification.json and RUNBOOK.txt here
Never run against production from this script: it only prints.
USAGE
}

receiver="" queue="" target="primary" out="" buckets=()
while [ $# -gt 0 ]; do
  case "$1" in
    --receiver) receiver="${2:?}"; shift 2 ;;
    --queue-dir) queue="${2:?}"; shift 2 ;;
    --target) target="${2:?}"; shift 2 ;;
    --bucket) buckets+=("${2:?}"); shift 2 ;;
    --out) out="${2:?}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
    *) echo "unknown argument: $1" >&2; usage >&2; exit 2 ;;
  esac
done

fail() { echo "error: $*" >&2; exit 2; }
[ -n "$receiver" ] && [ -n "$queue" ] || { usage >&2; exit 2; }
[[ "$target" =~ ^[A-Za-z0-9_-]+$ ]] || fail "--target: letters, digits, '_' and '-' only"
[[ "$receiver" =~ ^https?://[^/]+(/.*)?$ ]] || fail "--receiver: need http(s)://host[:port][/path]"
[[ "$queue" = /* ]] || fail "--queue-dir: need an absolute path"
for b in "${buckets[@]+"${buckets[@]}"}"; do
  [[ "$b" =~ ^[a-z0-9][a-z0-9-]{2,62}$ ]] || fail "--bucket $b: not an S3 bucket name"
done

# RustFS refuses webhook endpoints whose origin is not allow-listed (loopback included);
# the allow-list takes the ORIGIN only, no path.
origin="$(printf '%s' "$receiver" | sed -E 's#^(https?://[^/]+).*#\1#')"
T="$(printf '%s' "$target" | tr '[:lower:]' '[:upper:]')"

env_block="# --- RustFS environment, added to the unit/launcher that starts RustFS (restart needed) ---
RUSTFS_OUTBOUND_ALLOW_ORIGINS=${origin}
RUSTFS_AUDIT_ENABLE=true
RUSTFS_AUDIT_WEBHOOK_ENABLE_${T}=on
RUSTFS_AUDIT_WEBHOOK_ENDPOINT_${T}=${receiver}
RUSTFS_AUDIT_WEBHOOK_QUEUE_DIR_${T}=${queue}/audit
RUSTFS_NOTIFY_ENABLE=true
RUSTFS_NOTIFY_WEBHOOK_ENABLE_${T}=on
RUSTFS_NOTIFY_WEBHOOK_ENDPOINT_${T}=${receiver%/audit}/notify
RUSTFS_NOTIFY_WEBHOOK_QUEUE_DIR_${T}=${queue}/notify"

arn="arn:rustfs:sqs::${target}:webhook"
xml="<NotificationConfiguration><QueueConfiguration><Id>weftos-activity</Id><Queue>${arn}</Queue><Event>s3:ObjectCreated:*</Event><Event>s3:ObjectRemoved:*</Event></QueueConfiguration></NotificationConfiguration>"
json="{\"QueueConfigurations\":[{\"Id\":\"weftos-activity\",\"QueueArn\":\"${arn}\",\"Events\":[\"s3:ObjectCreated:*\",\"s3:ObjectRemoved:*\"]}]}"

plan="$env_block

# --- 1. create the queue dirs on the RustFS host (the RustFS user must own them) ---
mkdir -p '${queue}/audit' '${queue}/notify'

# --- 2. after RustFS restarts, confirm the audit target is online (SigV4 admin call; use the
#        RustFS root key from the secret store, never paste it into a shell history) ---
#   GET <endpoint>/rustfs/admin/v3/audit/target/list      -> expect source \"env\", online
#   Do NOT configure audit targets through set-config-kv: it silently drops enable/endpoint.

# --- 3. PutBucketNotification, once per bucket (the target must exist first, else HTTP 500) ---
#   body: ${xml}
#   aws CLI form (--endpoint-url <RustFS>, root or an admin key):
#   aws s3api put-bucket-notification-configuration --bucket <bucket> \\
#     --notification-configuration '${json}'
#   In Rust: RustfsBackend::put_bucket_notification(bucket, \"${target}\")"
for b in "${buckets[@]+"${buckets[@]}"}"; do
  plan="$plan
#   -> bucket: ${b}"
done
plan="$plan

# --- 4. verify: one governed put through WeftOS must appear in the activity log as the cog's
#        principal (see the guide). 403 refusals are NOT in the audit stream: they are read from
#        rustfs.log (parse_denial). ---"

printf '%s\n' "$plan"
if [ -n "$out" ]; then
  mkdir -p "$out"
  printf '%s\n' "$env_block" > "$out/env.snippet"
  printf '%s\n' "$xml" > "$out/notification.xml"
  printf '%s\n' "$json" > "$out/notification.json"
  printf '%s\n' "$plan" > "$out/RUNBOOK.txt"
  echo "wrote plan files to $out (nothing was applied)" >&2
fi
