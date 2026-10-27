#!/usr/bin/env bash
# Publish every publish=true workspace crate to crates.io in dependency order.
#
# - Skips crate versions that are already on crates.io (safe to re-run).
# - On HTTP 429 (crates.io new-crate limit: burst 5, then 1 per 10 min)
#   it sleeps and retries the same crate.
# - Needs CARGO_REGISTRY_TOKEN in the environment. Never pass it on argv.
#
# Usage: scripts/publish-crates.sh [--dry-run]   (dry run lists the order only)
set -euo pipefail

DRY=0
[ "${1:-}" = "--dry-run" ] && DRY=1
UA="weftos-publish (https://github.com/WeftOS/weftos)"
RETRY_SLEEP="${PUBLISH_RETRY_SLEEP:-660}"
MAX_TRIES="${PUBLISH_MAX_TRIES:-30}"

order=$(cargo metadata --format-version 1 --no-deps | python3 -c '
import json, sys
m = json.load(sys.stdin)
members = set(m["workspace_members"])
pk = {p["name"]: p for p in m["packages"] if p["id"] in members}
pub = {n: p for n, p in pk.items() if p.get("publish") is None or p["publish"]}
# crates.io needs every dependency (dev-deps with a version included) to exist first.
deps = {n: {d["name"] for d in p["dependencies"] if d["name"] in pub and d["name"] != n}
        for n, p in pub.items()}
done, out = set(), []
def visit(n, stack=()):
    if n in done: return
    if n in stack: sys.exit("dependency cycle at " + n)
    for d in sorted(deps[n]): visit(d, stack + (n,))
    done.add(n); out.append(n + " " + pub[n]["version"])
for n in sorted(pub): visit(n)
print("\n".join(out))
')

echo "Publish order ($(echo "$order" | wc -l | tr -d ' ') crates):"
echo "$order"
[ "$DRY" = 1 ] && exit 0
: "${CARGO_REGISTRY_TOKEN:?CARGO_REGISTRY_TOKEN must be set}"

exists() {
  code=$(curl -s -o /dev/null -w '%{http_code}' -A "$UA" "https://crates.io/api/v1/crates/$1/$2")
  [ "$code" = "200" ]
}

while read -r name ver; do
  if exists "$name" "$ver"; then
    echo "skip  $name $ver (already on crates.io)"
    continue
  fi
  tries=0
  while :; do
    tries=$((tries + 1))
    log=$(mktemp)
    if cargo publish -p "$name" --locked 2>&1 | tee "$log"; then
      rm -f "$log"
      echo "done  $name $ver"
      break
    fi
    if grep -qiE '429|too many|rate limit' "$log" && [ "$tries" -lt "$MAX_TRIES" ]; then
      rm -f "$log"
      echo "rate-limited on $name; sleeping ${RETRY_SLEEP}s (try $tries/$MAX_TRIES)"
      sleep "$RETRY_SLEEP"
      continue
    fi
    if grep -qi 'already exists' "$log"; then
      rm -f "$log"
      echo "skip  $name $ver (already exists)"
      break
    fi
    rm -f "$log"
    echo "::error::publish failed for $name $ver"
    exit 1
  done
done <<< "$order"
echo "All crates published."
