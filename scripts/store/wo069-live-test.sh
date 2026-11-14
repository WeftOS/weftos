#!/usr/bin/env bash
# WO-069 lane C: live check of the worktree browse reader against ONE throwaway loopback RustFS.
#
#   WEFTOS_STORE_TEST_RUSTFS_BIN=/path/to/rustfs scripts/store/wo069-live-test.sh
#
# Never point this at PG or any shared RustFS. It starts its own server on 127.0.0.1 with a random
# port and credentials under target/, runs the browse live test and the S2 two-machine scenario,
# prints pass/fail per test, then kills the server and removes its data (trap). No weaver, no daemon.
set -uo pipefail

BIN="${WEFTOS_STORE_TEST_RUSTFS_BIN:?set WEFTOS_STORE_TEST_RUSTFS_BIN to the rustfs binary}"
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$REPO/target"
WORK="$(mktemp -d "$REPO/target/wo069-live.XXXXXX")"
PID=""
cleanup() {
    [ -n "$PID" ] && { kill "$PID" 2>/dev/null || true; wait "$PID" 2>/dev/null || true; }
    rm -rf "$WORK"
    echo "cleanup: server stopped, $WORK removed"
}
trap cleanup EXIT INT TERM

rand_hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
PORT="$(python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])')"
ACCESS="live$(rand_hex 6)"
SECRET="$(rand_hex 20)"
mkdir -p "$WORK/data"
env RUSTFS_ACCESS_KEY="$ACCESS" RUSTFS_SECRET_KEY="$SECRET" \
    "$BIN" server --address "127.0.0.1:$PORT" "$WORK/data" >"$WORK/server.log" 2>&1 &
PID=$!
up=""
for _ in $(seq 1 60); do
    code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/" || true)"
    # 000 = not listening yet, 503 = listening but still starting (first bucket call would fail)
    if [ "$code" != "000" ] && [ "$code" != "503" ]; then up=1; break; fi
    sleep 0.5
done
[ -n "$up" ] && sleep 2
[ -n "$up" ] || { echo "rustfs did not come up (see $WORK/server.log)" >&2; exit 1; }
umask 077
printf 'MESH_S3_ENDPOINT=http://127.0.0.1:%s\nMESH_S3_ACCESS_KEY=%s\nMESH_S3_SECRET_KEY=%s\nDATA_DIR=%s\n' \
    "$PORT" "$ACCESS" "$SECRET" "$WORK/data" >"$WORK/live.env"
export WEFTOS_STORE_TEST_RUSTFS_ENV="$WORK/live.env"
echo "throwaway rustfs 127.0.0.1:$PORT (pid $PID)"

cd "$REPO"
rc=0
run() {
    local name="$1"; shift
    echo "== $name"
    "$@" -- --ignored --nocapture --test-threads=1 >"$WORK/$name.out" 2>&1
    grep -E 'TIMING|test result|^test |panicked' "$WORK/$name.out"
    if grep -q 'test result: ok. 1 passed' "$WORK/$name.out"; then echo "PASS $name"; else echo "FAIL $name"; tail -25 "$WORK/$name.out"; rc=1; fi
}
run s2_browse_live cargo test -p weftos-store --features rustfs --test s2_browse_live
run s2_done_when cargo test -p weftos-store --features rustfs --test s2_done_when two_machines_share_a_worktree_on_a_real_rustfs
exit $rc
