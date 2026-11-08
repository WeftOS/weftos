#!/bin/bash
# ADR-126 S4 spike: what RustFS 1.0.4 gives a release bucket for immutability and retention
# (versioning, Object Lock, bucket policy, lifecycle). Read-only against everything except a few
# uniquely named buckets it creates. Point it ONLY at a THROWAWAY RustFS, never PG:
#   scripts/store/release-lock-spike.sh <env file with MESH_S3_ENDPOINT/_ACCESS_KEY/_SECRET_KEY>
# Findings: docs/research/adr126-s4-release-bucket-immutability-2026-11-08.md
set -u
. "$1"
E=$MESH_S3_ENDPOINT
BODY=$(mktemp -p "${SPIKE_DIR:-$PWD}" .spike-body.XXXXXX)
trap 'rm -f "$BODY"' EXIT
req() { local m=$1 p=$2; shift 2; curl -s -o "$BODY" -w '%{http_code}' -X "$m" --aws-sigv4 "aws:amz:us-east-1:s3" --user "$MESH_S3_ACCESS_KEY:$MESH_S3_SECRET_KEY" "$@" "$E$p"; }
show() { printf '%-52s -> %s  %s\n' "$1" "$2" "$(head -c 200 "$BODY" | tr '\n' ' ')"; }
t() { local label=$1; shift; show "$label" "$(req "$@")"; }
realver() { req GET "/$1?versions&prefix=$2" >/dev/null; python3 - "$BODY" <<'PY'
import re,sys
x=open(sys.argv[1]).read()
m=re.search(r'<Version>.*?<VersionId>([^<]*)</VersionId>',x,re.S)
print(m.group(1) if m else '')
PY
}
echo '###### versioning (plain bucket)'
B=sp-$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' \n')
echo "== plain bucket $B-p"
t "create"                       PUT /$B-p
t "get versioning (default)"      GET "/$B-p?versioning"
t "put versioning Enabled"        PUT "/$B-p?versioning" --data '<VersioningConfiguration><Status>Enabled</Status></VersioningConfiguration>'
t "get versioning"                GET "/$B-p?versioning"
t "put k=v1"                      PUT /$B-p/k --data-binary v1
t "put k=v2 (unconditional)"      PUT /$B-p/k --data-binary v2
t "put k=v3 if-none-match:*"      PUT /$B-p/k --data-binary v3 -H 'If-None-Match: *'
t "list versions"                 GET "/$B-p?versions&prefix=k"
t "delete k (versioned)"          DELETE /$B-p/k
t "get k after delete"            GET /$B-p/k
t "list versions after delete"    GET "/$B-p?versions&prefix=k"

P=$B
echo '###### Object Lock'
B=sp2-$(head -c4 /dev/urandom | od -An -tx1 | tr -d ' \n')
t "create lock bucket"            PUT /$B -H 'x-amz-bucket-object-lock-enabled: true'
UNTIL=$(date -u -d '+1 day' +%Y-%m-%dT%H:%M:%SZ)
echo "-- COMPLIANCE"
t "put c1 COMPLIANCE"             PUT /$B/c1 --data-binary c -H 'x-amz-object-lock-mode: COMPLIANCE' -H "x-amz-object-lock-retain-until-date: $UNTIL"
VC=$(realver $B c1); echo "version=$VC"
t "delete locked version"         DELETE "/$B/c1?versionId=$VC"
t "delete locked version +bypass" DELETE "/$B/c1?versionId=$VC" -H 'x-amz-bypass-governance-retention: true'
t "shorten retention to +1h"      PUT "/$B/c1?retention&versionId=$VC" --data "<Retention><Mode>COMPLIANCE</Mode><RetainUntilDate>$(date -u -d '+1 hour' +%Y-%m-%dT%H:%M:%SZ)</RetainUntilDate></Retention>"
t "switch to GOVERNANCE"          PUT "/$B/c1?retention&versionId=$VC" --data "<Retention><Mode>GOVERNANCE</Mode><RetainUntilDate>$UNTIL</RetainUntilDate></Retention>"
t "extend retention +2d"          PUT "/$B/c1?retention&versionId=$VC" --data "<Retention><Mode>COMPLIANCE</Mode><RetainUntilDate>$(date -u -d '+2 day' +%Y-%m-%dT%H:%M:%SZ)</RetainUntilDate></Retention>"
t "plain delete (marker)"         DELETE /$B/c1
t "get after plain delete"        GET /$B/c1
t "get by version still readable" GET "/$B/c1?versionId=$VC"
t "re-put if-none-match after marker" PUT /$B/c1 --data-binary c2 -H 'If-None-Match: *'
echo "-- GOVERNANCE"
t "put g1 GOVERNANCE"             PUT /$B/g1 --data-binary g -H 'x-amz-object-lock-mode: GOVERNANCE' -H "x-amz-object-lock-retain-until-date: $UNTIL"
VG=$(realver $B g1)
t "delete gov version no bypass"  DELETE "/$B/g1?versionId=$VG"
t "delete gov version bypass"     DELETE "/$B/g1?versionId=$VG" -H 'x-amz-bypass-governance-retention: true'
echo "-- legal hold, copy, list"
t "put l1 (no retention)"         PUT /$B/l1 --data-binary l
VL=$(realver $B l1)
t "legal hold ON"                 PUT "/$B/l1?legal-hold&versionId=$VL" --data '<LegalHold><Status>ON</Status></LegalHold>'
t "delete held version"           DELETE "/$B/l1?versionId=$VL"
t "legal hold OFF"                PUT "/$B/l1?legal-hold&versionId=$VL" --data '<LegalHold><Status>OFF</Status></LegalHold>'
t "copy l1 -> q/l1"               PUT /$B/q/l1 -H "x-amz-copy-source: /$B/l1"
t "copy result lock headers"      HEAD /$B/q/l1 -I
t "list v2 hides markers? c1"     GET "/$B?list-type=2&prefix=c1"
t "list v2 all"                   GET "/$B?list-type=2"
echo "-- default retention + copy"
t "put default retention GOV 1d"  PUT "/$B?object-lock" --data '<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled><Rule><DefaultRetention><Mode>GOVERNANCE</Mode><Days>1</Days></DefaultRetention></Rule></ObjectLockConfiguration>'
t "put d1"                        PUT /$B/d1 --data-binary d
t "copy d1 -> d2 inherits?"       PUT /$B/d2 -H "x-amz-copy-source: /$B/d1"
t "head d2"                       HEAD /$B/d2 -I
t "remove default retention"      PUT "/$B?object-lock" --data '<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>'
t "disable object lock?"          PUT "/$B?object-lock" --data '<ObjectLockConfiguration></ObjectLockConfiguration>'
t "suspend versioning on lock bkt" PUT "/$B?versioning" --data '<VersioningConfiguration><Status>Suspended</Status></VersioningConfiguration>'
t "create plain bucket"           PUT /$B-plain
t "plain bucket: enable lock"     PUT "/$B-plain?object-lock" --data '<ObjectLockConfiguration><ObjectLockEnabled>Enabled</ObjectLockEnabled></ObjectLockConfiguration>'

echo "== bucket policy deny delete on $P-p"
t "put policy deny delete"        PUT "/$P-p?policy" --data "{\"Version\":\"2012-10-17\",\"Statement\":[{\"Effect\":\"Deny\",\"Principal\":\"*\",\"Action\":[\"s3:DeleteObject\"],\"Resource\":[\"arn:aws:s3:::$P-p/*\"]}]}"
t "get policy"                    GET "/$P-p?policy"
t "put k2"                        PUT /$P-p/k2 --data-binary x
t "delete k2 as root"             DELETE /$P-p/k2
echo "== lifecycle on $P-p"
t "put lifecycle expire 1d"       PUT "/$P-p?lifecycle" --data '<LifecycleConfiguration><Rule><ID>r</ID><Status>Enabled</Status><Filter><Prefix>tmp/</Prefix></Filter><Expiration><Days>1</Days></Expiration></Rule></LifecycleConfiguration>'
t "get lifecycle"                 GET "/$P-p?lifecycle"
t "put noncurrent expiry"         PUT "/$P-p?lifecycle" --data '<LifecycleConfiguration><Rule><ID>n</ID><Status>Enabled</Status><Filter><Prefix></Prefix></Filter><NoncurrentVersionExpiration><NoncurrentDays>7</NoncurrentDays></NoncurrentVersionExpiration></Rule></LifecycleConfiguration>'
