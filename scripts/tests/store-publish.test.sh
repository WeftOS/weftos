#!/usr/bin/env bash
# Test for scripts/release/store-publish.sh: fake weaver, no network.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
s="$here/release/store-publish.sh"
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
pass=0; fail=0
ok() { if eval "$2"; then pass=$((pass+1)); else fail=$((fail+1)); echo "FAIL: $1"; fi; }
mkdir -p "$t/dist" "$t/bin"; echo a >"$t/dist/a.txt"
printf '#!/usr/bin/env bash\necho "$@" >"%s/called"\n' "$t" >"$t/bin/weaver"; chmod +x "$t/bin/weaver"
echo '{}' >"$t/key.json"; chmod 600 "$t/key.json"
printf 'MESH_S3_ENDPOINT=http://x\nMESH_S3_ACCESS_KEY=a\nMESH_S3_SECRET_KEY=b\n' >"$t/s3.env"; chmod 600 "$t/s3.env"
export GITHUB_SHA=0123456789abcdef0123456789abcdef01234567 WEAVER="$t/bin/weaver"
ST=weftos://mesh1/projects/01ARZ3NDEKTSV4RRFFQ69G5FAV/releases/stable/store
run() { WEFTOS_RELEASE_STORE="$ST" WEFTOS_RELEASE_STORE_KEY="$t/key.json" WEFTOS_RELEASE_STORE_S3ENV="$t/s3.env" "$@"; }

out=$("$s" "$t/dist" v1.2.3); ok "off by default prints disabled" '[[ $out == *disabled* ]]'
out=$("$s" "$t/dist" v1.2.3 --execute); ok "off even with --execute" '[[ $out == *disabled* && ! -e $t/called ]]'
out=$(WEFTOS_RELEASE_STORE_PUBLISH=1 run "$s" "$t/dist" v1.2.3); ok "dry run prints command" '[[ $out == *"DRY RUN"* && $out == *"--build-id v1.2.3"* ]]'
ok "dry run does not call weaver" '[[ ! -e $t/called ]]'
ok "dry run does not print secrets" '[[ $out != *MESH_S3_SECRET* ]]'
WEFTOS_RELEASE_STORE_PUBLISH=1 run "$s" "$t/dist" v1.2.3 --execute >/dev/null
ok "execute calls weaver" 'grep -q "store release publish $ST" "$t/called"'
ok "route is direct" 'grep -q -- "--ci-route direct" "$t/called"'
out=$(WEFTOS_RELEASE_STORE_PUBLISH=1 WEFTOS_RELEASE_STORE_LOCK=governance:30 run "$s" "$t/dist" v1.2.3)
ok "lock passed" '[[ $out == *"--lock governance"* && $out == *"--lock-days 30"* ]]'
ok "bad lock refused" '! WEFTOS_RELEASE_STORE_PUBLISH=1 WEFTOS_RELEASE_STORE_LOCK=forever run "$s" "$t/dist" v1.2.3 >/dev/null 2>&1'
ok "bad tag refused" '! WEFTOS_RELEASE_STORE_PUBLISH=1 run "$s" "$t/dist" main >/dev/null 2>&1'
ok "bad store name refused" '! WEFTOS_RELEASE_STORE_PUBLISH=1 WEFTOS_RELEASE_STORE_KEY="$t/key.json" WEFTOS_RELEASE_STORE_S3ENV="$t/s3.env" WEFTOS_RELEASE_STORE=weftos://m/x "$s" "$t/dist" v1.2.3 >/dev/null 2>&1'
chmod 644 "$t/key.json"
ok "group-readable key refused" '! WEFTOS_RELEASE_STORE_PUBLISH=1 run "$s" "$t/dist" v1.2.3 >/dev/null 2>&1'
chmod 600 "$t/key.json"; ln -s a.txt "$t/dist/l"
ok "symlink in dir refused" '! WEFTOS_RELEASE_STORE_PUBLISH=1 run "$s" "$t/dist" v1.2.3 >/dev/null 2>&1'
echo "store-publish tests: $pass passed, $fail failed"; [ "$fail" -eq 0 ]
