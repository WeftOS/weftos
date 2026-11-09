#!/usr/bin/env bash
# Run the ignored `client_store::tests::live` tests (ADR-126 S5 on S1's RustFS transport) against
# two THROWAWAY RustFS instances on loopback, then stop them and remove their data.
#
#   WEFTOS_STORE_TEST_RUSTFS_BIN=/path/to/rustfs scripts/store/client-live-test.sh
#   WEFTOS_STORE_TEST_RUSTFS_BIN=... scripts/store/client-live-test.sh scripts/build.sh test weftos-store --features rustfs --ignored
#
# With no arguments it runs the S5 live tests and S2's. With arguments it runs that command instead, with
# WEFTOS_STORE_TEST_RUSTFS_ENV (S1's live tests) pointing at the first instance too.
#
# Never point this at PG or any shared RustFS: it starts its own processes on 127.0.0.1 with
# random ports and credentials, in a temporary directory under target/. The Rust tests read the
# endpoints from the env files named in WEFTOS_STORE_TEST_CLIENT_RUSTFS_ENVS (a:b).
set -euo pipefail

BIN="${WEFTOS_STORE_TEST_RUSTFS_BIN:?set WEFTOS_STORE_TEST_RUSTFS_BIN to the rustfs binary}"
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$REPO/target"
WORK="$(mktemp -d "$REPO/target/client-live.XXXXXX")"
PIDS=()
cleanup() {
    for p in ${PIDS[@]+"${PIDS[@]}"}; do kill "$p" 2>/dev/null || true; done
    for p in ${PIDS[@]+"${PIDS[@]}"}; do wait "$p" 2>/dev/null || true; done
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

free_port() { python3 -c 'import socket; s = socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
rand_hex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }

start() {
    local name="$1" port access secret
    port="$(free_port)"
    access="live$(rand_hex 6)"
    secret="$(rand_hex 20)"
    mkdir -p "$WORK/$name/data"
    RUSTFS_ACCESS_KEY="$access" RUSTFS_SECRET_KEY="$secret" \
        "$BIN" server --address "127.0.0.1:$port" "$WORK/$name/data" >"$WORK/$name/log" 2>&1 &
    PIDS+=("$!")
    local up=""
    for _ in $(seq 1 60); do
        code="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/" || true)"
        if [ "$code" != "000" ]; then up=1; break; fi
        sleep 0.5
    done
    [ -n "$up" ] || { echo "rustfs $name did not come up (see $WORK/$name/log)" >&2; exit 1; }
    umask 077
    printf 'MESH_S3_ENDPOINT=http://127.0.0.1:%s\nMESH_S3_ACCESS_KEY=%s\nMESH_S3_SECRET_KEY=%s\nDATA_DIR=%s\n' \
        "$port" "$access" "$secret" "$WORK/$name/data" >"$WORK/$name.env"
}

start a
start b
export WEFTOS_STORE_TEST_CLIENT_RUSTFS_ENVS="$WORK/a.env:$WORK/b.env"
export WEFTOS_STORE_TEST_RUSTFS_ENV="$WORK/a.env"
cd "$REPO"
if [ $# -gt 0 ]; then
    "$@"
else
    # Every slice runs under a timeout (SLICE_TIMEOUT seconds, default 1200); all run, and the
    # script fails if any did.
    rc=0
    run() { echo "== $*"; timeout "${SLICE_TIMEOUT:-1200}" "$@" || { echo "FAILED (or timed out after ${SLICE_TIMEOUT:-1200}s): $*" >&2; rc=1; }; }
    # S5 on S1's transport, including S5 x S2 x S1 (a promoted worktree stays on the client's instance)
    run scripts/build.sh test clawft-weave --filter client_store::tests::live --ignored
    # S2's own live tests on the same throwaway instance: the shared worktree, land preview and caps,
    # concurrent claims with one winner, a follower across three identities. (The notification
    # wake-up test needs a notify target and scripts/store/notify-sink.py, so it is not run here.)
    run scripts/build.sh test weftos-store --features rustfs --filter two_machines_share_a_worktree_on_a_real_rustfs --ignored
    run scripts/build.sh test weftos-store --features rustfs --filter live_land_preview --ignored
    run scripts/build.sh test weftos-store --features rustfs --filter live_concurrent_claims --ignored
    run scripts/build.sh test weftos-store --features rustfs --filter live_follower_sees_unmerged --ignored
    exit $rc
fi
