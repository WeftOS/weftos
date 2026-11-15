#!/usr/bin/env bash
# WO-069 lane F2, ADR-126 section 4a gate item 4: a leftover and a handoff on REAL clocks, resumable over days.
set -euo pipefail
export GATE_NAME=wo069-gate-item4
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-item4-leftover.sh start|check|finish [--smoke] [--merger=local|second-host] [--second-host H] [--dry-run]
       WO069_GATE_STATE=<dir> WEFTOS_STORE_TEST_RUSTFS_BIN=<rustfs> [WEFTOS_E2E_WEAVER=<weaver>] wo069-gate-item4-leftover.sh ...

The drill is time. `start` publishes four worktrees on a throwaway local RustFS and daemon, then STOPS EVERYTHING and leaves the state
on disk under $WO069_GATE_STATE (a mode 0700 dir on the notes volume, never /tmp or $HOME). Nothing runs between checks. `check`
(resume command, run it as often as you like) restarts the same RustFS and daemon from that state, asserts what the clock now
allows, appends RESULT lines to $WO069_GATE_STATE/results.log and stops them again. `finish` runs a last check, retires the orphan
with gc --confirmed when its 14 days are up, verifies the bucket and registry entry are gone, stops everything and deletes the key.

  worktrees   grace (g): object grace 24 h and hash-cache re-hash 24 h, on the real clock (rows object-grace-24h, rehash-24h)
              open (published, then left alone)          -> forgotten_open after 3 d
              hand (shared --handoff, never claimed)     -> unclaimed_handoff after 24 h
              orph (published, git worktree removed)     -> daemon marks orphan on a sweep after 24 h missing; gc due 14 d after marking
              land (handoff, claimed, landed with git, promoted, archived) -> its registry entry must stay gone
  Run `check` at about T+5 min (baseline, nothing listed), T+23 h (negatives), T+25 h, T+3 d 1 h, T+15 d 2 h, then `finish`.
  Each row says whether its threshold was crossed at that moment: before it, the row asserts the item is NOT reported.
  --smoke               sets WEFTOS_WORKTREE_ORPHAN_AFTER_HOURS=1 and _ORPHAN_ARCHIVE_AFTER_DAYS=1 for the daemon. The open (3 d) and unclaimed
                        (24 h) thresholds have NO override, so a smoke run exercises only the orphan chain; every smoke line is tagged SMOKE
                        and is NOT evidence for the defaults.
  --merger=second-host  the claim, fetch and land of `land` run on the second host's merger daemon. REFUSES unless WO069_OWNER_GO_Q3=1.
                        Default local: a second principal and a second clone on this machine (already evidenced; adds nothing new).
Environment: WO069_GATE_STATE (required), WEFTOS_STORE_TEST_RUSTFS_BIN (required for start/check), WEFTOS_E2E_WEAVER (default target/debug/weaver).
EOF
}
CMD=""; SMOKE=0; MERGER=local; HOST="aepod@aepod-xpc"
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        --smoke) SMOKE=1; shift ;;
        --merger=local|--merger=second-host) MERGER=${1#--merger=}; shift ;;
        --second-host) HOST=$2; shift 2 ;;
        start|check|finish) CMD=$1; shift ;;
        *) gate_die "unknown option $1 (see --help)" ;;
    esac
done
[ -n "$CMD" ] || { usage >&2; exit 2; }
gate_refuse_pg "second host" "$HOST"
[ "$MERGER" != second-host ] || gate_require_go_q3 "--merger=second-host"
STATE=${WO069_GATE_STATE:-}; [ -n "$STATE" ] || gate_die "set WO069_GATE_STATE (absolute, on the notes volume)"
gate_cred_dir_ok "$STATE"
BIN=${WEFTOS_STORE_TEST_RUSTFS_BIN:-}; WEAVER=${WEFTOS_E2E_WEAVER:-$GATE_REPO/target/debug/weaver}; S="$GATE_REPO/scripts/worktree-store.sh"
META="$STATE/meta.env"; RESL="$STATE/results.log"; GATE_TMP="$STATE/tmp"; W="$STATE"
OPEN_S=259200; UNCL_S=86400; ORPH_S=86400; ARCH_S=1209600

if [ "$GATE_DRY" = 1 ]; then
    gate_say "== item 4 $CMD (dry run): state dir $STATE, smoke=$SMOKE, merger=$MERGER"
    case "$CMD" in
    start)
        gate_note "refuse if $META exists; mkdir -m 700 $STATE; random root key to $STATE/rustfs.env (0600); start $BIN on a free loopback port, data in $STATE/rfs-data"
        gate_note "daemon (isolated HOME/run under $STATE; smoke: ORPHAN_AFTER_HOURS=1, ARCHIVE_AFTER_DAYS=1); project; worktrees open, hand, orph, land"
        gate_note "open: new + register + sweep. hand: sweep, then share --handoff. orph: sweep, git worktree remove, sweep (stamps missing_since)"
        gate_note "land: share --handoff; merger claim (epoch); fetch; git fetch bundle + merge --ff-only in a second clone; promote --epoch; store archive --delete; registry entry must vanish"
        gate_note "write $META (start epoch, port, project, smoke); stop daemon and RustFS; print the T+ schedule" ;;
    check|finish)
        gate_note "read $META; restart RustFS (same data dir, key and port) and the daemon; elapsed = now - start"
        gate_note "rows: open-forgotten(3d) hand-unclaimed(24h) orphan-marked(24h) orphan-gc-due(marked+14d) land-stays-gone; before a threshold assert NOT reported, after it assert reported"
        gate_note "append RESULT|item4|name|PASS/FAIL/NOTYET|elapsed_s|detail to $RESL; stop daemon and RustFS"
        [ "$CMD" = check ] || gate_note "finish: gc --confirmed on the orphan when due, verify bucket and registry entry gone, delete $STATE/rustfs.env and tmp" ;;
    esac
    exit 0
fi

mkdir -p "$STATE"; chmod 700 "$STATE"; mkdir -p "$GATE_TMP"; chmod 700 "$GATE_TMP"
res() { local line; line=$(printf 'RESULT|item4|%s|%s|%s|%s%s' "$1" "$2" "$3" "${SMOKE_TAG:-}" "$4"); echo "$line" | tee -a "$RESL"; }
ulid() { python3 -c "import random; print('01'+''.join(random.choice('0123456789ABCDEFGHJKMNPQRSTVWXYZ') for _ in range(24)))"; }
rhex() { head -c "$1" /dev/urandom | od -An -tx1 | tr -d ' \n'; }
gx() { gate_gx "$W" "$1" "$PROJECT" "$WEAVER" "${@:2}"; }
G() { git -C "$1" "${@:2}"; }

rfs_up() { # same data dir, key and port every time
    [ -x "$BIN" ] || gate_die "set WEFTOS_STORE_TEST_RUSTFS_BIN to the rustfs binary"
    local ak sk code
    [ -f "$STATE/rustfs.env" ] || ( umask 077; printf 'MESH_S3_ENDPOINT=http://127.0.0.1:%s\nMESH_S3_ACCESS_KEY=g4%s\nMESH_S3_SECRET_KEY=%s\nMESH_S3_REGION=us-east-1\n' "$PORT" "$(rhex 6)" "$(rhex 20)" >"$STATE/rustfs.env" )
    ak=$(gate_env_get "$STATE/rustfs.env" MESH_S3_ACCESS_KEY); sk=$(gate_env_get "$STATE/rustfs.env" MESH_S3_SECRET_KEY); mkdir -p "$STATE/rfs-data"
    env RUSTFS_ACCESS_KEY="$ak" RUSTFS_SECRET_KEY="$sk" "$BIN" server --address "127.0.0.1:$PORT" "$STATE/rfs-data" >>"$STATE/rustfs.log" 2>&1 &
    RPID=$!
    for _ in $(seq 1 360); do code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$PORT/" || true); [ "$code" != 000 ] && [ "$code" != 503 ] && { sleep 2; return 0; }; kill -0 "$RPID" 2>/dev/null || return 1; sleep 0.5; done
    return 1
}
stop_all() { [ -z "${MPID:-}" ] || gate_kill_tree "$MPID"; [ -z "${DPID:-}" ] || gate_kill_tree "$DPID"; [ -z "${RPID:-}" ] || gate_kill_tree "$RPID"; sleep 1; echo "stopped: daemon and RustFS (nothing of this drill is running)"; }
up_all() {
    [ -x "$WEAVER" ] || gate_die "no weaver at $WEAVER"
    mkdir -p "$W/home-writer"; [ -f "$W/gitconfig" ] || gate_gitconfig "$W"
    rfs_up || gate_die "RustFS did not come up (see $STATE/rustfs.log)"
    local ov=(); [ "$SMOKE" = 0 ] || ov=(WEFTOS_WORKTREE_ORPHAN_AFTER_HOURS=1 WEFTOS_WORKTREE_ORPHAN_ARCHIVE_AFTER_DAYS=1)
    gate_daemon_up "$W" writer "$PROJECT" "$WEAVER" "$STATE/rustfs.env" ${ov[@]+"${ov[@]}"} || gate_die "daemon did not come up (see $W/daemon-writer.out)"
    DPID=$GATE_PID
    gate_daemon_up "$W" merger "$PROJECT" "$WEAVER" "$STATE/rustfs.env" ${ov[@]+"${ov[@]}"} && MPID=$GATE_PID || true
}
trap 'stop_all' EXIT INT TERM
new_wt() { # <letter> <branch> <title>
    local d="$STATE/wt-$1"; G "$STATE/repo" worktree add -q -b "$2" "$d" master; mkdir -p "$d/docs"; printf '# %s\n\n%s body\n' "$3" "$3" >"$d/docs/$1.md"
    ( cd "$d" && gx writer "$S" store-create && gx writer "$S" register ) >/dev/null
}
sweep_out() { ( cd "$STATE" && gx writer "$S" sweep --only "$1" ) 2>&1; }
listed() { gx writer "$S" leftovers 2>&1 | grep -i "$1" | grep -qiE "$2"; }

# worktree g: the 24 h object grace and the 24 h hash-cache re-hash, both on the real clock
start_grace_rehash() {
    local d="$STATE/wt-g" g1 old new
    new_wt g feat/g "Grace Rehash"; head -c 2097152 /dev/urandom >"$d/big.bin"; sweep_out feat--g >/dev/null
    g1=$(gate_sha256 "$d/docs/g.md"); old=$(gate_sha256 "$d/big.bin")
    printf 'v2\n' >>"$d/docs/g.md"; sweep_out feat--g >/dev/null          # publish 2 retires the v1 object of g.md (grace starts)
    python3 -I - "$d/big.bin" <<'PY'
import os, sys
p = sys.argv[1]; st = os.stat(p); d = open(p, 'rb').read()
open(p, 'wb').write(bytes((b + 1) % 256 for b in d)); os.utime(p, ns=(st.st_atime_ns, st.st_mtime_ns))
PY
    new=$(gate_sha256 "$d/big.bin"); printf 'v3\n' >>"$d/docs/g.md"; sweep_out feat--g >/dev/null   # big.bin: same size and mtime, other bytes
    printf 'G1SHA=%s\nOLDBIG=%s\nNEWBIG=%s\n' "$g1" "$old" "$new" >>"$META"
    rm -rf "$STATE/fetch-g0"; gx writer "$S" fetch feat--g "$STATE/fetch-g0" >/dev/null 2>&1
    res rehash-at-start INFO 0 "fetched big.bin is the $([ "$(gate_sha256 "$STATE/fetch-g0/big.bin")" = "$old" ] && echo OLD || echo NEW) bytes (design: the cache trusts size+mtime until the 24 h full pass)"
}

do_start() {
    [ ! -e "$META" ] || gate_die "$STATE already holds a drill ($META): finish it, or use another WO069_GATE_STATE"
    PORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'); PROJECT=$(ulid)
    : >"$RESL"; up_all
    printf 'START=%s\nPORT=%s\nPROJECT=%s\nSMOKE=%s\nMERGER=%s\nORPHAN_MARKED_AT=\n' "$(date +%s)" "$PORT" "$PROJECT" "$SMOKE" "$MERGER" >"$META"
    git init -q -b master "$STATE/repo"; echo base >"$STATE/repo/README.md"; G "$STATE/repo" add .; G "$STATE/repo" commit -qm base
    new_wt a feat/a "Leftover Open"; new_wt h feat/h "Leftover Handoff"; new_wt o feat/o "Leftover Orphan"; new_wt l feat/l "Leftover Land"
    gx writer "$S" sweep >"$STATE/first-sweep.out" 2>&1
    ( cd "$STATE/wt-h" && printf 'x\n' >>docs/h.md && git add -A && git commit -qm w && gx writer "$S" share --handoff "unclaimed on purpose" ) >/dev/null 2>&1
    G "$STATE/repo" worktree remove --force "$STATE/wt-o"; sweep_out feat--o >/dev/null
    ( cd "$STATE/wt-l" && printf 'pub fn landed() {}\n' >lib.rs && git add -A && git commit -qm land && gx writer "$S" share --handoff "to land" ) >/dev/null 2>&1
    local epoch head; head=$(G "$STATE/wt-l" rev-parse HEAD)
    if [ "$MERGER" = second-host ]; then
        gate_say "second-host merger: run on $HOST (follower/merger daemon per the runbook): worktree-store.sh claim feat--l; fetch feat--l <dir>; land with git; store worktree promote --epoch; then press return"
        gate_run ssh -o BatchMode=yes "$HOST" 'podman exec wo069-gate-fmerger bash /src/scripts/worktree-store.sh claim feat--l'; read -r _ </dev/tty
    else
        gx merger "$S" claim feat--l >"$STATE/claim.out" 2>&1; epoch=$(sed -n 's/.*"epoch": *\([0-9]*\).*/\1/p' "$STATE/claim.out" | head -1)
        git clone -q "$STATE/repo" "$STATE/fetch-l"; git clone -q "$STATE/repo" "$STATE/clone-l"
        gx merger "$S" fetch feat--l "$STATE/fetch-l" >/dev/null 2>&1
        G "$STATE/clone-l" fetch -q "$STATE/fetch-l/.wt-branch.bundle" '+refs/heads/*:refs/shared/feat--l/*' && G "$STATE/clone-l" merge -q --ff-only refs/shared/feat--l/feat/l
        [ "$(G "$STATE/clone-l" rev-parse HEAD)" = "$head" ] || gate_die "second clone did not land the head"
        gx merger "$WEAVER" store worktree promote --project "$PROJECT" --slug feat--l --epoch "$epoch" >/dev/null
        gx writer "$WEAVER" store archive "weftos://local/projects/$PROJECT/worktrees/feat--l/store" --delete >/dev/null
    fi
    start_grace_rehash
    gx writer "$S" browse --json | jq -e '[.worktrees[].slug] | index("feat--l") == null' >/dev/null && res land-registry-entry-gone PASS 0 "after promote and archive, at start" || res land-registry-entry-gone FAIL 0 "entry still listed at start"
    cat <<EOF
started at $(date -u +%FT%TZ). State: $STATE. Nothing is running now.
Resume with:  WO069_GATE_STATE=$STATE WEFTOS_STORE_TEST_RUSTFS_BIN=$BIN $0 check$([ "$SMOKE" = 1 ] && echo ' --smoke')
Schedule: now+5min baseline, now+23h negatives, now+25h unclaimed and orphan, now+3d1h forgotten_open, now+15d2h gc due, then finish.
EOF
}

do_check() {
    [ -f "$META" ] || gate_die "no drill in $STATE (run start)"
    # shellcheck disable=SC1090
    . "$META"; PORT=$PORT; PROJECT=$PROJECT; SMOKE=$SMOKE; SMOKE_TAG=$([ "$SMOKE" = 1 ] && echo 'SMOKE ')
    [ "$SMOKE" = 0 ] || { ORPH_S=3600; ARCH_S=86400; }
    local el out; el=$(( $(date +%s) - START ))
    up_all; gate_say "== check at T+${el}s ($(awk "BEGIN{printf \"%.1f\", $el/3600}") h)"
    row() { # <name> <due seconds> <slug> <why regex>
        local st=PASS note
        if [ "$el" -lt "$2" ]; then if listed "$3" "$4"; then st=FAIL; note="reported before its $2 s threshold"; else note="correctly NOT reported yet (threshold $2 s, next due in $(( $2 - el )) s)"; fi
        elif listed "$3" "$4"; then note="reported after its $2 s threshold"
        else st=FAIL; note="NOT reported although the $2 s threshold passed"; fi
        res "$1" "$st" "$el" "$note"
    }
    row open-forgotten "$OPEN_S" feat--a 'forgotten'
    row hand-unclaimed "$UNCL_S" feat--h 'unclaimed'
    out=$(sweep_out feat--o)
    if [ "$el" -lt "$ORPH_S" ]; then echo "$out" | grep -q '"marked": true' && res orphan-marked FAIL "$el" "marked before the grace" || res orphan-marked PASS "$el" "grace not over: marked:false (due in $(( ORPH_S - el )) s)"
    elif echo "$out" | grep -q '"marked": true'; then
        [ -n "${ORPHAN_MARKED_AT:-}" ] || { sed -i.bak "s/^ORPHAN_MARKED_AT=.*/ORPHAN_MARKED_AT=$(date +%s)/" "$META"; rm -f "$META.bak"; ORPHAN_MARKED_AT=$(date +%s); }
        res orphan-marked PASS "$el" "daemon marked it orphan at T+${el}s (first mark time kept in meta)"; row orphan-listed 0 feat--o 'orphan'
    else res orphan-marked FAIL "$el" "grace over but not marked: $(echo "$out" | tr '\n\t' '  ' | cut -c1-140)"; fi
    if [ -n "${ORPHAN_MARKED_AT:-}" ]; then
        local age=$(( $(date +%s) - ORPHAN_MARKED_AT )); gx writer "$WEAVER" store worktree gc --project "$PROJECT" >"$STATE/gc.json" 2>&1 || true
        local isdue; isdue=$(jq -r '.orphans[] | select(.slug=="feat--o") | .due' "$STATE/gc.json" 2>/dev/null || echo '?')
        if [ "$age" -lt "$ARCH_S" ]; then [ "$isdue" = false ] && res orphan-gc-due PASS "$el" "orphan age ${age} s, not due (due=false), correct" || res orphan-gc-due FAIL "$el" "due=$isdue at age ${age} s"
        else [ "$isdue" = true ] && res orphan-gc-due PASS "$el" "orphan age ${age} s, due=true" || res orphan-gc-due FAIL "$el" "due=$isdue at age ${age} s past $ARCH_S"; fi
    fi
    if [ -n "${G1SHA:-}" ]; then
        local pn wb; pn=$(gx writer "$WEAVER" store ls --json 2>/dev/null | grep -o "weftos://[^\"' ]*projects/$PROJECT/store" | head -1)
        printf 'c%s\n' "$el" >>"$STATE/wt-g/docs/g.md"; sweep_out feat--g >/dev/null     # a publish triggers the object sweep and a full re-hash when due
        wb=$(gx writer "$WEAVER" store get "$pn" worktrees/feat--g.json --as agent:writer | jq -r .bucket)
        GATE_OUT="$STATE/objs.xml" gate_s3raw "$STATE/rustfs.env" GET "/$wb?list-type=2&prefix=.wt/obj/" >/dev/null
        if [ "$el" -lt 86400 ]; then grep -q "$G1SHA" "$STATE/objs.xml" && res object-grace-24h PASS "$el" "a publish ran and the retired v1 object is still there (grace not over)" || res object-grace-24h FAIL "$el" "retired object deleted before 24 h"
        else grep -q "$G1SHA" "$STATE/objs.xml" && res object-grace-24h FAIL "$el" "retired object still there after 24 h and a publish" || res object-grace-24h PASS "$el" "retired v1 object deleted by the first publish after 24 h"; fi
        rm -rf "$STATE/fetch-g1"; gx writer "$S" fetch feat--g "$STATE/fetch-g1" >/dev/null 2>&1; got=$(gate_sha256 "$STATE/fetch-g1/big.bin")
        if [ "$el" -lt 86400 ]; then res rehash-24h INFO "$el" "big.bin is the $([ "$got" = "$OLDBIG" ] && echo OLD || echo NEW) bytes before 24 h (design: OLD)"
        else [ "$got" = "$NEWBIG" ] && res rehash-24h PASS "$el" "the 24 h full pass picked up the same-size same-mtime edit" || res rehash-24h FAIL "$el" "big.bin still the OLD bytes after 24 h and a publish"; fi
    fi
    gx writer "$S" browse --json | jq -e '[.worktrees[].slug] | index("feat--l") == null' >/dev/null && res land-stays-gone PASS "$el" "landed worktree's entry still absent" || res land-stays-gone FAIL "$el" "landed entry reappeared"
}

do_finish() {
    do_check
    # shellcheck disable=SC1090
    . "$META"
    if [ -n "${ORPHAN_MARKED_AT:-}" ] && [ $(( $(date +%s) - ORPHAN_MARKED_AT )) -ge "$ARCH_S" ]; then
        gx writer "$WEAVER" store worktree gc --project "$PROJECT" --confirmed >"$STATE/gc-confirmed.json" 2>&1
        jq -e '.orphans_retired | index("feat--o") != null' "$STATE/gc-confirmed.json" >/dev/null && res orphan-retired PASS 0 "gc --confirmed retired feat--o" || res orphan-retired FAIL 0 "see $STATE/gc-confirmed.json"
        gx writer "$S" browse --json | jq -e '[.worktrees[].slug] | index("feat--o") == null' >/dev/null && res orphan-entry-gone PASS 0 "registry entry removed" || res orphan-entry-gone FAIL 0 "registry entry still there"
    else res orphan-retired NOTYET 0 "archive delay not reached: do not finish yet if the 14 d row is needed"; fi
    stop_all; trap - EXIT INT TERM; rm -f "$STATE/rustfs.env"; rm -rf "$GATE_TMP"
    gate_say "finished: key deleted, results kept in $RESL. Remove $STATE yourself when the numbers are pasted into the runbook."
}

case "$CMD" in start) do_start ;; check) do_check ;; finish) do_finish ;; esac
