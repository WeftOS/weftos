#!/usr/bin/env bash
# Fixture tests for scripts/check-wasm-bindgen-pin.sh. Run: scripts/tests/check-wasm-bindgen-pin-test.sh
set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CHK="$ROOT/scripts/check-wasm-bindgen-pin.sh"
WORK="$(mktemp -d "${TMPDIR:-/tmp}/wbpin.XXXXXX")"; trap 'rm -rf "$WORK"' EXIT
pass=0; fail=0
# case NAME WANT_EXIT WORKFLOW_LINE [CONTAINERFILE_LINE]
run() {
    local name="$1" want="$2" wf="$3" cf="${4:-}" d="$WORK/$1"
    mkdir -p "$d/.github/workflows" "$d/scripts" "$d/ci"
    printf '[[package]]\nname = "wasm-bindgen"\nversion = "0.2.130"\n' >"$d/Cargo.lock"
    printf 'jobs:\n  x:\n    steps:\n      - run: %s\n' "$wf" >"$d/.github/workflows/a.yml"
    : >"$d/scripts/build.sh"
    [ -n "$cf" ] && printf 'FROM scratch\nRUN %s\n' "$cf" >"$d/ci/Containerfile"
    "$CHK" "$d" >"$d/out" 2>&1; local rc=$?
    if [ "$rc" = "$want" ]; then pass=$((pass+1)); echo "  ok    $name"
    else fail=$((fail+1)); echo "  FAIL  $name (exit $rc, wanted $want)"; sed 's/^/        /' "$d/out"; fi
}
run match          0 'cargo install wasm-bindgen-cli --version 0.2.130 --locked'
run flag-first     0 'cargo install --locked wasm-bindgen-cli --version 0.2.130'
run stale-workflow 1 'cargo install wasm-bindgen-cli --version 0.2.126 --locked'
run stale-flag-first 1 'cargo install --locked wasm-bindgen-cli --version 0.2.108'
run unpinned       1 'cargo install --locked wasm-bindgen-cli'
run stale-container 1 'cargo install wasm-bindgen-cli --version 0.2.130' 'cargo install wasm-bindgen-cli --version 0.2.99'
run binstall-at    1 'cargo binstall wasm-bindgen-cli@0.2.126'
run unrelated      0 'echo hello'
echo "$pass passed, $fail failed"; [ "$fail" = 0 ]
