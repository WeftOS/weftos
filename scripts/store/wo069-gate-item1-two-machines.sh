#!/usr/bin/env bash
# WO-069 lane F2, ADR-126 section 4a gate item 1: two machines, two principals, two clocks, against a THROWAWAY RustFS on the second host.
set -euo pipefail
export GATE_NAME=wo069-gate-item1
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-item1-two-machines.sh --follower=library|daemon [--second-host H] [--trials N] [--poll-interval-ms MS] [--dry-run]

Gate item 1: publish, poll, search, sync-docs, browse, fetch and claim with two principals on two machines; a follower sees a publish
within one poll interval. Run wo069-gate-remote-rustfs.sh up and mint-keys first (the store lives on the second host).

  --follower=library  NEEDS NO OWNER DECISION. The Mac runs the writer half (the weftos-store test s2_follower_half, writer_half, with
                      the writer key: it publishes a synthetic tree and refreshes the registry entry and digest as the daemon does)
                      and, for the daemon-path verbs, a writer and a follower daemon. The second host runs NO weaver and NO daemon: it
                      runs the same test binary built for Linux x86-64 in the CI container (follower_half, read-only key, inside the CI
                      image under rootless podman): poll (digest, then --full), older-writer verdict, search, sync-docs, browse, fetch,
                      and the eight refused writes, on ITS clock. It also runs the raw SigV4 probe (bash and curl, fed over ssh stdin) as
                      a cross-check. Not covered: a claim made BY the second machine (it needs a daemon there); recorded as NOT RUN.
  --follower=daemon   The second host runs a follower daemon (read-only key) and a merger daemon (writer-key copy) in rootless
                      podman, and does follow, search, sync-docs, browse, fetch and the claim itself. REFUSES unless
                      WO069_OWNER_GO_Q3=1 (owner question 3: a store-capable weaver on aepod-xpc).
  --second-host H     ssh target (default aepod@aepod-xpc). PG is refused.
  --trials N          publishes timed against the probe (default 10)
  --poll-interval-ms  the follower poll interval being tested (default 30000; the ADR says 30 to 60 s)
  --dry-run           print every local and remote command; run none, touch nothing

Environment: WO069_GATE_WRITER_ENV and WO069_GATE_FOLLOWER_ENV (0600 env files from mint-keys; required), WEFTOS_E2E_WEAVER (this
branch's weaver, default target/debug/weaver), WO069_GATE_REMOTE_DIR (default ~/ci-jobs/wo069-gate-rfs), WO069_GATE_PROJECT (a project
ULID; default a random one), WO069_GATE_FH_LINUX_BIN (library mode: the Linux x86-64 s2_follower_half binary built in CI, on the Mac; see
the runbook, item 1), WO069_GATE_FH_BIN (library mode: a prebuilt Mac s2_follower_half binary; default: built here with a targeted cargo
test --no-run), WO069_GATE_FH_IMAGE (the CI image the follower half runs in on the host), WO069_GATE_REMOTE_WEAVER and
WO069_GATE_REMOTE_SRC (daemon mode: the Linux weaver and the ci-push source dir on the host), KEEP=1 (keep the work dir). Results: lines RESULT|item1|step|PASS/FAIL|ms|detail (paste them
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
# The Mac daemons put unix sockets under $WORK/run-<role>/ (deepest: run-follower/child-ipc/child.sock, 34 characters), and macOS caps a
# socket path at 104 bytes: a long work dir makes `kernel start` fail with "path must be shorter than SUN_LEN". Keep the name short.
WROOT=${WO069_GATE_WORK_ROOT:-$GATE_REPO/target}
if [ "$GATE_DRY" = 1 ]; then WORK="$WROOT/wi1.DRYRUN"; else mkdir -p "$WROOT"; WORK=$(mktemp -d "$WROOT/wi1.XXXXXX"); fi
[ $(( ${#WORK} + 34 )) -lt 104 ] || { [ "$GATE_DRY" = 1 ] || rmdir "$WORK"; gate_die "work dir $WORK is too long for the daemons' unix sockets (macOS limit 104 bytes); set WO069_GATE_WORK_ROOT to a shorter absolute dir"; }
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
DPIDS=(); BGPIDS=(); FH_NAMES=()
gx() { # <role> cmd...: a throwaway principal on the Mac with its own HOME, runtime dir, state and agent label
    local role=$1; shift
    env HOME="$WORK/home-$role" WEFTOS_RUNTIME_DIR="$WORK/run-$role" WEFTOS_AGENT="$role" XDG_STATE_HOME="$WORK/xdg-$role" WTS_STATE_DIR="$WORK/xdg-$role/wts" \
        WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER="$WEAVER" WTS_PROJECT="$PROJECT" GIT_CONFIG_GLOBAL="$WORK/gitconfig" GIT_CONFIG_NOSYSTEM=1 AGF_TARGET_REF=master \
        GIT_CEILING_DIRECTORIES="$WORK" "$@"   # $WORK may sit inside a git checkout (target/ of this repo): never let git walk up into it
}
daemon_up() { # <role> <env file>: user daemon whose store key is that env file
    local role=$1 envf=$2 i
    mkdir -p "$WORK/home-$role/.weftos" "$WORK/run-$role"
    printf '[kernel.governance]\noutside_project = "allow_all"\n' >"$WORK/home-$role/.weftos/weave.toml"
    env HOME="$WORK/home-$role" WEFTOS_RUNTIME_DIR="$WORK/run-$role" WEFTOS_STORE_RUSTFS_ENV="$envf" "$WEAVER" kernel start --foreground --profile user >"$WORK/daemon-$role.out" 2>&1 &
    DPIDS+=($!)
    for i in $(seq 1 120); do gx "$role" "$WEAVER" store ls >/dev/null 2>&1 && return 0; kill -0 "${DPIDS[${#DPIDS[@]}-1]}" 2>/dev/null || break; sleep 0.5; done
    return 1
}
cleanup() {
    trap - EXIT INT TERM
    local p; for p in ${DPIDS[@]+"${DPIDS[@]}"}; do pkill -P "$p" 2>/dev/null || true; kill "$p" 2>/dev/null || true; done
    for p in ${BGPIDS[@]+"${BGPIDS[@]}"}; do kill "$p" 2>/dev/null || true; done   # the ssh probe and follower-half sessions this script started
    [ "$GATE_DRY" = 1 ] && return 0
    # a follower-half container outlives a killed ssh session: remove exactly the names this script used (they are --rm, so normally gone)
    [ -z "${FH_NAMES[*]:-}" ] || "${SSH[@]}" "$HOST" "podman rm -f ${FH_NAMES[*]} >/dev/null 2>&1; true" || true
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
# A grant is a backend policy (admin API). mint-keys gives the writer a scoped key with no admin, so the writer daemon must be REFUSED here;
# the follower's read-only key, not a daemon grant, is what keeps the follower from writing.
step "scoped writer key cannot change grants (admin API refused)" 'PN=$(pname); [ -n "$PN" ] && ! gx writer "$WEAVER" store grant "$PN" agent:follower read 2>"$WORK/grant.err" && grep -qiE "forbidden|accessdenied" "$WORK/grant.err"'

# --- 2: publish and the poll trials against the second host's clock
SLUG=feat-fh                                   # the worktree the library halves publish (the daemon verbs below use feat--gate)
HR=${RDIR/#\~/\$HOME}                          # the remote shell expands $HOME; a ~ inside a -v argument is not expanded
FHIMG=${WO069_GATE_FH_IMAGE:-localhost/tapestry/build:1.95-ci-wasm}
fh_follower_cmd() { # <expect head> <wait ms> <poll ms>: the follower half on the second host, in the CI image, read-only key, state kept in $RDIR
    # container named wo069-fh-follower-<head>, so cleanup removes exactly the containers this script started; callers add the name to
    # FH_NAMES themselves (this function runs in a $(...) subshell)
    printf 'podman run --rm --pull=never --name wo069-fh-follower-%s --network host --userns=keep-id -e WEFTOS_FH_FOLLOWER_ENV=/rfs/follower.env -e WEFTOS_FH_PROJECT_BUCKET=%s -e WEFTOS_FH_SLUG=%s -e WEFTOS_FH_STATE=/rfs/fh-state.json -e WEFTOS_FH_EXPECT_HEAD=%s -e WEFTOS_FH_WAIT_MS=%s -e WEFTOS_FH_POLL_MS=%s -v %s:/rfs -w /rfs %s /rfs/s2_follower_half follower_half --exact --ignored --nocapture --test-threads=1' \
        "$1" "$PB" "$SLUG" "$1" "$2" "$3" "$HR" "$FHIMG"
}
fh_writer() { # <round>: the writer half on the Mac with the writer key (round 1 also creates the two buckets)
    env WEFTOS_FH_WRITER_ENV="$WENV" WEFTOS_FH_PROJECT_BUCKET="$PB" WEFTOS_FH_SLUG="$SLUG" WEFTOS_FH_ROUND="$1" WEFTOS_FH_CREATE=1 "$MACBIN" writer_half --exact --ignored --nocapture --test-threads=1
}
fh_pub() { sed -n 's/^FH|writer|round=[0-9]*|published_ms=\([0-9]*\)|.*/\1/p' "$1" | tail -1; }   # <writer log>: the local clock (ms) the registry refresh finished at
PUBLISHED=()
if [ "$MODE" = library ]; then
    LBIN=${WO069_GATE_FH_LINUX_BIN:-}; MACBIN=${WO069_GATE_FH_BIN:-}
    if [ "$GATE_DRY" = 1 ]; then
        LBIN=${LBIN:-"<the Linux x86-64 s2_follower_half binary built in CI>"}
        [ -n "$MACBIN" ] || { gate_run cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run --message-format=json; MACBIN="<the Mac s2_follower_half binary>"; }
    else
        [ -s "$LBIN" ] || gate_die "set WO069_GATE_FH_LINUX_BIN to the Linux x86-64 s2_follower_half binary built in CI (runbook, item 1, 'Build the follower half')"
        if [ -z "$MACBIN" ]; then
            MACBIN=$(cd "$GATE_REPO" && cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run --message-format=json 2>"$WORK/build.err" \
                | jq -r 'select(.reason == "compiler-artifact" and .target.name == "s2_follower_half" and .executable != null) | .executable' | tail -1)
        fi
        [ -x "$MACBIN" ] || gate_die "no Mac s2_follower_half binary (see $WORK/build.err)"
    fi
    gate_note "copy the Linux binary to the host; the follower env is already there (mint-keys, 0600)"
    gate_run scp -o BatchMode=yes "$LBIN" "$HOST:${RDIR#\~/}/s2_follower_half"
    rx "chmod 755 $RDIR/s2_follower_half && rm -f $RDIR/fh-state.json $RDIR/tmp/probe.stop"
    gate_note "round 1 on the Mac: writer_half creates the project and worktree buckets and publishes head h1"
    if [ "$GATE_DRY" = 1 ]; then gate_run env WEFTOS_FH_WRITER_ENV="$WENV" WEFTOS_FH_PROJECT_BUCKET="$PB" WEFTOS_FH_SLUG="$SLUG" WEFTOS_FH_ROUND=1 WEFTOS_FH_CREATE=1 "$MACBIN" writer_half --exact --ignored --nocapture --test-threads=1
    else step "writer half round 1 on the Mac (buckets, publish h1, registry and digest)" 'fh_writer 1 | tee "$WORK/fh-w-1.log"'; fi
    gate_note "follower half, first look (no wait): poll (digest and --full), older-writer verdict, search, sync-docs, browse, fetch tree and bundle, the eight refused writes"
    if [ "$GATE_DRY" = 1 ]; then rx "$(fh_follower_cmd h1 0 2000)"
    else
        FH_NAMES+=("wo069-fh-follower-h1")
        "${SSH[@]}" "$HOST" "$(fh_follower_cmd h1 0 2000)" >"$WORK/fh-f-1.log" 2>&1 && res "follower-half-on-second-host" PASS 0 "$(grep -c '|PASS|' "$WORK/fh-f-1.log") FH lines PASS (poll, poll-full, older-writer, search, sync-docs, browse, fetch, refused-1..8); see $WORK/fh-f-1.log" \
            || { FAILS=$((FAILS + 1)); res "follower-half-on-second-host" FAIL 0 "$(grep -E 'panicked|FH\|' "$WORK/fh-f-1.log" | tail -3 | tr '\n' ' ')"; }
        WBH=$(sed -n 's/^.*FH|writer|names|project_bucket=[^|]*|worktree_bucket=\([^|]*\)|.*/\1/p' "$WORK/fh-w-1.log" | tail -1)
    fi
    gate_note "second host: { cat wo069-gate-lib.sh; echo gate_probe_poll ...; } | ssh $HOST bash -s   (raw cross-check: read-only key, poll every $IVAL ms, prints SEEN <remote epoch ms> <body sha256>)"
    if [ "$GATE_DRY" = 1 ]; then gate_run "${SSH[@]}" "$HOST" "bash -s" '<' "$GATE_REPO/scripts/store/wo069-gate-lib.sh"
    else
        # polls the digest; on each change it reads the registry entry for the head; stops at the stop file (touched after the trials) or the cap
        { cat "$GATE_REPO/scripts/store/wo069-gate-lib.sh"; printf 'gate_probe_poll %s/follower.env %s worktrees/_digest.json %s %s worktrees/%s.json %s/tmp/probe.stop\n' "$RDIR" "$PB" "$IVAL" "$(( (TRIALS + 2) * (2 * IVAL + 60000) ))" "$SLUG" "$RDIR"; } \
            | "${SSH[@]}" "$HOST" bash -s >"$WORK/seen.log" 2>"$WORK/seen.err" &
        PROBE=$!; BGPIDS+=("$PROBE")
        sleep 3   # the first SEEN line is the digest as it is now (head h1): the baseline
    fi
    # the trials: the follower half starts first on the host and polls every $IVAL ms on ITS clock; the writer half publishes at a different phase of that cycle each trial
    FLATS=""; FOK=0; FPOLLS=""
    for i in $(seq 2 $((TRIALS + 1))); do
        if [ "$GATE_DRY" = 1 ]; then
            gate_note "trial $i (head h$i): start the follower half on the host (below, in the background), sleep 2 s plus a phase of the poll cycle, run writer_half round $i on the Mac; latency = seen_ms - offset - published_ms"
            gate_run "${SSH[@]}" "$HOST" "$(fh_follower_cmd "h$i" $((IVAL + 5000)) "$IVAL")"
            gate_run env WEFTOS_FH_WRITER_ENV="$WENV" WEFTOS_FH_PROJECT_BUCKET="$PB" WEFTOS_FH_SLUG="$SLUG" WEFTOS_FH_ROUND="$i" "$MACBIN" writer_half --exact --ignored --nocapture --test-threads=1
            break
        fi
        FCMD=$(fh_follower_cmd "h$i" $((IVAL + 5000)) "$IVAL"); FH_NAMES+=("wo069-fh-follower-h$i")
        "${SSH[@]}" "$HOST" "$FCMD" >"$WORK/fh-f-$i.log" 2>&1 & FP=$!; BGPIDS+=("$FP")
        sleep "$(awk "BEGIN{print 2 + ($i * 0.37 * $IVAL / 1000) % ($IVAL / 1000)}")"
        fh_writer "$i" >"$WORK/fh-w-$i.log" 2>&1 || { FAILS=$((FAILS + 1)); res "writer-half-round-$i" FAIL 0 "$(tail -2 "$WORK/fh-w-$i.log" | tr '\n' ' ')"; }
        PUBLISHED+=("$(fh_pub "$WORK/fh-w-$i.log")"); LASTPUB=${PUBLISHED[${#PUBLISHED[@]}-1]}
        wait "$FP" || { FAILS=$((FAILS + 1)); res "follower-half-round-$i" FAIL 0 "$(grep -E 'panicked|expected head' "$WORK/fh-f-$i.log" | tail -2 | tr '\n' ' ')"; }
        seen=$(sed -n 's/^FH|follower|poll|seen_ms=\([0-9]*\)|.*/\1/p' "$WORK/fh-f-$i.log" | tail -1)
        if [ -n "$seen" ] && [ -n "$LASTPUB" ]; then
            l=$(( seen - OFF - LASTPUB )); FLATS="$FLATS $l"; [ "$l" -le "$((IVAL + 2000))" ] && FOK=$((FOK + 1))
            FPOLLS="$FPOLLS $(sed -n 's/^FH|follower|poll|PASS|[0-9]* ms|digest path: \([0-9]*\) poll.*/\1/p' "$WORK/fh-f-$i.log" | tail -1)"
        else FLATS="$FLATS MISSED"; fi
    done
    if [ "$GATE_DRY" = 0 ]; then
        if [ "$FOK" = "$TRIALS" ]; then res "publish-seen-within-one-poll" PASS 0 "$FOK/$TRIALS within interval+2s on the second host's clock; latencies ms (offset ${OFF} +- $((RTT / 2))):$FLATS; polls per trial:$FPOLLS"
        else FAILS=$((FAILS + 1)); res "publish-seen-within-one-poll" FAIL 0 "$FOK/$TRIALS; latencies ms:$FLATS"; fi
        read -r OFF2 RTT2 < <(gate_clock_offset "$HOST"); res clocks-after PASS 0 "remote minus local ${OFF2} ms after the trials (before: ${OFF} ms); drift $(( OFF2 - OFF )) ms, rtt ${RTT2} ms"
        sleep "$(awk "BEGIN{print $IVAL/1000 + 3}")"; rx "touch $RDIR/tmp/probe.stop"; wait "$PROBE" || true   # one more probe poll, then stop it
        # Match by head, not by order: the probe polls on its own phase, so two publishes inside one of its intervals show as one change
        # (the earlier head is superseded before the probe looks: not observable by a poller, and not a miss).
        ok=0; sup=0; lats=""; n=${#PUBLISHED[@]}; k=0
        while [ "$k" -lt "$n" ]; do
            h="h$((k + 2))"; s=$(awk -v h="$h" '$1 == "SEEN" && $4 == h {print $2; exit}' "$WORK/seen.log"); nx=${PUBLISHED[k + 1]:-}
            if [ -n "$s" ] && [ -n "${PUBLISHED[k]}" ]; then l=$(( s - OFF - PUBLISHED[k] )); lats="$lats $h:$l"; [ "$l" -le "$((IVAL + 2000))" ] && ok=$((ok + 1))
            elif [ -n "$nx" ] && [ -n "${PUBLISHED[k]}" ] && [ $(( nx - PUBLISHED[k] )) -lt "$((IVAL + 2000))" ]; then sup=$((sup + 1)); lats="$lats $h:superseded-after-$(( nx - PUBLISHED[k] ))ms"
            else lats="$lats $h:MISSED"; fi
            k=$((k + 1))
        done
        if [ $((ok + sup)) = "$TRIALS" ] && [ "$ok" -gt 0 ]; then res "raw-probe-cross-check" PASS 0 "$ok seen within interval+2s, $sup superseded by the next publish inside one interval; latencies ms (raw digest GET, head from the entry, probe clock corrected by offset):$lats"
        else FAILS=$((FAILS + 1)); res "raw-probe-cross-check" FAIL 0 "$ok seen in time, $sup superseded, of $TRIALS; latencies ms:$lats"; fi
        { cat "$GATE_REPO/scripts/store/wo069-gate-lib.sh"; printf 'gate_probe_refuse %s/follower.env %s %s\n' "$RDIR" "$PB" "$WBH"; } | "${SSH[@]}" "$HOST" bash -s >"$WORK/refuse.log" 2>&1 \
            && res "second-host-eight-writes-refused" PASS 0 "$(tail -1 "$WORK/refuse.log")" || { FAILS=$((FAILS + 1)); res "second-host-eight-writes-refused" FAIL 0 "$(tail -2 "$WORK/refuse.log" | tr '\n' ' ')"; }
    else
        gate_note "after the trials: re-measure the clock offset (drift), then on the host gate_probe_refuse (raw eight writes, all 403; the library follower half already refused its own eight)"
    fi
    # the daemon-path verbs below need one real sweep of feat--gate
    step "writer: edit, commit, sweep feat--gate (for the daemon-path follower verbs)" 'printf "v%s\n" "$TRIALS" >>"$WORK/wt/docs/gate.md"; git -C "$WORK/wt" add -A; git -C "$WORK/wt" commit -qm "edit $TRIALS"; ( cd "$WORK/wt" && gx writer "$S" sweep --only feat--gate )'
else
    gate_note "second host (daemon mode): podman run follower and merger containers from \$WO069_GATE_REMOTE_WEAVER; see the runbook, item 1, daemon mode"
    rx "podman run -d --name wo069-gate-fnode --pull=never --network host --userns=keep-id -e HOME=/node/home -e WEFTOS_RUNTIME_DIR=/node/run -e WEFTOS_AGENT=follower -e WEFTOS_STORE_RUSTFS_ENV=/node/follower.env -e WTS_WEAVER=/rfs/weaver -e WTS_PROJECT=$PROJECT -v $RDIR/node:/node -v $RDIR/follower.env:/node/follower.env:ro -v \$WO069_GATE_REMOTE_WEAVER:/rfs/weaver:ro -v \$WO069_GATE_REMOTE_SRC:/src:ro -w /src localhost/tapestry/build:1.95-ci-wasm sleep infinity"
    rx "podman exec -d wo069-gate-fnode /rfs/weaver kernel start --foreground --profile user"
    for i in $(seq 1 "$TRIALS"); do
        if [ "$GATE_DRY" = 1 ]; then gate_note "trial $i: edit a doc, commit, gx writer sweep --only feat--gate, record T_i (local clock) when sweep returns; wait $((IVAL + 5000)) ms"; continue; fi
        printf 'v%s\n' "$i" >>"$WORK/wt/docs/gate.md"; git -C "$WORK/wt" add -A; git -C "$WORK/wt" commit -qm "edit $i"
        ( cd "$WORK/wt" && gx writer "$S" sweep --only feat--gate ) >/dev/null 2>&1
        PUBLISHED+=("$(gate_now_ms)"); sleep "$(awk "BEGIN{print ($IVAL + 5000)/1000}")"
    done
fi

# --- 3: the follower verbs under the read-only key
step "follower follow (first poll sees the worktree)" 'gx follower "$S" follow | grep -q feat--gate'
step "follower search" 'gx follower "$S" search Gate | grep -q docs/gate.md'
step "follower sync-docs" 'gx follower "$S" sync-docs feat--gate "$WORK/cache" && grep -rq "v$TRIALS" "$WORK/cache"'
step "follower browse (list, ls docs/, get)" 'gx follower "$S" browse --json && gx follower "$S" browse feat--gate docs/ --json && gx follower "$S" browse feat--gate docs/gate.md | grep -q "v$TRIALS"'
step "follower fetch tree" 'gx follower "$S" fetch feat--gate "$WORK/fetched" && grep -q "v$TRIALS" "$WORK/fetched/docs/gate.md"'
step "follower fetch into a clone (tree + bundle commits under refs/shared/)" 'git clone -q "$WORK/repo" "$WORK/fclone" && gx follower "$S" fetch feat--gate "$WORK/fclone" && git -C "$WORK/fclone" for-each-ref refs/shared/feat--gate | grep -q . && grep -q "v$TRIALS" "$WORK/fclone/docs/gate.md"'
step "follower governed put refused (project store, registry key, worktree store)" '! gx follower "$WEAVER" store put "$PN" d1/nope.txt no --as agent:follower && ! gx follower "$WEAVER" store put "$PN" worktrees/forged.json "{}" --as agent:follower'
if [ "$MODE" = library ]; then
    res "claim-by-second-machine" NOTRUN 0 "needs a daemon on the second host (--follower=daemon, owner question 3); the claim below is the Mac's"
    step "merger daemon up on the Mac (a copy of the writer key, agent merger)" 'daemon_up merger "$WENV"'
    step "claim and fetch on the Mac as merger (not the second machine)" '( cd "$WORK/wt" && gx writer "$S" share --handoff "gate handoff" ) && gx merger "$S" claim feat--gate'
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
