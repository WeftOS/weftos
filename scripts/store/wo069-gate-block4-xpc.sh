#!/usr/bin/env bash
# WO-069 lane H2, ADR-126 section 4a gate block 4 with the MERGER ON THE SECOND HOST (owner Q3, 2026-11-15 20:42 ET): a claim and merge
# made by the second machine (item 1), a handoff landed by it (item 4, no multi-day thresholds), and the merger killed mid-claim on it
# with a real 30 minute takeover (item 5). Throwaway RustFS only (wo069-gate-remote-rustfs.sh up + mint-keys with WO069_GATE_MINT_MERGER=1).
set -uo pipefail
export GATE_NAME=wo069-gate-block4-xpc
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-block4-xpc.sh setup|kill-start|claim-merge|handoff-land|takeover-check|versions|stop|all [--second-host H] [--dry-run]

  setup           Mac: work dir, a git repo, a writer daemon (writer key) and a taker daemon (a copy of the writer key, agent taker).
                  Second host: two containers from the CI image, each running ONE merger daemon of this branch with its own HOME and
                  runtime dir under $RDIR/<node>, the merger key (one file, read-only) and only the throwaway RustFS as its store:
                  wo069-h2-m1 (items 1 and 4) and wo069-h2-mk (item 5; its store traffic goes through the hold proxy inside it).
  kill-start      item 5: the Mac hands off feat--mk; wo069-h2-mk claims it; the proxy forwards the claim's conditional PUT of the handoff
                  record and withholds the answer; `podman kill wo069-h2-mk` 0.3 s later. Then on the Mac, under caffeinate, the taker
                  claims every 60 s (10 s from 29:00, 2 s from 29:50) until it takes over. Also starts the voided-claim fence of item 1:
                  feat--v1 is claimed by wo069-h2-m1 now and republished by the Mac once the claim is 30 minutes old (same schedule).
  claim-merge     item 1: the Mac hands off feat--c1 (tree + git bundle); m1 claims; a Mac republish under the live claim is refused;
                  m1 fetches, lands with plain git in a clone, promotes with its epoch; the Mac sees merged; the Mac's
                  `store archive --delete` removes the bucket and the registry entry.
  handoff-land    item 4 (handoff part only): the same with feat--l in another project; cleanup by `gc --keep-days 0 --confirmed`.
  takeover-check  after the loops end: takeover timeline, epoch 2; the dead merger restarted (wo069-h2-mk2, same node dir) has its
                  epoch 1 refused and cannot claim; m1's `promote --epoch` on the republished feat--v1 is refused ("claim again").
  versions        `weaver --version` of the four binaries (Mac drill, Mac installed, container, host installed). Read-only.
  stop            kill the PIDs this script recorded (Mac daemons, loops), remove the wo069-h2-* containers and their node dirs; verify.
  all             setup, kill-start, claim-merge, handoff-land, wait for the loops, takeover-check, versions (not stop).
REFUSES without WO069_OWNER_GO_Q3=1. Env: WO069_GATE_WRITER_ENV (0600, Mac), WO069_GATE_STATE (Mac work dir, short: unix sockets),
WEFTOS_E2E_WEAVER (Mac weaver), WO069_GATE_REMOTE_DIR (default ~/ci-jobs/wo069-gate-rfs, holding merger.env, bin/weaver,
src/scripts), WO069_GATE_PROXY_PORT (19433), WO069_GATE_RFS_PORT (19431). Results: RESULT|b4|step|PASS/FAIL|ms|clocks|detail.
EOF
}
CMD=""; HOST="aepod@aepod-xpc"
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        --second-host) HOST=$2; shift 2 ;;
        setup|kill-start|claim-merge|handoff-land|takeover-check|versions|stop|all) CMD=$1; shift ;;
        *) gate_die "unknown option $1 (see --help)" ;;
    esac
done
[ -n "$CMD" ] || { usage >&2; exit 2; }
gate_refuse_pg "second host" "$HOST"
gate_require_go_q3 "a merger daemon with a write key on the second host"
WENV=${WO069_GATE_WRITER_ENV:-}; [ -n "$WENV" ] || gate_die "set WO069_GATE_WRITER_ENV (0600 env file of the throwaway store)"
gate_secret_ok "$WENV"; gate_refuse_pg_env "$WENV"
W=${WO069_GATE_STATE:-}; [ -n "$W" ] || gate_die "set WO069_GATE_STATE (a short absolute dir, e.g. <repo>/target/b4)"
gate_cred_dir_ok "$W"; [ $(( ${#W} + 34 )) -lt 104 ] || gate_die "WO069_GATE_STATE is too long for the daemons' unix sockets (macOS 104 bytes)"
WEAVER=${WEFTOS_E2E_WEAVER:-$GATE_REPO/target/debug/weaver}; S="$GATE_REPO/scripts/worktree-store.sh"
RDIR=${WO069_GATE_REMOTE_DIR:-'~/ci-jobs/wo069-gate-rfs'}; HR=${RDIR/#\~/\$HOME}
PPORT=${WO069_GATE_PROXY_PORT:-19433}; RPORT=${WO069_GATE_RFS_PORT:-19431}; IMG=${WO069_GATE_FH_IMAGE:-localhost/tapestry/build:1.95-ci-wasm}
SSH=(ssh -o BatchMode=yes -o ConnectTimeout=10); M1=wo069-h2-m1; MK=wo069-h2-mk; MK2=wo069-h2-mk2
META="$W/meta.env"; RES="$W/results.tsv"; OUT="$W/out"; GATE_TMP="$W/tmp"
[ "$GATE_DRY" = 1 ] || { mkdir -p "$W" "$OUT" "$GATE_TMP"; chmod 700 "$W" "$GATE_TMP"; touch "$META" "$RES"; }
# shellcheck disable=SC1090
[ "$GATE_DRY" = 1 ] || . "$META"
meta() { [ "$GATE_DRY" = 1 ] || printf '%s=%s\n' "$1" "$2" >>"$META"; printf -v "$1" '%s' "$2"; }
now() { gate_now_ms; }
res() { # <step> <PASS|FAIL|INFO> <ms> <clocks> <detail>
    local l; l=$(printf 'RESULT|b4|%s|%s|%s|%s|%s' "$1" "$2" "$3" "$4" "$5" | tr '\n' ' '); echo "$l"; [ "$GATE_DRY" = 1 ] || echo "$l" >>"$RES"
}
gx() { # <role> <project> cmd...: a throwaway Mac principal (own HOME, runtime dir, state, agent)
    local role=$1 proj=$2; shift 2
    env HOME="$W/home-$role" WEFTOS_RUNTIME_DIR="$W/run-$role" WEFTOS_AGENT="$role" XDG_STATE_HOME="$W/xdg-$role" WTS_STATE_DIR="$W/xdg-$role/wts" \
        WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER="$WEAVER" WTS_PROJECT="$proj" GIT_CONFIG_GLOBAL="$W/gitconfig" GIT_CONFIG_NOSYSTEM=1 \
        AGF_TARGET_REF=master GIT_CEILING_DIRECTORIES="$W" "$@"
}
# xrun <container> <project> <script>: run a bash script INSIDE one of this script's containers on the host (fed on stdin, no quoting
# through ssh). The script's stdout/stderr come back. The last line it prints is "XT <xpc t0 ms> <xpc t1 ms> rc=<n>" (added here).
xrun() {
    local body; body=$(printf 't0=$(date +%%s%%3N)\n( set -e\n%s\n) 2>&1; rc=$?\necho "XT $t0 $(date +%%s%%3N) rc=$rc"\n' "$3")
    if [ "$GATE_DRY" = 1 ]; then printf 'DRY: [%s: podman exec -i -e WTS_PROJECT=%s %s bash -s] <<\n%s\n' "$HOST" "$2" "$1" "$(printf '%s\n' "$3" | sed 's/^/DRY:   | /')"; echo "XT 0 0 rc=0"; return 0; fi
    printf '%s\n' "$body" | "${SSH[@]}" "$HOST" "podman exec -i -e WTS_PROJECT=$2 $1 bash -s"
}
hrun() { # <script>: a bash script on the host itself (container lifecycle only), fed on stdin
    if [ "$GATE_DRY" = 1 ]; then printf 'DRY: [%s: bash -s] <<\n%s\n' "$HOST" "$(printf '%s\n' "$1" | sed 's/^/DRY:   | /')"; return 0; fi
    printf '%s\n' "$1" | "${SSH[@]}" "$HOST" bash -s
}
# xstep <name> <container> <project> <script> [grep regex that the output must match]: timed on both clocks, output in $OUT/<name>.out
xstep() {
    local name=$1 t0 t1 o rc xt; t0=$(now)
    if [ "$GATE_DRY" = 1 ]; then printf 'DRY: xstep %s (output must match /%s/):\n' "$name" "${5:-}"; xrun "$2" "$3" "$4" | grep -v '^XT '; return 0; fi
    o=$(xrun "$2" "$3" "$4"); t1=$(now); printf '%s\n' "$o" >"$OUT/$name.out" 2>/dev/null || true
    xt=$(printf '%s\n' "$o" | grep '^XT ' | tail -1); rc=${xt##*rc=}; rc=${rc:-255}
    if [ "$rc" = 0 ] && { [ -z "${5:-}" ] || printf '%s\n' "$o" | grep -Eq "$5"; }; then
        res "$name" PASS "$((t1 - t0))" "mac=$t0 xpc=$(echo "$xt" | awk '{print $2"-"$3}')" "$(printf '%s\n' "$o" | grep -v '^XT ' | tail -2 | cut -c1-220)"
    else res "$name" FAIL "$((t1 - t0))" "mac=$t0 xpc=$(echo "$xt" | awk '{print $2"-"$3}')" "rc=$rc $(printf '%s\n' "$o" | grep -v '^XT ' | tail -3 | cut -c1-300)"; return 1; fi
}
mstep() { # <name> <cmd string> [regex the output must match]: a timed local step, output in $OUT/<name>.out
    local t0 t1 rc=0; t0=$(now)
    if [ "$GATE_DRY" = 1 ]; then printf 'DRY: step %s: %s\n' "$1" "$2"; return 0; fi
    eval "$2" >"$OUT/$1.out" 2>&1 || rc=$?; t1=$(now)
    if [ $rc = 0 ] && { [ -z "${3:-}" ] || grep -Eq "$3" "$OUT/$1.out"; }; then res "$1" PASS "$((t1 - t0))" "mac=$t0" "$(tail -2 "$OUT/$1.out" | cut -c1-220)"
    else res "$1" FAIL "$((t1 - t0))" "mac=$t0" "rc=$rc $(tail -3 "$OUT/$1.out" | cut -c1-300)"; return 1; fi
}
mrefused() { # <name> <cmd string> <regex the refusal must match>: the command must FAIL with that text
    local t0 t1 rc=0; t0=$(now)
    if [ "$GATE_DRY" = 1 ]; then printf 'DRY: step %s (must be refused): %s\n' "$1" "$2"; return 0; fi
    eval "$2" >"$OUT/$1.out" 2>&1 || rc=$?; t1=$(now)
    if [ $rc != 0 ] && grep -Eqi "$3" "$OUT/$1.out"; then res "$1" PASS "$((t1 - t0))" "mac=$t0" "refused rc=$rc: $(grep -Ei "$3" "$OUT/$1.out" | head -1 | cut -c1-260)"
    else res "$1" FAIL "$((t1 - t0))" "mac=$t0" "rc=$rc (want a refusal matching /$3/): $(tail -2 "$OUT/$1.out" | cut -c1-260)"; return 1; fi
}
epoch_of() { sed -n 's/.*"epoch"[[:space:]]*:[[:space:]]*\([0-9][0-9]*\).*/\1/p' "$1" 2>/dev/null | head -1; }
browse_field() { # <project> <slug> <jq field>: from the Mac writer's fresh registry view; "-" when the entry is absent
    gx writer "$1" "$S" browse --json --fresh 2>/dev/null | jq -r --arg s "$2" "[.worktrees[] | select(.slug == \$s)][0] | if . == null then \"-\" else ($3 | tostring) end"
}
bucket_of() { gx writer "$1" "$WEAVER" store worktree buckets --project "$1" --slug "$2" 2>/dev/null | awk '{print $1; exit}'; }   # prints "<worktree bucket> <project bucket>"
bucket_gone() { local c; c=$(GATE_OUT=/dev/null gate_s3raw "$WENV" GET "/$1?list-type=2&max-keys=1"); [ "$c" = 404 ]; }

mac_daemon_up() { # <role>: nohup'd user daemon on a throwaway runtime dir with the writer key; PID kept in meta
    local role=$1 pid
    mkdir -p "$W/home-$role/.weftos" "$W/run-$role"; printf '[kernel.governance]\noutside_project = "allow_all"\n' >"$W/home-$role/.weftos/weave.toml"; rm -f "$W/run-$role/kernel.lock"
    nohup env HOME="$W/home-$role" WEFTOS_RUNTIME_DIR="$W/run-$role" WEFTOS_STORE_RUSTFS_ENV="$WENV" "$WEAVER" kernel start --foreground --profile user >>"$W/daemon-$role.out" 2>&1 </dev/null &
    pid=$!; meta "PID_$role" "$pid"
    for _ in $(seq 1 120); do gx "$role" x "$WEAVER" store ls >/dev/null 2>&1 && return 0; kill -0 "$pid" 2>/dev/null || return 1; sleep 0.5; done; return 1
}
# node_up <container> <node dir> <hostname> <proxy 0|1>: one merger daemon in a container on the host (see usage)
node_up() {
    local envf=/key/merger.env proxy=""; [ "$4" = 0 ] || envf=/node/proxy.env
    [ "$4" = 0 ] || proxy="umask 077; sed 's#^MESH_S3_ENDPOINT=.*#MESH_S3_ENDPOINT=http://127.0.0.1:$PPORT#' \$R/merger.env >\$ND/proxy.env; umask 022
! ss -ltn | grep -q ':$PPORT ' || { echo 'proxy port $PPORT in use'; exit 1; }"
    hrun "set -e; R=$HR; ND=\$R/$2; N=$1
! podman container exists \$N || { echo \"container \$N already exists\"; exit 1; }
test -s \$R/merger.env && [ \"\$(stat -c %a \$R/merger.env)\" = 600 ] || { echo 'no 0600 merger.env (mint-keys with WO069_GATE_MINT_MERGER=1)'; exit 1; }
mkdir -p \$ND/home/.weftos \$ND/run \$ND/xdg \$ND/ctl; rm -f \$ND/run/kernel.lock \$ND/ctl/hold \$ND/ctl/held
printf '[kernel.governance]\\noutside_project = \"allow_all\"\\n' >\$ND/home/.weftos/weave.toml
printf '[user]\\n\\tname = merger\\n\\temail = merger@example.invalid\\n[init]\\n\\tdefaultBranch = master\\n' >\$ND/gitconfig
$proxy
podman run -d --pull=never --name \$N --hostname $3 --network host --userns=keep-id -e HOME=/node/home -e WEFTOS_RUNTIME_DIR=/node/run \\
  -e WEFTOS_AGENT=merger -e WEFTOS_STORE_RUSTFS_ENV=$envf -e XDG_STATE_HOME=/node/xdg -e WEAVER_PROFILE=user -e WTS_STRICT=1 \\
  -e WTS_WEAVER=/opt/w/weaver -e GIT_CEILING_DIRECTORIES=/node -e GIT_CONFIG_GLOBAL=/node/gitconfig -e GIT_CONFIG_NOSYSTEM=1 -e AGF_TARGET_REF=master \\
  -v \$ND:/node -v \$R/merger.env:/key/merger.env:ro -v \$R/bin/weaver:/opt/w/weaver:ro -v \$R/src/scripts:/opt/w/scripts:ro \\
  -v \$R/bin/hold-proxy.py:/opt/w/hold-proxy.py:ro -v \$R/bin/base.bundle:/opt/w/base.bundle:ro -w /node $IMG sleep infinity >/dev/null
$([ "$4" = 0 ] || echo "podman exec -d \$N python3 -I /opt/w/hold-proxy.py $PPORT 127.0.0.1 $RPORT /node/ctl
for i in \$(seq 1 50); do ss -ltn | grep -q '127.0.0.1:$PPORT ' && break; sleep 0.2; done; ss -ltn | grep -q '127.0.0.1:$PPORT ' || { echo 'proxy did not listen'; exit 1; }")
podman exec -d \$N sh -c 'exec /opt/w/weaver kernel start --foreground --profile user >>/node/daemon.out 2>&1'
for i in \$(seq 1 120); do podman exec \$N /opt/w/weaver store ls >/dev/null 2>&1 && { echo \"\$N daemon up\"; exit 0; }; sleep 0.5; done
echo \"\$N daemon did not come up\"; tail -5 \$ND/daemon.out; exit 1"
}
# new_wt <x> <project>: worktree feat/<x> of the drill repo (store-create + register at the base), then a doc and a code file, one commit
new_wt() {
    local d="$W/wt-$1"; git -C "$W/repo" worktree add -q -b "feat/$1" "$d" master && ( cd "$d" && gx writer "$2" "$S" store-create && gx writer "$2" "$S" register ) \
        && mkdir -p "$d/docs" && printf '# Gate %s\n\nbody %s\n' "$1" "$(head -c 8 /dev/urandom | od -An -tx1 | tr -d ' \n')" >"$d/docs/$1.md" \
        && printf 'pub fn f_%s() {}\n' "$1" >"$d/lib.rs" && git -C "$d" add -A && GIT_CONFIG_GLOBAL="$W/gitconfig" git -C "$d" commit -qm "work $1"
}
handoff() { ( cd "$W/wt-$1" && gx writer "$2" "$S" share --handoff "$3" ); }   # <x> <project> <note>
land_script() { # <slug> <branch>: fetch tree+bundle into one clone, land with plain git in another, print the landed head and the doc sha
    printf 'cd /node; rm -rf f-%s l-%s\ngit clone -q -b master /opt/w/base.bundle f-%s\ngit clone -q -b master /opt/w/base.bundle l-%s\n' "$1" "$1" "$1" "$1"
    printf 'bash /opt/w/scripts/worktree-store.sh fetch %s /node/f-%s\n' "$1" "$1"
    printf 'git -C l-%s fetch -q /node/f-%s/.wt-branch.bundle "+refs/heads/*:refs/shared/%s/*"\ngit -C l-%s merge -q --ff-only refs/shared/%s/%s\n' "$1" "$1" "$1" "$1" "$1" "$2"
    printf 'echo "LANDED_HEAD $(git -C l-%s rev-parse HEAD)"; echo "DOC_SHA $(sha256sum f-%s/docs/%s.md | cut -d" " -f1)"\n' "$1" "$1" "${1#feat--}"
}

do_setup() {
    gate_say "== setup: Mac work dir $W, second host $HOST ($RDIR)"
    [ "$GATE_DRY" = 1 ] || [ -x "$WEAVER" ] || gate_die "no weaver at $WEAVER"
    if [ "$GATE_DRY" = 1 ]; then gate_note "git repo $W/repo (master, one commit), base bundle; Mac daemons writer and taker (writer key) via nohup; PIDs in $META"
    else
        [ -z "${PID_writer:-}" ] || gate_die "$W already has a setup (meta.env): run stop, or use another WO069_GATE_STATE"
        printf '[user]\n\tname = gate\n\temail = gate@example.invalid\n[init]\n\tdefaultBranch = master\n' >"$W/gitconfig"
        git init -q -b master "$W/repo" && echo base >"$W/repo/README.md" && git -C "$W/repo" add . && GIT_CONFIG_GLOBAL="$W/gitconfig" git -C "$W/repo" commit -qm base
        git -C "$W/repo" bundle create -q "$W/base.bundle" master
        for p in P1 P4 P5; do meta "$p" "01$(python3 -c "import random; print(''.join(random.choice('0123456789ABCDEFGHJKMNPQRSTVWXYZ') for _ in range(24)))")"; done
        mstep "mac-writer-daemon-up" 'mac_daemon_up writer'
        mstep "mac-taker-daemon-up" 'mac_daemon_up taker'
    fi
    gate_run scp -q -o BatchMode=yes "$GATE_REPO/scripts/store/wo069-gate-hold-proxy.py" "$HOST:${RDIR#\~/}/bin/hold-proxy.py"
    gate_run scp -q -o BatchMode=yes "$W/base.bundle" "$HOST:${RDIR#\~/}/bin/base.bundle"
    if [ "$GATE_DRY" = 1 ]; then node_up "$M1" m1 xpc-m1 0; node_up "$MK" mk xpc-mk 1
    else mstep "xpc-merger-m1-up" 'node_up "$M1" m1 xpc-m1 0' 'daemon up'; mstep "xpc-merger-mk-up-behind-proxy" 'node_up "$MK" mk xpc-mk 1' 'daemon up'; fi
}

# loop_script <name> <cmd> <ok regex>: a Mac loop, one attempt at once, then every 60 s, every 10 s from 29:00, every 2 s from 29:50, until an
# attempt succeeds (rc 0 and the regex) or 40 min pass. Lines: "<mac ms> <elapsed s since the first attempt> rc=<n> <output head>".
loop_script() {
    cat <<LOOP
#!/usr/bin/env bash
t0=\$(python3 -c 'import time; print(int(time.time()*1000))')
while :; do
    t=\$(python3 -c 'import time; print(int(time.time()*1000))'); el=\$(( (t - t0) / 1000 ))
    out=\$( $2 2>&1 ); rc=\$?
    echo "\$t \$el rc=\$rc \$(printf '%s' "\$out" | tr '\\n' ' ' | cut -c1-400)" >>'$W/$1.log'
    if [ \$rc = 0 ] && printf '%s' "\$out" | grep -Eq '$3'; then break; fi
    [ \$el -lt 2400 ] || break
    if [ \$el -lt 1740 ]; then sleep 60; elif [ \$el -lt 1790 ]; then sleep 10; else sleep 2; fi
done
echo done >'$W/$1.done'
LOOP
}
do_kill_start() {
    gate_say "== kill-start (item 5 merger kill; item 1 voided-claim fence)"
    if [ "$GATE_DRY" = 0 ]; then
        mstep "mk-handoff-on-mac" 'new_wt mk "$P5" && handoff mk "$P5" "claim me (merger kill)"' || return 1
        mstep "v1-handoff-on-mac" 'new_wt v1 "$P1" && handoff v1 "$P1" "claim me (voided-claim fence)"' || return 1
        meta V1_HEAD "$(git -C "$W/wt-v1" rev-parse HEAD)"; meta MK_HEAD "$(git -C "$W/wt-mk" rev-parse HEAD)"
    else gate_note "Mac: worktrees feat/mk (project P5) and feat/v1 (P1), each store-create, register, share --handoff"; fi
    xstep "v1-claim-by-xpc-m1" "$M1" "${P1:-P1}" 'bash /opt/w/scripts/worktree-store.sh claim feat--v1' '"epoch"' && meta EV1 "$(epoch_of "$OUT/v1-claim-by-xpc-m1.out")"
    [ "$GATE_DRY" = 1 ] || meta V1_CLAIM_MAC_MS "$(now)"
    # arm the hold, start the host-side killer (it waits for the proxy's "held", sleeps 0.3 s, kills the container), then the claim
    hrun "echo 'hold handoff.json' >$HR/mk/ctl/hold"
    local killer="for i in \$(seq 1 1200); do [ -f $HR/mk/ctl/held ] && break; sleep 0.05; done; [ -f $HR/mk/ctl/held ] || { echo NOHOLD; exit 1; }
sleep 0.3; k0=\$(date +%s%3N); podman kill $MK >/dev/null; echo \"HELD \$(cat $HR/mk/ctl/held) KILL \$k0 \$(date +%s%3N)\"; podman ps -a --filter name=^$MK\$ --format '{{.Names}} {{.Status}}'"
    if [ "$GATE_DRY" = 1 ]; then hrun "$killer"; xrun "$MK" P5 'bash /opt/w/scripts/worktree-store.sh claim feat--mk'
    else
        printf '%s\n' "$killer" | "${SSH[@]}" "$HOST" bash -s >"$OUT/mk-killer.out" 2>&1 & local kp=$!
        sleep 1; local c0; c0=$(now)
        xrun "$MK" "$P5" 'bash /opt/w/scripts/worktree-store.sh claim feat--mk' >"$OUT/mk-claim.out" 2>&1 & local cp=$!
        wait "$kp"; local krc=$?; wait "$cp" 2>/dev/null; meta MK_KILL_MAC_MS "$(now)"
        if [ $krc = 0 ] && grep -q '^HELD ' "$OUT/mk-killer.out"; then res merger-killed-mid-claim PASS 0 "mac=$c0 $(grep '^HELD' "$OUT/mk-killer.out" | awk '{print "xpc held="$2" kill="$4"-"$5}')" "claim PUT forwarded, answer withheld, container killed 0.3 s later: $(tail -1 "$OUT/mk-killer.out"); client: $(grep -v '^XT' "$OUT/mk-claim.out" | head -c 160 | tr '\n' ' ')"
        else res merger-killed-mid-claim FAIL 0 "mac=$c0" "killer rc=$krc: $(tr '\n' ' ' <"$OUT/mk-killer.out" | cut -c1-200)"; return 1; fi
        local wb; wb=$(bucket_of "$P5" feat--mk); meta MK_BUCKET "$wb"
        GATE_OUT="$OUT/mk-handoff-after-kill.json" gate_s3raw "$WENV" GET "/$wb/.wt/handoff.json" >/dev/null
        if jq -e '.state == "claimed" and .epoch == 1 and (.merger.host == "xpc-mk")' "$OUT/mk-handoff-after-kill.json" >/dev/null 2>&1; then
            res claim-on-store-after-kill PASS 0 "mac=$(now)" "handoff record: $(jq -c '{state, epoch, merger, claimed_head, updated_ms}' "$OUT/mk-handoff-after-kill.json")"
        else res claim-on-store-after-kill FAIL 0 "mac=$(now)" "$(head -c 300 "$OUT/mk-handoff-after-kill.json" | tr '\n' ' ')"; fi
        "${SSH[@]}" "$HOST" "cat $HR/mk/ctl/requests.log" >"$OUT/mk-proxy-requests.log" 2>&1 || true
    fi
    local tk="env HOME='$W/home-taker' WEFTOS_RUNTIME_DIR='$W/run-taker' WEFTOS_AGENT=taker XDG_STATE_HOME='$W/xdg-taker' WTS_STATE_DIR='$W/xdg-taker/wts' WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER='$WEAVER' WTS_PROJECT='${P5:-P5}' GIT_CONFIG_GLOBAL='$W/gitconfig' GIT_CEILING_DIRECTORIES='$W' '$S' claim feat--mk"
    local vr="cd '$W/wt-v1' && env HOME='$W/home-writer' WEFTOS_RUNTIME_DIR='$W/run-writer' WEFTOS_AGENT=writer XDG_STATE_HOME='$W/xdg-writer' WTS_STATE_DIR='$W/xdg-writer/wts' WEAVER_PROFILE=user WTS_STRICT=1 WTS_WEAVER='$WEAVER' WTS_PROJECT='${P1:-P1}' GIT_CONFIG_GLOBAL='$W/gitconfig' GIT_CEILING_DIRECTORIES='$W' AGF_TARGET_REF=master '$S' share"
    if [ "$GATE_DRY" = 1 ]; then gate_note "taker loop (caffeinate -ims, nohup): $tk; until rc 0 and an epoch"; gate_note "void loop: wait until the v1 claim is 29 min old, commit once, then: $vr; until rc 0"; return 0; fi
    loop_script taker "$tk" '"epoch"' >"$W/taker-loop.sh"
    # the void loop waits until 29:00 after the claim, makes one commit, then republishes on the same schedule (refused while the claim lives)
    { printf '#!/usr/bin/env bash\nsleep %s\nprintf "late\\n" >>"%s/wt-v1/docs/v1.md"; git -C "%s/wt-v1" add -A; GIT_CONFIG_GLOBAL="%s/gitconfig" git -C "%s/wt-v1" commit -qm late\n' \
        "$(( 1740 - ( $(now) - V1_CLAIM_MAC_MS ) / 1000 ))" "$W" "$W" "$W" "$W"; loop_script void "$vr" '.' | tail -n +2 | sed 's/el -lt 1740/el -lt 0/; s/el -lt 1790/el -lt 50/'; } >"$W/void-loop.sh"
    : >"$W/taker.log"; : >"$W/void.log"; rm -f "$W/taker.done" "$W/void.done"
    nohup caffeinate -ims bash "$W/taker-loop.sh" >/dev/null 2>&1 </dev/null & meta PID_takerloop "$!"
    nohup caffeinate -ims bash "$W/void-loop.sh" >/dev/null 2>&1 </dev/null & meta PID_voidloop "$!"
    gate_say "taker loop pid ${PID_takerloop:-?}, void loop pid ${PID_voidloop:-?}; both end by themselves (takeover / republish, or 40 min)"
}

# the claim-land-promote flow by m1 for <x> in <project>; <cleanup> is archive or gc
merge_flow() {
    local x=$1 p=$2 how=$3 slug="feat--$1" h e wb st
    mstep "$x-handoff-on-mac" 'new_wt "$x" "$p" && handoff "$x" "$p" "land me on the second host"' || return 1
    h=HEAD; wb=BUCKET; [ "$GATE_DRY" = 1 ] || { h=$(git -C "$W/wt-$x" rev-parse HEAD); wb=$(bucket_of "$p" "$slug"); }
    xstep "$x-claim-by-xpc-m1" "$M1" "$p" "bash /opt/w/scripts/worktree-store.sh claim $slug" '"epoch"' || return 1
    e=$(epoch_of "$OUT/$x-claim-by-xpc-m1.out"); e=${e:-1}
    mstep "$x-mac-sees-claim" '[ "$(browse_field "$p" "$slug" .state)/$(browse_field "$p" "$slug" .merger)" = "claimed/merger@xpc-m1" ] && echo "state claimed by merger@xpc-m1, older_writer=$(browse_field "$p" "$slug" .older_writer)"'
    if [ "$x" = c1 ]; then
        mrefused "c1-mac-republish-under-live-claim-refused" '( cd "$W/wt-c1" && printf "more\n" >>docs/c1.md && git add -A && git commit -qm more && gx writer "$p" "$S" share )' 'claim|merger@xpc-m1'
        mstep "c1-head-unchanged-after-refusal" '[ "$(browse_field "$p" "$slug" .head)" = "$h" ] && echo "registry head still $h"'
    fi
    xstep "$x-fetch-and-git-land-on-xpc" "$M1" "$p" "$(land_script "$slug" "feat/$x")" "LANDED_HEAD $h"
    [ "$GATE_DRY" = 1 ] || { st=$(sed -n 's/^DOC_SHA //p' "$OUT/$x-fetch-and-git-land-on-xpc.out"); [ "$st" = "$(git -C "$W/wt-$x" show "$h:docs/$x.md" | gate_sha256 /dev/stdin)" ] && res "$x-fetched-doc-byte-equal" PASS 0 "" "sha256 $st" || res "$x-fetched-doc-byte-equal" FAIL 0 "" "fetched $st"; }
    xstep "$x-promote-with-epoch-on-xpc" "$M1" "$p" "/opt/w/weaver store worktree promote --project $p --slug $slug --epoch $e" '"promoted"'
    mstep "$x-mac-sees-merged" 'for i in $(seq 1 30); do [ "$(browse_field "$p" "$slug" .state)" = merged ] && break; sleep 1; done; [ "$(browse_field "$p" "$slug" .state)" = merged ] && echo "merged after ${i} s of polling, merger $(browse_field "$p" "$slug" .merger), older_writer=$(browse_field "$p" "$slug" .older_writer)"'
    if [ "$how" = archive ]; then
        mstep "$x-cleanup-store-archive-delete" 'gx writer "$p" "$WEAVER" store archive "weftos://local/projects/$p/worktrees/$slug/store" --delete'
    else
        mstep "$x-cleanup-gc-keep-days-0-confirmed" 'gx writer "$p" "$WEAVER" store worktree gc --project "$p" --keep-days 0 --confirmed | tee /dev/stderr | jq -e --arg s "$slug" "[.retired[] | select(contains(\$s))] | length == 1"'
    fi
    mstep "$x-registry-entry-gone" '[ "$(browse_field "$p" "$slug" .state)" = - ] && echo "no registry entry for $slug"'
    mstep "$x-bucket-gone" 'bucket_gone "$wb" && echo "bucket $wb: 404"'
    [ "$x" != l ] || mstep "l-not-in-leftovers" '! gx writer "$p" "$S" leftovers | grep -q "$slug" && echo "leftovers does not list $slug"'
}
do_claim_merge() { gate_say "== claim-merge (item 1, feat--c1 in P1)"; merge_flow c1 "${P1:-P1}" archive; }
do_handoff_land() { gate_say "== handoff-land (item 4 handoff part, feat--l in P4)"; merge_flow l "${P4:-P4}" gc; }

do_takeover_check() {
    gate_say "== takeover-check"
    if [ "$GATE_DRY" = 0 ]; then
        [ -f "$W/taker.done" ] && [ -f "$W/void.done" ] || gate_die "the loops are still running (taker.done / void.done missing)"
        local first last ok_el nref last_ref ep
        first=$(awk 'NR==1{print $1}' "$W/taker.log"); last=$(tail -1 "$W/taker.log"); ok_el=$(echo "$last" | awk '{print $2}')
        nref=$(grep -c 'rc=[1-9]' "$W/taker.log"); last_ref=$(grep 'rc=[1-9]' "$W/taker.log" | tail -1 | awk '{print $2}')
        ep=$(echo "$last" | sed -n 's/.*"epoch"[[:space:]]*:[[:space:]]*\([0-9]*\).*/\1/p')
        if echo "$last" | grep -q 'rc=0' && [ "$ok_el" -ge 1798 ] && [ "${last_ref:-0}" -lt 1802 ]; then
            res merger-takeover-after-30-min PASS "$((ok_el * 1000))" "mac first=$first taken=$(echo "$last" | awk '{print $1}')" "$nref refusals, last at ${last_ref}s; takeover at ${ok_el}s of the taker's observation; $(echo "$last" | cut -d' ' -f4- | cut -c1-200)"
        else res merger-takeover-after-30-min FAIL "$((ok_el * 1000))" "" "last refusal ${last_ref}s, success line: $(echo "$last" | cut -c1-200)"; fi
        [ "$ep" = 2 ] && echo "$last" | grep -q 'taker@' && res takeover-epoch-2 PASS 0 "" "epoch 2 by the Mac taker" || res takeover-epoch-2 FAIL 0 "" "epoch '$ep'"
        hrun "podman rm -f -v $MK >/dev/null 2>&1; ! podman container exists $MK && echo '$MK removed (node dir kept for the restart)'"
    fi
    mstep "dead-merger-restarted-on-xpc" 'node_up "$MK2" mk xpc-mk 0' 'daemon up'
    xstep "dead-merger-epoch-1-refused" "$MK2" "${P5:-P5}" "! /opt/w/weaver store worktree promote --project ${P5:-P5} --slug feat--mk --epoch 1" 'stale|taken over'
    xstep "dead-merger-cannot-claim" "$MK2" "${P5:-P5}" "! bash /opt/w/scripts/worktree-store.sh claim feat--mk" 'taker@|laimed'
    if [ "$GATE_DRY" = 0 ]; then
        local vok vfirst; vok=$(grep -m1 ' rc=0 ' "$W/void.log" | awk '{print $1}'); vfirst=$(awk 'NR==1{print $1}' "$W/void.log")
        res v1-republish-refused-then-allowed "$([ -n "$vok" ] && echo PASS || echo FAIL)" 0 "claim mac=$V1_CLAIM_MAC_MS" "$(grep -c 'rc=[1-9]' "$W/void.log") refused from $(( (vfirst - V1_CLAIM_MAC_MS) / 1000 ))s, accepted at $(( (${vok:-0} - V1_CLAIM_MAC_MS) / 1000 ))s after the claim (Mac clock); last refusal: $(grep 'rc=[1-9]' "$W/void.log" | tail -1 | cut -d' ' -f4- | cut -c1-160)"
    fi
    xstep "v1-promote-after-void-refused" "$M1" "${P1:-P1}" "! /opt/w/weaver store worktree promote --project ${P1:-P1} --slug feat--v1 --epoch ${EV1:-1}" 'claim again|claim it again'
}
do_versions() {
    local prod
    # shellcheck disable=SC2034  # read inside the eval strings of mstep
    prod=$(ps -o command= -ax 2>/dev/null | awk '$2=="kernel" && $3=="start" && /--profile user/ {print $1}' | grep -v "$WEAVER" | head -1)
    mstep "version-mac-drill-daemon" '"$WEAVER" --version'
    mstep "version-mac-installed" '"${prod:-weaver}" --version; echo "binary of the running production daemon: ${prod:-not found}"'
    xstep "version-xpc-container" "$M1" x '/opt/w/weaver --version'
    mstep "version-xpc-installed" '"${SSH[@]}" "$HOST" "/usr/local/bin/weaver --version"'
}
do_stop() {
    gate_say "== stop: recorded PIDs and the wo069-h2-* containers"
    local v p
    if [ "$GATE_DRY" = 0 ]; then for v in PID_takerloop PID_voidloop PID_taker PID_writer; do p=${!v:-}; [ -z "$p" ] || { gate_kill_tree "$p"; echo "killed $v=$p"; }; done; fi
    hrun "R=$HR; podman rm -f -v $M1 $MK $MK2 >/dev/null 2>&1; rm -rf \$R/m1 \$R/mk; for n in $M1 $MK $MK2; do ! podman container exists \$n || { echo \"STILL \$n\"; exit 1; }; done; ! test -e \$R/m1 && ! test -e \$R/mk && ! ss -ltn | grep -q ':$PPORT ' && echo 'containers, node dirs and the proxy port gone'"
    [ "$GATE_DRY" = 1 ] || { for v in PID_takerloop PID_voidloop PID_taker PID_writer; do p=${!v:-}; [ -z "$p" ] || ! kill -0 "$p" 2>/dev/null || echo "STILL RUNNING $v=$p"; done; }
}
case "$CMD" in
setup) do_setup ;; kill-start) do_kill_start ;; claim-merge) do_claim_merge ;; handoff-land) do_handoff_land ;;
takeover-check) do_takeover_check ;; versions) do_versions ;; stop) do_stop ;;
all) do_setup && do_kill_start; do_claim_merge; do_handoff_land
     [ "$GATE_DRY" = 1 ] || until [ -f "$W/taker.done" ] && [ -f "$W/void.done" ]; do sleep 15; done
     do_takeover_check; do_versions ;;
esac
[ "$GATE_DRY" = 1 ] && gate_say "dry run complete: nothing was run on either host." || true
