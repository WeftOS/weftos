#!/usr/bin/env bash
# ADR-126: try S1's and S3's "one creator of the user and policy" branches together in a THROWAWAY
# worktree (never feat/adr126-s5), run the non-live integration tests, then the loopback live slices.
#
#   S1_REF=<branch-or-sha> S3_REF=<branch-or-sha> WORK=<empty dir outside the repo> \
#   WEFTOS_STORE_TEST_RUSTFS_BIN=/path/to/rustfs scripts/store/one-creator-integ.sh
#
# BASE (default feat/adr126-s5) is checked out detached; S1_REF then S3_REF are merged into it; a
# conflict stops the run and is printed. Nothing is committed to any branch, nothing is pushed, and
# the worktree is removed afterwards (KEEP=1 keeps it). The live part starts and stops its own
# loopback RustFS (scripts/store/integ-live-test.sh); it never touches PG. Gate C6 itself needs the
# owner's PG items (ADR-126 "What gate C6 needs"); this script only runs its loopback counterpart.
set -uo pipefail
: "${S1_REF:?set S1_REF}" "${S3_REF:?set S3_REF}" "${WORK:?set WORK to a path that does not exist yet}"
BASE="${BASE:-feat/adr126-s5}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
[ ! -e "$WORK" ] || { echo "WORK exists: $WORK" >&2; exit 1; }
export PATH="$HOME/.cargo/bin:$PATH"
cleanup() { [ -n "${KEEP:-}" ] || { git -C "$REPO" worktree remove --force "$WORK" 2>/dev/null; git -C "$REPO" worktree prune; }; }
trap cleanup EXIT
git -C "$REPO" worktree add --detach "$WORK" "$BASE" >/dev/null || exit 1
cd "$WORK" || exit 1
for ref in "$S1_REF" "$S3_REF"; do
    git merge --no-ff -q -m "throwaway: merge $ref" "$ref" || { echo "CONFLICT merging $ref:"; git diff --name-only --diff-filter=U; exit 2; }
done
rc=0
run() { echo "== $*"; timeout "${SLICE_TIMEOUT:-1500}" scripts/build.sh "$@" 2>&1 | grep -E '^test result|FAILED|panicked|^error' | grep -v ' 0 passed; 0 failed'; [ "${PIPESTATUS[0]}" = 0 ] || { echo "FAILED: $*"; rc=1; }; }
run test weftos-edge-creds --features mock,keygen,loopback-chain
run test weftos-edge-creds
run test weftos-release
run test weftos-store --features rustfs
run test clawft-weave --filter store
run clippy weftos-store weftos-edge-creds weftos-release
if [ -n "${WEFTOS_STORE_TEST_RUSTFS_BIN:-}" ]; then
    echo "== live (throwaway loopback RustFS)"
    scripts/store/integ-live-test.sh || rc=1
fi
echo "rustfs left: $(pgrep -x rustfs | wc -l)"
exit $rc
