#!/usr/bin/env bash
# WO-069 lane F2, ADR-126 section 4a gate item 5: failure drills with real kills and a real 30 minutes.
set -euo pipefail
export GATE_NAME=wo069-gate-item5
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-item5-failures.sh writer-kill|merger-kill|merger-check|refresh-kill [--dry-run]
       WO069_GATE_WRITER_ENV=<0600 env file of a THROWAWAY store> WO069_GATE_STATE=<dir> wo069-gate-item5-failures.sh ...

  writer-kill    the writer daemon, publishing with its OWN scoped key, is killed with kill -9 at several offsets into a publish of
                 WO069_GATE_FILES files (default 300); after each kill: restart, the previous manifest is still served, a fetch has
                 no checksum error, the rerun completes and the fetched tree equals the worktree. Offsets: WO069_GATE_DELAYS
                 (default "0.4 0.9 1.5 2.2 3 4 5.5" seconds). Real kills of a real process on a real store; the offsets are wall-clock,
                 so which request the kill lands on varies run to run (that is the point).
  merger-kill    a handed-off worktree; merger daemon M starts a claim; a loopback proxy forwards the claim's conditional PUT of the
                 handoff record in full, WITHHOLDS the response and signals; M is killed with kill -9 0.3 s later, so the claim is
                 on the store and its owner never heard of it. A second daemon T then calls `claim` every 60 s for real, under
                 caffeinate, from a background loop, and returns at once. T's own observation of the unchanged record
                 must run 30 minutes before it takes over (a daemon restart resets that watch: T must stay up).
  merger-check   run at T+31 min (and again whenever): reads the loop's log, asserts refusals up to 29 min and takeover at the first
                 attempt after 30:00 of T's observation, epoch 2, and that epoch 1 is fenced out of promote. Stops the daemons.
  refresh-kill   the daemon is killed BETWEEN the two writes of one registry refresh: the proxy drops the PUT of worktrees/_digest.json
                 (the second write) after the entry write went through, and the daemon is killed. A digest follower and a --full
                 follower must then disagree, and the next refresh must heal it.
What can be timed from outside: a kill at a wire-level boundary (after a named request is applied, before its answer; or between two named
requests), and a kill at a wall-clock offset. What cannot: a kill at a named instruction inside the daemon, or inside the store server.
Environment: WO069_GATE_WRITER_ENV (required; the store's endpoint must be loopback, e.g. the ssh forward), WO069_GATE_STATE (required for
merger-*), WEFTOS_E2E_WEAVER (default target/debug/weaver), WO069_GATE_FILES, WO069_GATE_DELAYS, KEEP=1. Results: RESULT|item5|... lines.
EOF
}
CMD=""
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        writer-kill|merger-kill|merger-check|refresh-kill) CMD=$1; shift ;;
        *) gate_die "unknown option $1 (see --help)" ;;
    esac
done
[ -n "$CMD" ] || { usage >&2; exit 2; }
WENV=${WO069_GATE_WRITER_ENV:-}; [ -n "$WENV" ] || gate_die "set WO069_GATE_WRITER_ENV (0600 env file of a throwaway store)"
gate_secret_ok "$WENV"; gate_refuse_pg_env "$WENV"
WEAVER=${WEFTOS_E2E_WEAVER:-$GATE_REPO/target/debug/weaver}; S="$GATE_REPO/scripts/worktree-store.sh"
# the default state dir name is short on purpose: a unix socket path (<run dir>/child-ipc/child.sock) must stay under 104 bytes: run dir + 20 < 104
FILES=${WO069_GATE_FILES:-300}; DELAYS=${WO069_GATE_DELAYS:-"0.4 0.9 1.5 2.2 3 4 5.5"}
if [ "$CMD" = writer-kill ] || [ "$CMD" = refresh-kill ]; then W=${WO069_GATE_STATE:-$GATE_REPO/target/g5.$CMD}; else W=${WO069_GATE_STATE:-}; fi

if [ "$GATE_DRY" = 1 ]; then
    gate_say "== item 5 $CMD (dry run)"
    case "$CMD" in
    writer-kill)
        gate_note "start writer daemon (key from $WENV), project, one worktree with $FILES files; baseline publish"
        gate_note "for each delay in [$DELAYS]: add $FILES new files; sweep in the background; sleep delay; kill -9 the daemon and its children"
        gate_note "restart the daemon; follow --full and browse (previous manifest still served); fetch into a fresh dir (no checksum error); rerun sweep; fetch again and diff -r against the worktree" ;;
    merger-kill)
        gate_note "start proxy: python3 wo069-gate-hold-proxy.py <port> <store host> <store port> <ctl>; arm: hold PUT handoff.json; daemons M (through the proxy) and T (direct)"
        gate_note "writer: share --handoff; M: claim in the background; wait for ctl/held; sleep 0.3; kill -9 M; stop the proxy"
        gate_note "T: nohup caffeinate loop of 'claim' every 60 s into $W/stale.log until it succeeds; return; run merger-check at T+31 min" ;;
    merger-check)
        gate_note "read $W/stale.log: first refusal time, refusal count, takeover time, epoch; promote --epoch 1 must be refused; stop T and delete state" ;;
    refresh-kill)
        gate_note "proxy armed: drop PUT worktrees/_digest.json; commit and sweep; entry PUT passes, digest PUT dropped, kill -9 the daemon"
        gate_note "restart; digest follower versus follow --full must differ; another commit and sweep must make them agree" ;;
    esac
    exit 0
fi

[ -x "$WEAVER" ] || gate_die "no weaver at $WEAVER"
mkdir -p "$W"; chmod 700 "$W"; GATE_TMP="$W/tmp"; mkdir -p "$GATE_TMP"
PROJECT=${PROJECT:-$(python3 -c "import random; print('01'+''.join(random.choice('0123456789ABCDEFGHJKMNPQRSTVWXYZ') for _ in range(24)))")}
[ ! -f "$W/project" ] || PROJECT=$(cat "$W/project"); echo "$PROJECT" >"$W/project"
gx() { gate_gx "$W" "$1" "$PROJECT" "$WEAVER" "${@:2}"; }
G() { git -C "$1" "${@:2}"; }
res() { printf 'RESULT|item5|%s|%s|%s|%s\n' "$1" "$2" "$3" "$4" | tee -a "$W/results.log"; }
PIDS=(); PROXY=""
cleanup() { trap - EXIT INT TERM; local p; [ -z "$PROXY" ] || kill "$PROXY" 2>/dev/null || true; for p in ${PIDS[@]+"${PIDS[@]}"}; do gate_kill_tree "$p"; done; [ "$CMD" != writer-kill ] && [ "$CMD" != refresh-kill ] || { [ -n "${KEEP:-}" ] || rm -rf "$W"; }; }
case "$CMD" in merger-kill) ;; *) trap cleanup EXIT INT TERM ;; esac
up() { gate_daemon_up "$W" "$1" "$PROJECT" "$WEAVER" "${2:-$WENV}" || gate_die "daemon $1 did not come up ($W/daemon-$1.out)"; PIDS+=("$GATE_PID"); }
[ -f "$W/gitconfig" ] || gate_gitconfig "$W"
seed_project() { # <slug letter> <title>: repo, worktree, store, grants, register
    git init -q -b master "$W/repo"; echo base >"$W/repo/README.md"; G "$W/repo" add .; G "$W/repo" commit -qm base
    G "$W/repo" worktree add -q -b "feat/$1" "$W/wt-$1" master; mkdir -p "$W/wt-$1/docs"; printf '# %s\n\nbody\n' "$2" >"$W/wt-$1/docs/$1.md"
    ( cd "$W/wt-$1" && gx writer "$S" store-create && gx writer "$S" register ) >/dev/null
    local pn; pn=$(gx writer "$WEAVER" store ls --json 2>/dev/null | grep -o "weftos://[^\"' ]*projects/$PROJECT/store" | head -1)
    for who in "writer write" "follower read" "merger write" "taker write"; do gx writer "$WEAVER" store grant "$pn" "agent:${who% *}" "${who#* }" >/dev/null; done
}
proxy_up() { # sets PPORT CTL and PENV: a copy of the env file whose endpoint is the proxy
    local ep host port; ep=$(gate_env_get "$WENV" MESH_S3_ENDPOINT); host=${ep#http://}; host=${host%%:*}; port=${ep##*:}; port=${port%%/*}
    case "$host" in 127.0.0.1|localhost) ;; *) gate_die "the store endpoint must be loopback (use the ssh forward): got $host" ;; esac
    PPORT=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])'); CTL="$W/ctl"; mkdir -p "$CTL"; rm -f "$CTL"/hold "$CTL"/held "$CTL"/release
    python3 -I "$GATE_REPO/scripts/store/wo069-gate-hold-proxy.py" "$PPORT" "$host" "$port" "$CTL" >"$W/proxy.out" 2>&1 & PROXY=$!
    sleep 1; PENV="$W/proxy.env"; ( umask 077; sed "s#^MESH_S3_ENDPOINT=.*#MESH_S3_ENDPOINT=http://127.0.0.1:$PPORT#" "$WENV" >"$PENV" )
}
wait_held() { local i; for i in $(seq 1 600); do [ -f "$CTL/held" ] && return 0; sleep 0.1; done; return 1; }
files() { local d=$1 n=$2 g=$3 i; mkdir -p "$d/gen$g"; for i in $(seq 1 "$n"); do head -c 12000 /dev/urandom | base64 >"$d/gen$g/f$i.txt"; done; }

case "$CMD" in
writer-kill)
    up writer; WP=$GATE_PID; seed_project w "Writer Kill"; ( cd "$W" && gx writer "$S" sweep --only feat--w ) >/dev/null 2>&1 || true; g=0; up follower
    for d in $DELAYS; do
        g=$((g + 1)); files "$W/wt-w" "$FILES" "$g"; ( cd "$W" && gx writer "$S" sweep --only feat--w >"$W/kill-$d.out" 2>&1 ) & sp=$!
        sleep "$d"; gate_kill_tree "$WP"; wait "$sp" 2>/dev/null || true
        # the sweep's client output says "daemon closed connection" / "share exited" when the kill landed before the publish answered; a finished publish prints its JSON
        landed=$(grep -qE 'daemon closed connection|share exited' "$W/kill-$d.out" && echo MID-PUBLISH || echo after-publish)
        up writer; WP=$GATE_PID
        rm -rf "$W/f-$d"; frc=0; out=$(gx follower "$S" fetch feat--w "$W/f-$d" 2>&1) || frc=$?
        nf=$(find "$W/f-$d" -type f 2>/dev/null | wc -l | tr -d ' '); bad=$(diff -rq --exclude=.git --exclude='.wt-*' "$W/wt-w" "$W/f-$d" 2>&1 | grep -c ' differ$' || true)
        # the fetched tree is whatever manifest the store serves now (the previous one, or the new one when the kill came after the manifest write):
        # it must be read without a checksum error, complete (the fetch exits 0) and byte-equal, file by file, to the worktree files it names
        if echo "$out" | grep -qi checksum; then res "kill-at-${d}s-previous-manifest-clean" FAIL 0 "checksum error through the served manifest ($landed)"
        elif [ "$frc" != 0 ] || [ "$bad" != 0 ]; then res "kill-at-${d}s-previous-manifest-clean" FAIL 0 "fetch rc=$frc, $bad differing file(s) ($landed): $(echo "$out" | head -2 | tr '\n' ' ' | cut -c1-120)"
        else res "kill-at-${d}s-previous-manifest-clean" PASS 0 "no checksum error, fetch rc 0, $nf files all byte-equal to the worktree ($landed)"; fi
        ( cd "$W" && gx writer "$S" sweep --only feat--w ) >"$W/rerun-$d.out" 2>&1 || true
        rm -rf "$W/g-$d"; gx follower "$S" fetch feat--w "$W/g-$d" >/dev/null 2>&1 || true
        if diff -rq --exclude=.git --exclude='.wt-*' "$W/wt-w" "$W/g-$d" >/dev/null 2>&1; then res "kill-at-${d}s-rerun-complete" PASS 0 "fetched tree equals the worktree"; else res "kill-at-${d}s-rerun-complete" FAIL 0 "fetched tree differs: $(diff -rq --exclude=.git --exclude='.wt-*' "$W/wt-w" "$W/g-$d" 2>&1 | head -2 | tr '\n' ' ')"; fi
    done ;;
merger-kill)
    : >"$W/results.log"; proxy_up; up writer; WP=$GATE_PID; seed_project m "Merger Kill"
    ( cd "$W/wt-m" && printf 'x\n' >>docs/m.md && git add -A && git commit -qm w && gx writer "$S" share --handoff "claim me" ) >/dev/null 2>&1
    gate_daemon_up "$W" merger "$PROJECT" "$WEAVER" "$PENV" || gate_die "merger daemon did not come up"; MP=$GATE_PID
    gate_daemon_up "$W" taker "$PROJECT" "$WEAVER" "$WENV" || gate_die "taker daemon did not come up"; TP=$GATE_PID
    echo "hold handoff.json" >"$CTL/hold"
    ( gx merger "$S" claim feat--m >"$W/claim-m.out" 2>&1 || true ) & CP=$!
    wait_held || { res merger-claim-held FAIL 0 "the claim's PUT never reached the proxy"; exit 1; }
    sleep 0.3; gate_kill_tree "$MP"; wait "$CP" 2>/dev/null || true; kill "$PROXY" 2>/dev/null || true; PROXY=""
    res merger-killed-mid-claim PASS 0 "claim PUT applied, answer withheld, merger daemon killed 0.3 s later (client said: $(head -c 100 "$W/claim-m.out" | tr '\n' ' '))"
    T0=$(date +%s); echo "T0=$T0" >"$W/meta"
    cat >"$W/taker-loop.sh" <<LOOP
#!/usr/bin/env bash
# the taker's claim attempts, one a minute, until one succeeds. Appends "<epoch> rc=<n> <output head>" to stale.log.
while :; do
    out=\$(env HOME='$W/home-taker' WEFTOS_RUNTIME_DIR='$W/run-taker' WEFTOS_AGENT=taker XDG_STATE_HOME='$W/xdg-taker' WTS_STATE_DIR='$W/xdg-taker/wts' \\
        WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER='$WEAVER' WTS_PROJECT='$PROJECT' GIT_CONFIG_GLOBAL='$W/gitconfig' '$S' claim feat--m 2>&1); rc=\$?
    echo "\$(date +%s) rc=\$rc \$(echo "\$out" | tr '\\n' ' ' | cut -c1-160)" >>'$W/stale.log'
    [ \$rc = 0 ] && break
    sleep 60
done
LOOP
    : >"$W/stale.log"; nohup caffeinate -ims bash "$W/taker-loop.sh" >/dev/null 2>&1 &
    LP=$!; echo "$WP $TP $LP" >"$W/pids"
    gate_say "merger killed mid-claim. The taker loop runs for about 31 minutes under caffeinate (taker daemon pid $TP must stay up; do not restart it). Come back at $(date -v+31M +%H:%M 2>/dev/null || date -d '+31 min' +%H:%M) and run: WO069_GATE_STATE=$W WO069_GATE_WRITER_ENV=$WENV $0 merger-check" ;;
merger-check)
    [ -f "$W/meta" ] || gate_die "no merger-kill state in $W"; . "$W/meta"
    first=$(awk 'NR==1{print $1}' "$W/stale.log"); last=$(tail -1 "$W/stale.log"); refused=$(grep -vc 'rc=0' "$W/stale.log" || true)
    if echo "$last" | grep -q 'rc=0'; then
        taken=$(echo "$last" | awk '{print $1}'); obs=$(( taken - first ))
        [ "$obs" -ge 1800 ] && [ "$refused" -ge 29 ] && res merger-takeover-after-30-min PASS "$obs" "taker took over at ${obs}s of its own observation after $refused refusals; line: $(echo "$last" | cut -c1-120)" || res merger-takeover-after-30-min FAIL "$obs" "observation ${obs}s, refusals $refused (want >= 1800 and >= 29)"
        grep -q '"epoch": *2' "$W/stale.log" && res takeover-epoch-2 PASS 0 "new epoch 2" || res takeover-epoch-2 FAIL 0 "epoch 2 not seen in the last line: $(echo "$last" | cut -c1-120)"
        if gx taker "$WEAVER" store worktree promote --project "$PROJECT" --slug feat--m --epoch 1 >"$W/promote1.out" 2>&1; then res stale-epoch-fenced FAIL 0 "promote with epoch 1 was ACCEPTED"; else res stale-epoch-fenced PASS 0 "promote with epoch 1 refused: $(head -c 100 "$W/promote1.out" | tr '\n' ' ')"; fi
        for p in $(cat "$W/pids"); do gate_kill_tree "$p"; done
        gate_say "stopped. Delete $W when the numbers are in the runbook."
    else res merger-takeover-after-30-min NOTYET "$(( $(date +%s) - T0 ))" "not taken over yet after $(( ($(date +%s) - T0) / 60 )) min and $refused refusals; the taker takes over at its first attempt after 30:00 of observation"; fi ;;
refresh-kill)
    proxy_up; up writer; WP=$GATE_PID; seed_project r "Refresh Kill"; gx writer "$S" sweep --only feat--r >/dev/null 2>&1 || true
    up follower
    # both followers learn the worktree at its first head BEFORE the cut (a fresh state file would report "new", which says nothing about a moved head)
    gx follower "$S" follow --state "$W/fs-digest.json" >/dev/null 2>&1 || true
    gx follower "$WEAVER" store worktree follow --project "$PROJECT" --state "$W/fs-full.json" --full --json >/dev/null 2>&1 || true
    gate_kill_tree "$WP"; PIDS=(); sleep 1   # the first writer daemon (direct to the store) must be gone: two daemons on one runtime dir share one socket and the client would talk to the old one
    gate_daemon_up "$W" writer "$PROJECT" "$WEAVER" "$PENV" || gate_die "writer daemon through the proxy did not come up"; PIDS+=("$GATE_PID"); WP=$GATE_PID
    echo "drop _digest.json" >"$CTL/hold"
    printf 'v2\n' >>"$W/wt-r/docs/r.md"; G "$W/wt-r" add -A; G "$W/wt-r" commit -qm v2
    ( cd "$W" && gx writer "$S" sweep --only feat--r >"$W/refresh.out" 2>&1 || true ) & sp=$!
    wait_held || { res refresh-cut FAIL 0 "the digest PUT never reached the proxy"; exit 1; }
    gate_kill_tree "$WP"; wait "$sp" 2>/dev/null || true; PIDS=()
    res refresh-cut PASS 0 "entry PUT applied, digest PUT dropped, daemon killed (wire-level boundary; not an in-daemon instruction)"
    up writer
    gx follower "$S" follow --state "$W/fs-digest.json" >"$W/f-digest.out" 2>&1 || true
    gx follower "$WEAVER" store worktree follow --project "$PROJECT" --state "$W/fs-full.json" --full --json >"$W/f-full.json" 2>&1 || true
    if grep -q head_moved "$W/f-digest.out"; then res digest-follower-behind NOTYET 0 "the digest follower already saw the move (the cut did not leave the digest behind)"; else res digest-follower-behind PASS 0 "digest follower sees no head move"; fi
    grep -q head_moved "$W/f-full.json" && res full-follower-sees-move PASS 0 "--full follower sees the head move" || res full-follower-sees-move FAIL 0 "--full follower does not see it"
    printf 'v3\n' >>"$W/wt-r/docs/r.md"; G "$W/wt-r" add -A; G "$W/wt-r" commit -qm v3; ( cd "$W" && gx writer "$S" sweep --only feat--r ) >/dev/null 2>&1
    gx follower "$S" follow --state "$W/fs-digest.json" >"$W/f-digest2.out" 2>&1 || true
    grep -q 'moved' "$W/f-digest2.out" && res next-refresh-heals PASS 0 "after the next refresh the digest follower sees the head move: $(head -1 "$W/f-digest2.out" | cut -c1-100)" || res next-refresh-heals FAIL 0 "digest follower still blind after the next refresh: $(head -1 "$W/f-digest2.out" | cut -c1-100)" ;;
esac
