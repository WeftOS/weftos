#!/usr/bin/env bash
# WO-069 lane F2, ADR-126 section 4a gate item 1: two machines, two principals, two clocks, against a THROWAWAY RustFS on the second host.
set -euo pipefail
export GATE_NAME=wo069-gate-item1
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-item1-two-machines.sh --follower=library|daemon [--second-host H] [--trials N] [--poll-interval-ms MS] [--dry-run]

Gate item 1: publish, poll, search, sync-docs, browse, fetch and claim with two principals on two machines; a follower sees a publish
within one poll interval. Run wo069-gate-remote-rustfs.sh up and mint-keys first (the store lives on the second host).

  --follower=library  NEEDS NO OWNER DECISION. The Mac runs the writer daemon (writer key) and a follower daemon (read-only key).
                      The second host runs no weaver and no daemon: it runs a raw SigV4 probe (bash and curl only, fed over ssh
                      stdin, read-only key) that polls the registry digest every poll interval on ITS clock, so the second
                      clock sees every publish, and that attempts the eight write operations (all must be 403). Optionally,
                      WO069_GATE_TEST_TARGET=<weftos-store test target> builds and runs a follower-half library test on the host in
                      the CI container (see the runbook, item 1: that test file does not exist yet). Not covered: a claim made BY
                      the second machine (it needs a daemon there); the claim is made on the Mac and recorded as NOT RUN here.
  --follower=daemon   The second host runs a follower daemon (read-only key) and a merger daemon (writer-key copy) in rootless
                      podman, and does follow, search, sync-docs, browse, fetch and the claim itself. REFUSES unless
                      WO069_OWNER_GO_Q3=1 (owner question 3: a store-capable weaver on aepod-xpc).
  --second-host H     ssh target (default aepod@aepod-xpc). PG is refused.
  --trials N          publishes timed against the probe (default 10)
  --poll-interval-ms  the follower poll interval being tested (default 30000; the ADR says 30 to 60 s)
  --dry-run           print every local and remote command; run none, touch nothing

Environment: WO069_GATE_WRITER_ENV and WO069_GATE_FOLLOWER_ENV (0600 env files from mint-keys; required), WEFTOS_E2E_WEAVER (this
branch's weaver, default target/debug/weaver), WO069_GATE_REMOTE_DIR (default ~/ci-jobs/wo069-gate-rfs), WO069_GATE_PROJECT (a project
ULID; default a random one), WO069_GATE_TEST_TARGET, WO069_GATE_REMOTE_WEAVER and WO069_GATE_REMOTE_SRC (daemon mode: the Linux weaver
and the ci-push source dir on the host), KEEP=1 (keep the work dir). Results: lines RESULT|item1|step|PASS/FAIL|ms|detail (paste them
into the runbook's table). Credentials are used only by file path and are never printed.
EOF
}
MODE=""; HOST="aepod@aepod-xpc"; TRIALS=10; IVAL=30000
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        --follower=library|--follower=daemon) MODE=${1#--follower=}; shift ;;
        --second-host) HOST=$2; shift 2 ;;
        --trials) TRIALS=$2; shift 2 ;;
        --poll-interval-ms) IVAL=$2; shift 2 ;;
        *) gate_die "unknown option $1 (see --help)" ;;
    esac
done
[ -n "$MODE" ] || { usage >&2; exit 2; }
gate_refuse_pg "second host" "$HOST"
[ "$MODE" != daemon ] || gate_require_go_q3 "--follower=daemon"
WENV=${WO069_GATE_WRITER_ENV:-}; FENV=${WO069_GATE_FOLLOWER_ENV:-}
[ -n "$WENV" ] && [ -n "$FENV" ] || gate_die "set WO069_GATE_WRITER_ENV and WO069_GATE_FOLLOWER_ENV (0600 env files from wo069-gate-remote-rustfs.sh mint-keys)"
for f in "$WENV" "$FENV"; do gate_secret_ok "$f"; gate_refuse_pg_env "$f"; done
RDIR=${WO069_GATE_REMOTE_DIR:-'~/ci-jobs/wo069-gate-rfs'}
WEAVER=${WEFTOS_E2E_WEAVER:-$GATE_REPO/target/debug/weaver}; S="$GATE_REPO/scripts/worktree-store.sh"
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10)
rx() { gate_run "${SSH[@]}" "$HOST" "$1"; }
ULID_CHARS=0123456789ABCDEFGHJKMNPQRSTVWXYZ
PROJECT=${WO069_GATE_PROJECT:-01$(python3 -c "import random; print(''.join(random.choice('$ULID_CHARS') for _ in range(24)))")}
PB="p-$(printf '%s' "$PROJECT" | tr 'A-Z' 'a-z')"
if [ "$GATE_DRY" = 1 ]; then WORK="$GATE_REPO/target/wo069-gate-item1.DRYRUN"; else mkdir -p "$GATE_REPO/target"; WORK=$(mktemp -d "$GATE_REPO/target/wo069-gate-item1.XXXXXX"); fi
GATE_TMP="$WORK/tmp"; RES="$WORK/results.tsv"
res() { # <step> <PASS|FAIL|NOTRUN> <ms> <detail>
    printf 'RESULT|item1|%s|%s|%s|%s\n' "$1" "$2" "$3" "$4" | { if [ "$GATE_DRY" = 1 ]; then cat; else tee -a "$RES"; fi; }
}
FAILS=0
step() { # <name> <cmd string>: timed pass/fail of a local command (dry-run: print only)
    if [ "$GATE_DRY" = 1 ]; then printf 'DRY: step %s: %s\n' "$1" "$2"; return 0; fi
    local t0 t1 rc=0; t0=$(gate_now_ms); eval "$2" >"$WORK/last.out" 2>"$WORK/last.err" || rc=$?; t1=$(gate_now_ms)
    if [ $rc = 0 ]; then res "$1" PASS "$((t1 - t0))" ""; else FAILS=$((FAILS + 1)); res "$1" FAIL "$((t1 - t0))" "rc=$rc $(head -c 160 "$WORK/last.err" | tr '\n' ' ')"; fi
}
DPIDS=()
gx() { # <role> cmd...: a throwaway principal on the Mac with its own HOME, runtime dir, state and agent label
    local role=$1; shift
    env HOME="$WORK/home-$role" WEFTOS_RUNTIME_DIR="$WORK/run-$role" WEFTOS_AGENT="$role" XDG_STATE_HOME="$WORK/xdg-$role" WTS_STATE_DIR="$WORK/xdg-$role/wts" \
        WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER="$WEAVER" WTS_PROJECT="$PROJECT" GIT_CONFIG_GLOBAL="$WORK/gitconfig" GIT_CONFIG_NOSYSTEM=1 AGF_TARGET_REF=master "$@"
}
daemon_up() { # <role> <env file>: user daemon whose store key is that env file
    local role=$1 envf=$2 i
    mkdir -p "$WORK/home-$role/.weftos" "$WORK/run-$role"
    printf '[kernel.governance]\noutside_project = "allow_all"\n' >"$WORK/home-$role/.weftos/weave.toml"
    env HOME="$WORK/home-$role" WEFTOS_RUNTIME_DIR="$WORK/run-$role" WEFTOS_STORE_RUSTFS_ENV="$envf" "$WEAVER" kernel start --foreground --profile user >"$WORK/daemon-$role.out" 2>&1 &
    DPIDS+=($!)
    for i in $(seq 1 120); do gx "$role" "$WEAVER" store ls >/dev/null 2>&1 && return 0; kill -0 "${DPIDS[-1]}" 2>/dev/null || break; sleep 0.5; done
    return 1
}
cleanup() {
    trap - EXIT INT TERM
    local p; for p in ${DPIDS[@]+"${DPIDS[@]}"}; do pkill -P "$p" 2>/dev/null || true; kill "$p" 2>/dev/null || true; done
    [ "$GATE_DRY" = 1 ] && return 0
    if [ -n "${KEEP:-}" ]; then echo "KEEP set: $WORK left in place"; else rm -rf "$WORK"; echo "work dir removed"; fi
    echo "REMINDER: the second host still runs the throwaway RustFS. Finish with: wo069-gate-remote-rustfs.sh down $HOST"
}
trap cleanup EXIT INT TERM

gate_say "== item 1, follower=$MODE, second host $HOST, project $PROJECT, $TRIALS trials at a $IVAL ms poll interval"
[ "$GATE_DRY" = 1 ] || { mkdir -p "$GATE_TMP"; chmod 700 "$GATE_TMP"; : >"$RES"; printf '[user]\n\tname = gate\n\temail = gate@example.invalid\n[init]\n\tdefaultBranch = master\n' >"$WORK/gitconfig"; }
[ "$GATE_DRY" = 1 ] || [ -x "$WEAVER" ] || gate_die "no weaver at $WEAVER (scripts/build.sh native-debug, or set WEFTOS_E2E_WEAVER)"

# --- 0: the two clocks
gate_note "clock offset: ssh $HOST 'date +%s%3N' against the local clock, before and after (rtt/2 accuracy)"
if [ "$GATE_DRY" = 1 ]; then gate_clock_offset "$HOST"; OFF=0; else read -r OFF RTT < <(gate_clock_offset "$HOST"); res clocks PASS 0 "remote minus local ${OFF} ms, rtt ${RTT} ms (the latencies below subtract this)"; fi

# --- 1: writer machine (Mac): writer daemon, follower daemon, project, one worktree
step "writer daemon up (writer key)" 'daemon_up writer "$WENV"'
step "follower daemon up (read-only key)" 'daemon_up follower "$FENV"'
step "git repo and a worktree store (store-create) as writer" 'git init -q -b master "$WORK/repo" && echo base >"$WORK/repo/README.md" && git -C "$WORK/repo" add . && git -C "$WORK/repo" commit -qm base \
   && git -C "$WORK/repo" worktree add -q -b feat/gate "$WORK/wt" master && mkdir -p "$WORK/wt/docs" && printf "# Gate One\n\nbody\n" >"$WORK/wt/docs/gate.md" \
   && cd "$WORK/wt" && gx writer "$S" store-create && gx writer "$S" register'
pname() { gx writer "$WEAVER" store ls --json 2>/dev/null | grep -o "weftos://[^\"' ]*projects/$PROJECT/store" | head -1; }
step "grants: writer write, follower read, merger write on the project store" 'PN=$(pname); [ -n "$PN" ] && gx writer "$WEAVER" store grant "$PN" agent:writer write \
   && gx writer "$WEAVER" store grant "$PN" agent:follower read && gx writer "$WEAVER" store grant "$PN" agent:merger write'

# --- 2: publish and the poll trials against the second host's clock
if [ "$MODE" = library ]; then
    gate_note "second host: { cat wo069-gate-lib.sh; echo gate_probe_poll ...; } | ssh $HOST bash -s   (read-only key, poll every $IVAL ms, prints SEEN <remote epoch ms> <body sha256>)"
    if [ "$GATE_DRY" = 1 ]; then gate_run "${SSH[@]}" "$HOST" "bash -s" '<' "$GATE_REPO/scripts/store/wo069-gate-lib.sh"
    else
        { cat "$GATE_REPO/scripts/store/wo069-gate-lib.sh"; printf 'gate_probe_poll %s/follower.env %s worktrees/_digest.json %s %s\n' "$RDIR" "$PB" "$IVAL" "$(( (TRIALS + 2) * (IVAL + 5000) ))"; } \
            | "${SSH[@]}" "$HOST" bash -s >"$WORK/seen.log" 2>"$WORK/seen.err" &
        PROBE=$!
        sleep 3   # the first SEEN line is the digest as it is now (or an empty body): the baseline
    fi
else
    gate_note "second host (daemon mode): podman run follower and merger containers from \$WO069_GATE_REMOTE_WEAVER; see the runbook, item 1, daemon mode"
    rx "podman run -d --name wo069-gate-fnode --pull=never --network host --userns=keep-id -e HOME=/node/home -e WEFTOS_RUNTIME_DIR=/node/run -e WEFTOS_AGENT=follower -e WEFTOS_STORE_RUSTFS_ENV=/node/follower.env -e WTS_WEAVER=/rfs/weaver -e WTS_PROJECT=$PROJECT -v $RDIR/node:/node -v $RDIR/follower.env:/node/follower.env:ro -v \$WO069_GATE_REMOTE_WEAVER:/rfs/weaver:ro -v \$WO069_GATE_REMOTE_SRC:/src:ro -w /src localhost/tapestry/build:1.95-ci-wasm sleep infinity"
    rx "podman exec -d wo069-gate-fnode /rfs/weaver kernel start --foreground --profile user"
fi
PUBLISHED=()
for i in $(seq 1 "$TRIALS"); do
    if [ "$GATE_DRY" = 1 ]; then gate_note "trial $i: edit a doc, commit, gx writer sweep --only feat--gate, record T_i (local clock) when sweep returns; wait $((IVAL + 5000)) ms"; continue; fi
    printf 'v%s\n' "$i" >>"$WORK/wt/docs/gate.md"; git -C "$WORK/wt" add -A; git -C "$WORK/wt" commit -qm "edit $i"
    ( cd "$WORK/wt" && gx writer "$S" sweep --only feat--gate ) >/dev/null 2>&1
    PUBLISHED+=("$(gate_now_ms)"); sleep "$(awk "BEGIN{print ($IVAL + 5000)/1000}")"
done
if [ "$MODE" = library ] && [ "$GATE_DRY" = 0 ]; then
    wait "$PROBE" || true
    # SEEN lines after the baseline map one to one onto the publishes (the first publish also creates the worktree entry)
    mapfile -t SEEN < <(awk '$1 == "SEEN" {print $2}' "$WORK/seen.log" | tail -n +2)
    ok=0; lats=""
    for i in "${!PUBLISHED[@]}"; do
        if [ -n "${SEEN[$i]:-}" ]; then l=$(( SEEN[i] - OFF - PUBLISHED[i] )); lats="$lats $l"; [ "$l" -le "$((IVAL + 2000))" ] && ok=$((ok + 1)); else lats="$lats MISSED"; fi
    done
    if [ "$ok" = "$TRIALS" ]; then res "publish-seen-within-one-poll" PASS 0 "$ok/$TRIALS within interval+2s; latencies ms (probe clock corrected by offset):$lats"
    else FAILS=$((FAILS + 1)); res "publish-seen-within-one-poll" FAIL 0 "$ok/$TRIALS; latencies ms:$lats"; fi
    WB=$(gx writer "$WEAVER" store get "$PN" worktrees/feat--gate.json --as agent:writer | jq -r .bucket)
    { cat "$GATE_REPO/scripts/store/wo069-gate-lib.sh"; printf 'gate_probe_refuse %s/follower.env %s %s\n' "$RDIR" "$PB" "$WB"; } | "${SSH[@]}" "$HOST" bash -s >"$WORK/refuse.log" 2>&1 \
        && res "second-host-eight-writes-refused" PASS 0 "$(tail -1 "$WORK/refuse.log")" || { FAILS=$((FAILS + 1)); res "second-host-eight-writes-refused" FAIL 0 "$(tail -2 "$WORK/refuse.log" | tr '\n' ' ')"; }
else
    gate_note "library mode: after the trials run on the second host: gate_probe_refuse (eight writes, all 403)"
fi

# --- 3: the follower verbs under the read-only key
step "follower follow (first poll sees the worktree)" 'gx follower "$S" follow | grep -q feat--gate'
step "follower search" 'gx follower "$S" search Gate | grep -q docs/gate.md'
step "follower sync-docs" 'gx follower "$S" sync-docs feat--gate "$WORK/cache" && grep -rq "v$TRIALS" "$WORK/cache"'
step "follower browse (list, ls docs/, get)" 'gx follower "$S" browse --json && gx follower "$S" browse feat--gate docs/ --json && gx follower "$S" browse feat--gate docs/gate.md | grep -q "v$TRIALS"'
step "follower fetch tree" 'gx follower "$S" fetch feat--gate "$WORK/fetched" && grep -q "v$TRIALS" "$WORK/fetched/docs/gate.md"'
step "follower governed put refused (project store, registry key, worktree store)" '! gx follower "$WEAVER" store put "$PN" d1/nope.txt no --as agent:follower && ! gx follower "$WEAVER" store put "$PN" worktrees/forged.json "{}" --as agent:follower'
if [ "$MODE" = library ]; then
    res "claim-by-second-machine" NOTRUN 0 "needs a daemon on the second host (--follower=daemon, owner question 3); the claim below is the Mac's"
    step "claim and fetch on the Mac as merger (not the second machine)" '( cd "$WORK/wt" && gx writer "$S" share --handoff "gate handoff" ) && gx merger "$S" claim feat--gate'
    if [ -n "${WO069_GATE_TEST_TARGET:-}" ]; then
        gate_note "library follower test on the host: ci-push, ci-run (see runbook item 1) with WEFTOS_STORE_TEST_RUSTFS_ENV=\$RDIR/follower.env"
    fi
else
    rx "podman exec wo069-gate-fnode bash /src/scripts/worktree-store.sh follow"
    rx "podman exec wo069-gate-fnode bash /src/scripts/worktree-store.sh search Gate"
    rx "podman exec wo069-gate-fnode bash /src/scripts/worktree-store.sh sync-docs feat--gate /node/cache"
    rx "podman exec wo069-gate-fnode bash /src/scripts/worktree-store.sh browse feat--gate docs/gate.md"
    rx "podman exec wo069-gate-fnode bash /src/scripts/worktree-store.sh fetch feat--gate /node/fetched"
    gate_note "writer: share --handoff; then on the host a merger daemon (writer-key copy, WEFTOS_AGENT=merger) runs: worktree-store.sh claim feat--gate (the second machine's claim)"
    rx "podman rm -f wo069-gate-fnode wo069-gate-fmerger; rm -rf $RDIR/node; ! podman container exists wo069-gate-fnode && ! test -e $RDIR/node"
fi
[ "$GATE_DRY" = 1 ] && { gate_say "dry run complete: nothing was run, nothing was written."; exit 0; }
gate_say "== item 1 finished: $FAILS failed step(s); results in $RES (copied below)"; cat "$RES"
exit $((FAILS > 0))
