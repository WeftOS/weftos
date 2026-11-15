#!/usr/bin/env bash
# WO-069 lane H: the ADR-126 section 4a gate drills that need no owner input, against a THROWAWAY RustFS only.
#
#   WEFTOS_STORE_TEST_RUSTFS_BIN=/path/to/rustfs [WEFTOS_E2E_WEAVER=target/debug/weaver] \
#     scripts/store/wo069-drills.sh [--only d1,d2,d3,lib,fh,scale] [--scale] [--release]
#
#   d1     two local principals through a real weaver + throwaway daemon (gate item 1, one machine)
#   d2     leftover drill and handoff drill (gate item 4)
#   d3     daemon killed during a publish / registry refresh, restarted (gate item 5c)
#   lib    the library drills on the real RustFS: 1a (two real S3 identities), 2l, 3a, 3b, 3c (s2_drills_live.rs)
#   fh     gate item 1 follower half: the writer half and the follower half (s2_follower_half.rs, both_halves_on_one_machine) with two scoped
#          keys on the throwaway server; needs no weaver. Never in the default run.
#   scale  drill 4, synthetic client-scale trees (s2_scale_bench.rs): 1k and 10k files; --scale adds 20k and 50k
#
# The default run is d1,d2,d3,lib (a few minutes). --scale is slower (the 50k tree writes about 1.5 GB and takes ~17 min to publish even on an internal disk) and is never in the default.
# WEFTOS_DRILLS_DATA_DIR=<dir on the internal disk> keeps the throwaway server's data there (a subdirectory, removed on exit) instead of under target/.
# --release builds the library tests in release mode (use it for the latency numbers; the weaver stays the debug build).
#
# Two machines (drill 1 with the RustFS on another host): do NOT pass a rustfs binary. Put the THROWAWAY server's endpoint and a
# root key in a 0600 env file on the machine that runs this script, with
#   MESH_S3_ENDPOINT=http://<host>:<port>
#   MESH_S3_ACCESS_KEY=...   MESH_S3_SECRET_KEY=...
# and run   WEFTOS_DRILLS_RUSTFS_ENV=/path/to/that.env scripts/store/wo069-drills.sh --only d1,lib
# The harness then starts no RustFS of its own, points the throwaway daemon and the library tests at that endpoint, and refuses
# the PG address. Pass credentials by file path only; never on a command line or in chat. The server must be disposable: the
# library drills create and delete their own buckets and keys, but d1/d2 leave whatever the daemon created there.
#
# Never point this at PG or any shared RustFS. A local run starts its own server on 127.0.0.1 (random port and credentials) and
# its own daemon with an isolated HOME and runtime dir under target/, kills only those, and removes its data on exit (trap).
set -uo pipefail
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
ONLY="d1,d2,d3,lib"; SCALE=""; REL=""
while [ $# -gt 0 ]; do
    case "$1" in
        --only) ONLY="$2"; shift 2 ;;
        --scale) SCALE=1; ONLY="${ONLY},scale"; shift ;;
        --release) REL="--release"; shift ;;
        -h|--help) sed -n 2,28p "${BASH_SOURCE[0]}"; exit 0 ;;
        *) echo "unknown argument $1" >&2; exit 2 ;;
    esac
done
want() { case ",$ONLY," in *",$1,"*) return 0 ;; esac; return 1; }
EXT_ENV="${WEFTOS_DRILLS_RUSTFS_ENV:-}"
RUSTFS_BIN="${WEFTOS_STORE_TEST_RUSTFS_BIN:-}"
WEAVER="${WEFTOS_E2E_WEAVER:-$REPO/target/debug/weaver}"
if [ -z "$EXT_ENV" ]; then [ -x "$RUSTFS_BIN" ] || { echo "set WEFTOS_STORE_TEST_RUSTFS_BIN to the rustfs binary (or WEFTOS_DRILLS_RUSTFS_ENV for a RustFS on another host)" >&2; exit 1; }; fi
if want d1 || want d2 || want d3; then [ -x "$WEAVER" ] || { echo "no weaver at $WEAVER (scripts/build.sh native-debug)" >&2; exit 1; }; fi
mkdir -p "$REPO/target"
WORK="$(mktemp -d "$REPO/target/wo069-drills.XXXXXX")"
# shellcheck source=wo069-e2e-lib.sh
. "$REPO/scripts/store/wo069-e2e-lib.sh"
# shellcheck source=wo069-drills-lib.sh
. "$REPO/scripts/store/wo069-drills-lib.sh"
trap cleanup_all EXIT INT TERM
S="$REPO/scripts/worktree-store.sh"
ULID_CHARS=0123456789ABCDEFGHJKMNPQRSTVWXYZ
PROJECT="01$(python3 -c "import random; print(''.join(random.choice('$ULID_CHARS') for _ in range(24)))")"
printf '[user]\n\tname = drill\n\temail = drill@example.invalid\n[init]\n\tdefaultBranch = master\n' >"$WORK/gitconfig"
export GIT_CONFIG_GLOBAL="$WORK/gitconfig" GIT_CONFIG_NOSYSTEM=1
G() { git -C "$1" "${@:2}"; }
jqs() { jq -r "$1" "$WORK/last.out"; }
: >"$WORK/findings.txt"
finding() { grep -q "^FINDING	${1%% *} " "$WORK/findings.txt" && return 0; printf 'FINDING\t%s\n' "$*" | tee -a "$WORK/findings.txt"; }   # once per finding id

echo "== host: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 )) GiB, load $(uptime | sed 's/.*averages*: //'), work dir $WORK on $(df -h "$WORK" | awk 'NR==2{print $1}')"
if [ -n "$EXT_ENV" ]; then
    use_external_rustfs "$EXT_ENV" || exit 1
else
    check "throwaway rustfs up on loopback" 'rustfs_start'
    if [ "$FAILS" != 0 ]; then
        echo "rustfs did not come up; its log:"; sed 's/^/    | /' "$WORK/rustfs.log" | head -20
        [ -z "${E2E_LOG_DIR:-}" ] || cp "$WORK/rustfs.log" "$E2E_LOG_DIR/drills-rustfs-start-failed.log" 2>/dev/null
        exit 1
    fi
fi
echo "== rustfs data dir: ${DRILL_DATA:-on the external host}"
export WEFTOS_STORE_TEST_RUSTFS_ENV="$WORK/live.env"

if want d1 || want d2 || want d3; then
    check "throwaway user daemon up (isolated runtime dir, store on that rustfs)" 'daemon_start'
    setup_project
fi
want d1 && drill1
want d2 && drill2
want d3 && drill3
if want lib; then lib_drills; fi
if want fh; then fh_drill; fi
if want scale; then scale_drills; fi

echo "== summary: $(grep -c '^PASS' "$WORK/steps.tsv" 2>/dev/null || echo 0) passed, $FAILS failed; $(grep -c '^FINDING' "$WORK/findings.txt") finding(s)"
[ -z "${E2E_LOG_DIR:-}" ] || { mkdir -p "$E2E_LOG_DIR"; cp "$WORK/steps.tsv" "$E2E_LOG_DIR/drills-steps.tsv" 2>/dev/null; cp "$WORK/findings.txt" "$E2E_LOG_DIR/drills-findings.txt" 2>/dev/null; }
exit $((FAILS > 0))
