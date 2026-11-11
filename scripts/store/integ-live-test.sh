#!/usr/bin/env bash
# ADR-126 cross-lane integration tests (S1xS4, S1xS3, S1xS2, S5xS2/S1) against THROWAWAY RustFS
# instances on loopback, then stop them and remove their data.
#
#   WEFTOS_STORE_TEST_RUSTFS_BIN=/path/to/rustfs scripts/store/integ-live-test.sh
#
# Never point this at PG or any shared RustFS: it starts its own processes on 127.0.0.1 with random
# ports and credentials, in a temporary directory under target/. Each cargo test run is checked
# individually; the script exits non-zero if any failed (all are run regardless).
set -uo pipefail

BIN="${WEFTOS_STORE_TEST_RUSTFS_BIN:?set WEFTOS_STORE_TEST_RUSTFS_BIN to the rustfs binary}"
[ -x "$BIN" ] || { echo "not executable: $BIN" >&2; exit 1; }
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
mkdir -p "$REPO/target"
WORK="$(mktemp -d "$REPO/target/integ-live.XXXXXX")"
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
    local EXTRA_ENV=(); [ "$name" = n ] && EXTRA_ENV=("${NOTIFY_ENV[@]}")
    port="$(free_port)"
    access="live$(rand_hex 6)"
    secret="$(rand_hex 20)"
    mkdir -p "$WORK/$name/data"
    env RUSTFS_ACCESS_KEY="$access" RUSTFS_SECRET_KEY="$secret" ${EXTRA_ENV[@]+"${EXTRA_ENV[@]}"} \
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
# A third instance with a webhook notify target `wake` pointing at a loopback sink, for the L1 follower
# relay test (needs RUSTFS_NOTIFY_ENABLE and RUSTFS_OUTBOUND_ALLOW_ORIGINS, or put_bucket_notification
# fails with "No notify targets configured").
SINK_PORT="$(free_port)"
python3 "$REPO/scripts/store/notify-sink.py" "$SINK_PORT" "$WORK/events.jsonl" &
PIDS+=("$!")
: >"$WORK/events.jsonl"
NOTIFY_ENV=(RUSTFS_OUTBOUND_ALLOW_ORIGINS="http://127.0.0.1:$SINK_PORT" RUSTFS_NOTIFY_ENABLE=true
    RUSTFS_NOTIFY_WEBHOOK_ENABLE_WAKE=on RUSTFS_NOTIFY_WEBHOOK_ENDPOINT_WAKE="http://127.0.0.1:$SINK_PORT"
    RUSTFS_NOTIFY_WEBHOOK_QUEUE_DIR_WAKE="$WORK/nq")
mkdir -p "$WORK/nq"
start n
export WEFTOS_STORE_TEST_RUSTFS_ENV="$WORK/a.env"
export WEFTOS_STORE_TEST_CLIENT_RUSTFS_ENVS="$WORK/a.env:$WORK/b.env"
cd "$REPO"
rc=0
run() { [ -n "${ONLY:-}" ] && [[ "$*" != *"$ONLY"* ]] && return 0; echo "== $*"; timeout "${SLICE_TIMEOUT:-1200}" "$@" || { echo "FAILED (or timed out after ${SLICE_TIMEOUT:-1200}s): $*" >&2; rc=1; }; }
# S1 x S4, S1 x S3 (weftos-release/tests/adr126_integ_live.rs)
run scripts/build.sh test weftos-release --filter live_s1_ --ignored
run scripts/build.sh test weftos-release --filter s1_s3_node_bucket_names
# S1 x S3: S3's chain-authenticated feed driven by S1's stamped grant events (forged, replayed, reordered)
run scripts/build.sh test weftos-release --filter live_s1_events_through_the_chain_authenticated_feed --ignored
# S1 x S2 (S2's own real-RustFS test)
run scripts/build.sh test weftos-store --features rustfs --filter two_machines_share_a_worktree_on_a_real_rustfs --ignored
# S1 x S2, S2's other live tests (land preview and caps, concurrent claims with one winner via put_if_match,
# a follower seeing unmerged worktrees across three identities). The notification wake-up test needs a
# RustFS notify target and scripts/store/notify-sink.py, so it is not part of this script.
run scripts/build.sh test weftos-store --features rustfs --filter live_land_preview --ignored
run scripts/build.sh test weftos-store --features rustfs --filter live_concurrent_claims --ignored
run scripts/build.sh test weftos-store --features rustfs --filter live_follower_sees_unmerged --ignored
# S1's own live suite (RustFS transport, grants, Object Lock, retention; on a branch that carries S1's
# one-creator change it also runs the one-creator provisioning proof). Slowest slice: 70 to 180 s.
run scripts/build.sh test weftos-store --features rustfs --filter tests::live_ --ignored
# S2 L1 follower relay: a bucket notification wakes the follower before its fallback poll
WEFTOS_STORE_TEST_RUSTFS_ENV="$WORK/n.env" WEFTOS_STORE_TEST_NOTIFY_FILE="$WORK/events.jsonl" \
    run scripts/build.sh test weftos-store --features rustfs --filter live_a_bucket_notification --ignored
# S5 x S2/S1
run scripts/build.sh test clawft-weave --filter a_promoted_worktree_stays_on_the_clients_instance --ignored
exit $rc
