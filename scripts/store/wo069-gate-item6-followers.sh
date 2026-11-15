#!/usr/bin/env bash
# WO-069 lane F2, ADR-126 section 4a gate item 6: the follower drills that the injected-clock library drills (lane Q) did not run for real.
set -euo pipefail
export GATE_NAME=wo069-gate-item6
. "$(dirname "${BASH_SOURCE[0]}")/wo069-gate-lib.sh"

usage() { cat <<'EOF'
usage: wo069-gate-item6-followers.sh [--only a,b,c,d,e,f,g] [--dry-run]
       WO069_GATE_WRITER_ENV=<0600 env file of a THROWAWAY store> wo069-gate-item6-followers.sh

Runs against the real weaver daemon and a real store, on the real clocks of this machine and the store host. Default: a,b,c,d,e,f,g.
  a  writer a day ahead, by STAMP: the registry entry's updated_ms is rewritten +24 h through raw S3 (no daemon seam for a clock exists,
     and no system clock is changed). Real follower clock, real store, real `leftovers`/`follow`. This is NOT an independent writer clock:
     the writer-side behaviours that read the writer's own clock (claim stamp, orphan stamp) are not exercised. `a2` (a writer process
     under libfaketime) is refused: it needs a store-capable weaver in a Linux container (owner question 3) AND libfaketime in the image
     (a second decision); without both, "a writer clock a day ahead" cannot be done here without changing a system clock.
  b  a corrupt registry entry, an entry whose slug does not match its key, and a digest of unknown schema (99): `follow`, `browse` and
     `leftovers` must report, not die, and keep listing the healthy worktree
  c  an empty project bucket after worktrees were known (every entry deleted) and a follower pointed at a wrong project ULID
  d  a merged entry whose cleanup never ran, stamped 23 h and 25 h ago on the real clock: stuck_cleanup only at 25 h
  e  an interrupted sync-docs (the daemon killed with kill -9 mid-mirror) followed by a deleted doc and a second sync-docs
  f  `overlaps` on two real worktrees of THIS repo's docs and scripts with different bases: reported versus the true conflict set
     (git merge-tree), so the false-positive share is measured on real history. WO069_GATE_GAP (commits between the bases, default 200),
     WO069_GATE_PATHS (sparse paths, default "docs/adr scripts/store")
  g  two to four `follow` runs sharing one --state file, concurrently, while the writer publishes: the file always parses
Environment: WO069_GATE_WRITER_ENV (required), WEFTOS_E2E_WEAVER (default target/debug/weaver), KEEP=1. Results: RESULT|item6|... lines.
EOF
}
ONLY="a,b,c,d,e,f,g"
while [ $# -gt 0 ]; do
    case "$1" in
        -h|--help) usage; exit 0 ;;
        --dry-run) GATE_DRY=1; shift ;;
        --only) ONLY=$2; shift 2 ;;
        *) gate_die "unknown option $1 (see --help)" ;;
    esac
done
want() { case ",$ONLY," in *",$1,"*) return 0 ;; esac; return 1; }
if want a2; then gate_require_go_q3 "a2 (a writer daemon under libfaketime in a container on aepod-xpc)"; gate_die "a2 is not implemented in this lane: it also needs libfaketime in the CI image (a second owner decision). Until then a writer clock a day ahead needs a system clock change, which this plan never does."; fi
WENV=${WO069_GATE_WRITER_ENV:-}; [ -n "$WENV" ] || gate_die "set WO069_GATE_WRITER_ENV (0600 env file of a throwaway store)"
gate_secret_ok "$WENV"; gate_refuse_pg_env "$WENV"
WEAVER=${WEFTOS_E2E_WEAVER:-$GATE_REPO/target/debug/weaver}; S="$GATE_REPO/scripts/worktree-store.sh"
if [ "$GATE_DRY" = 1 ]; then
    gate_say "== item 6 (dry run), drills $ONLY"
    gate_note "work dir under target/wo069-gate-item6.*; writer daemon and follower daemon on throwaway HOME/runtime dirs; project, two worktrees, publish"
    want a && gate_note "a: GET worktrees/<slug>.json; jq .updated_ms += 86400000; PUT; follow and leftovers (expect a future-timestamp report); restore the original"
    want b && gate_note "b: PUT worktrees/zz-corrupt.json '{broken'; PUT a mislabelled entry; PUT worktrees/_digest.json {schema:99}; follow, browse --json, leftovers; delete the forged keys"
    want c && gate_note "c: follow once; DELETE every worktrees/*.json and the digest; follow must be an error (wrong bucket?), not mass-gone; follow with WTS_PROJECT=<other ULID>"
    want d && gate_note "d: PUT a merged entry stamped now-23h then now-25h (raw S3, real clock); leftovers lists stuck_cleanup only at 25 h"
    want e && gate_note "e: publish 600 docs; sync-docs in the background; kill -9 the daemon at 1.5 s; restart; delete a doc, commit, sweep; sync-docs; the deleted doc must not be in the mirror"
    want f && gate_note "f: local clone of this repo, sparse paths, two branches at HEAD~GAP and HEAD each editing the same N files plus M own files; register, sweep both; overlaps A B; truth from git merge-tree"
    want g && gate_note "g: ten rounds of three concurrent 'follow --state FILE' with a publish between rounds; jq . FILE must parse after every round"
    gate_note "stop both daemons, remove the work dir (KEEP=1 keeps it); delete every forged object"
    exit 0
fi

[ -x "$WEAVER" ] || gate_die "no weaver at $WEAVER"
mkdir -p "$GATE_REPO/target"; W=$(mktemp -d "$GATE_REPO/target/wo069-gate-item6.XXXXXX"); GATE_TMP="$W/tmp"; mkdir -p "$GATE_TMP"; gate_gitconfig "$W"
PROJECT=01$(python3 -c "import random; print(''.join(random.choice('0123456789ABCDEFGHJKMNPQRSTVWXYZ') for _ in range(24)))")
PB="p-$(printf '%s' "$PROJECT" | tr 'A-Z' 'a-z')"; PIDS=(); FAILS=0
gx() { gate_gx "$W" "$1" "$PROJECT" "$WEAVER" "${@:2}"; }
G() { git -C "$1" "${@:2}"; }
res() { printf 'RESULT|item6|%s|%s|%s|%s\n' "$1" "$2" "$3" "$4" | tee -a "$W/results.log"; [ "$2" != FAIL ] || FAILS=$((FAILS + 1)); }
cleanup() { trap - EXIT INT TERM; local p; for p in ${PIDS[@]+"${PIDS[@]}"}; do gate_kill_tree "$p"; done; if [ -n "${KEEP:-}" ]; then echo "KEEP set: $W"; else rm -rf "$W"; fi; }
trap cleanup EXIT INT TERM
up() { gate_daemon_up "$W" "$1" "$PROJECT" "$WEAVER" "$WENV" || gate_die "daemon $1 did not come up ($W/daemon-$1.out)"; PIDS+=("$GATE_PID"); LASTPID=$GATE_PID; }
raw() { GATE_OUT=${OUT:-/dev/null} gate_s3raw "$WENV" "$@"; }   # <METHOD> </bucket/key> [body]
entry_get() { OUT="$W/entry.json" raw GET "/$PB/worktrees/$1.json" >/dev/null; }
now_ms() { gate_now_ms; }
newwt() { # <letter> <title>
    G "$W/repo" worktree add -q -b "feat/$1" "$W/wt-$1" master; mkdir -p "$W/wt-$1/docs"; printf '# %s\n\nbody\n' "$2" >"$W/wt-$1/docs/$1.md"
    ( cd "$W/wt-$1" && gx writer "$S" store-create && gx writer "$S" register ) >/dev/null
}
git init -q -b master "$W/repo"; echo base >"$W/repo/README.md"; G "$W/repo" add .; G "$W/repo" commit -qm base
up writer; WP=$LASTPID; up follower
PN=$(gx writer "$WEAVER" store ls --json 2>/dev/null | grep -o "weftos://[^\"' ]*projects/$PROJECT/store" | head -1)
newwt h "Healthy One"; PN=$(gx writer "$WEAVER" store ls --json 2>/dev/null | grep -o "weftos://[^\"' ]*projects/$PROJECT/store" | head -1)
gx writer "$WEAVER" store grant "$PN" agent:follower read >/dev/null 2>&1 || true
gx writer "$S" sweep >/dev/null 2>&1; gx follower "$S" follow >/dev/null 2>&1 || true

if want a; then
    entry_get feat--h; cp "$W/entry.json" "$W/entry.orig"
    jq '.updated_ms += 86400000' "$W/entry.orig" >"$W/entry.fwd"; raw PUT "/$PB/worktrees/feat--h.json" "$W/entry.fwd" >/dev/null
    out=$(gx follower "$S" leftovers 2>&1 || true)
    echo "$out" | grep -qiE 'future|skew|clock' && res a-writer-stamp-24h-ahead PASS 0 "leftovers reports the future stamp (writer clock ahead): $(echo "$out" | head -2 | tr '\n' ' ' | cut -c1-120)" || res a-writer-stamp-24h-ahead FAIL 0 "leftovers silent about a 24 h future stamp: $(echo "$out" | head -2 | tr '\n' ' ' | cut -c1-120)"
    gx follower "$S" follow >"$W/a-follow.out" 2>&1 && res a-follow-survives PASS 0 "follow exits 0 with the stamp ahead" || res a-follow-survives FAIL 0 "follow failed: $(head -c 120 "$W/a-follow.out")"
    raw PUT "/$PB/worktrees/feat--h.json" "$W/entry.orig" >/dev/null; res a-restored PASS 0 "original entry put back"
fi
if want b; then
    printf '{broken' >"$W/bad.json"; raw PUT "/$PB/worktrees/zz-corrupt.json" "$W/bad.json" >/dev/null
    jq '.slug = "someone-else"' "$W/entry.orig" >"$W/mis.json" 2>/dev/null || { entry_get feat--h; jq '.slug = "someone-else"' "$W/entry.json" >"$W/mis.json"; }
    raw PUT "/$PB/worktrees/zz-mislabelled.json" "$W/mis.json" >/dev/null
    for f in follow "browse --json" leftovers; do
        out=$(gx follower "$S" $f 2>&1) && rc=0 || rc=$?
        if [ $rc = 0 ] && echo "$out" | grep -q 'feat--h'; then res "b-corrupt-and-mislabelled-$f" PASS 0 "exit 0, healthy worktree still listed; damaged entries: $(echo "$out" | grep -ciE 'zz-|corrupt|unreadable|mislabel') line(s)"; else res "b-corrupt-and-mislabelled-$f" FAIL 0 "rc=$rc: $(echo "$out" | head -2 | tr '\n' ' ' | cut -c1-140)"; fi
    done
    printf '{"schema":99,"entries":[]}' >"$W/dig.json"; raw PUT "/$PB/worktrees/_digest.json" "$W/dig.json" >/dev/null
    out=$(gx follower "$S" follow --full 2>&1 || true); gx follower "$S" follow >"$W/b-dig.out" 2>&1 && grep -q 'feat--h\|no changes\|^$' "$W/b-dig.out" && res b-unknown-schema-digest PASS 0 "follow ran past a schema-99 digest" || res b-unknown-schema-digest FAIL 0 "follow failed on the digest: $(head -c 140 "$W/b-dig.out")"
    raw DELETE "/$PB/worktrees/zz-corrupt.json" >/dev/null; raw DELETE "/$PB/worktrees/zz-mislabelled.json" >/dev/null; gx writer "$S" sweep >/dev/null 2>&1
fi
if want c; then
    gx follower "$S" follow >/dev/null 2>&1 || true
    OUT="$W/list.xml" raw GET "/$PB?list-type=2&prefix=worktrees/" >/dev/null; for k in $(grep -o '<Key>[^<]*</Key>' "$W/list.xml" | sed 's#</\?Key>##g'); do raw DELETE "/$PB/$k" >/dev/null; done
    out=$(gx follower "$S" follow 2>&1) && rc=0 || rc=$?
    if [ $rc != 0 ] && echo "$out" | grep -qi 'empty'; then res c-empty-registry-is-an-error PASS 0 "error, not mass-gone: $(echo "$out" | head -1 | cut -c1-120)"; else res c-empty-registry-is-an-error FAIL 0 "rc=$rc: $(echo "$out" | head -2 | tr '\n' ' ' | cut -c1-140)"; fi
    gx writer "$S" sweep >/dev/null 2>&1 || true; ( cd "$W/wt-h" && printf 'again\n' >>docs/h.md && gx writer "$S" sweep --only feat--h ) >/dev/null 2>&1 || true
    other=01$(python3 -c "import random; print(''.join(random.choice('0123456789ABCDEFGHJKMNPQRSTVWXYZ') for _ in range(24)))")
    out=$(WTS_PROJECT=$other gx follower "$S" follow 2>&1) && rc=0 || rc=$?
    echo "$out" | grep -q 'gone' && res c-wrong-project-not-mass-gone FAIL 0 "a wrong project reported worktrees gone" || res c-wrong-project-not-mass-gone PASS 0 "rc=$rc, no 'gone' lines: $(echo "$out" | head -1 | cut -c1-120)"
fi
if want d; then
    entry_get feat--h; base=$(cat "$W/entry.json")
    for h in 23 25; do
        echo "$base" | jq --argjson t "$(( $(now_ms) - h * 3600000 ))" '.state="merged" | .slug="zz-stuck" | .updated_ms=$t' >"$W/stuck.json"
        raw PUT "/$PB/worktrees/zz-stuck.json" "$W/stuck.json" >/dev/null
        gx follower "$S" leftovers 2>&1 | grep -i 'zz-stuck' | grep -qiE 'stuck|cleanup' && seen=yes || seen=no
        if { [ "$h" = 23 ] && [ $seen = no ]; } || { [ "$h" = 25 ] && [ $seen = yes ]; }; then res "d-merged-entry-${h}h" PASS 0 "stuck_cleanup reported: $seen (threshold 24 h)"; else res "d-merged-entry-${h}h" FAIL 0 "stuck_cleanup reported: $seen at ${h} h"; fi
    done
    raw DELETE "/$PB/worktrees/zz-stuck.json" >/dev/null
fi
if want e; then
    newwt s "Sync Docs"; mkdir -p "$W/wt-s/docs/many"; for i in $(seq 1 600); do printf '# d%s\n\nbody %s\n' "$i" "$i" >"$W/wt-s/docs/many/d$i.md"; done
    ( cd "$W/wt-s" && gx writer "$S" sweep --only feat--s ) >/dev/null 2>&1
    ( gx follower "$S" sync-docs feat--s "$W/mirror" >/dev/null 2>&1 || true ) & sp=$!; sleep 1.5; gate_kill_tree "$WP"; wait "$sp" 2>/dev/null || true
    part=$(find "$W/mirror" -name '*.md' 2>/dev/null | wc -l | tr -d ' '); up writer; WP=$LASTPID
    rm -f "$W/wt-s/docs/many/d7.md"; G "$W/wt-s" add -A; G "$W/wt-s" commit -qm del; ( cd "$W/wt-s" && gx writer "$S" sweep --only feat--s ) >/dev/null 2>&1
    gx follower "$S" sync-docs feat--s "$W/mirror" >/dev/null 2>&1 || true
    have=$(find "$W/mirror" -name '*.md' | wc -l | tr -d ' '); [ -z "$(find "$W/mirror" -name d7.md)" ] && res e-deleted-doc-not-in-mirror PASS 0 "after the cut ($part docs mirrored when killed) and a second sync, d7 absent, $have docs" || res e-deleted-doc-not-in-mirror FAIL 0 "d7.md survives in the mirror ($have docs)"
fi
if want f; then
    gap=${WO069_GATE_GAP:-200}; paths=${WO069_GATE_PATHS:-"docs/adr scripts/store"}; C="$W/clone"
    git clone -q --no-checkout "$GATE_REPO" "$C" 2>/dev/null; G "$C" sparse-checkout init --cone >/dev/null; G "$C" sparse-checkout set $paths >/dev/null
    top=$(G "$GATE_REPO" rev-parse HEAD); old=$(G "$GATE_REPO" rev-parse "HEAD~$gap")
    G "$C" checkout -q -b oldside "$old"; G "$C" checkout -q -b newside "$top"
    files=$(G "$C" ls-files | grep -E '\.(md|sh)$' | head -40); common=$(echo "$files" | head -8); own_a=$(echo "$files" | sed -n 9,16p); own_b=$(echo "$files" | sed -n 17,24p)
    G "$C" checkout -q oldside; for f in $common $own_a; do [ -f "$C/$f" ] && printf '\nside-a edit\n' >>"$C/$f"; done; G "$C" add -A; G "$C" commit -qm a
    G "$C" checkout -q newside; for f in $common $own_b; do [ -f "$C/$f" ] && printf '\nside-b edit\n' >>"$C/$f"; done; G "$C" add -A; G "$C" commit -qm b
    for side in oldside newside; do G "$C" worktree add -q -b "gate/$side" "$W/wt-$side" "$side" 2>/dev/null; ( cd "$W/wt-$side" && gx writer "$S" store-create && gx writer "$S" register && gx writer "$S" sweep --only "gate--$side" ) >/dev/null 2>&1; done
    G "$C" ls-files >"$W/tracked.txt"; out=$(gx follower "$S" overlaps gate--oldside gate--newside 2>&1 || true)
    echo "$out" | grep -oFf "$W/tracked.txt" | sort -u >"$W/reported.txt"
    G "$C" merge-tree --write-tree --name-only oldside newside 2>/dev/null | sed '1d' | grep -v '^$' | grep -v 'Auto-merging\|CONFLICT' | sort -u >"$W/true.txt" || true
    rep=$(wc -l <"$W/reported.txt" | tr -d ' '); tru=$(wc -l <"$W/true.txt" | tr -d ' '); fp=$(comm -23 "$W/reported.txt" "$W/true.txt" | wc -l | tr -d ' '); miss=$(comm -13 "$W/reported.txt" "$W/true.txt" | wc -l | tr -d ' ')
    res f-overlaps-different-bases "$([ "$miss" = 0 ] && echo PASS || echo FAIL)" 0 "bases $gap commits apart; reported $rep, true conflicts $tru, false $fp ($([ "$rep" = 0 ] && echo 0 || echo $((fp * 100 / rep))) % of the report), missed $miss"
fi
if want g; then
    : >"$W/g.log"; for r in $(seq 1 10); do
        ( cd "$W/wt-h" && printf 'r%s\n' "$r" >>docs/h.md && gx writer "$S" sweep --only feat--h ) >/dev/null 2>&1 || true
        for k in 1 2 3; do ( gx follower "$S" follow --state "$W/shared-state.json" >/dev/null 2>>"$W/g.log" || true ) & done; wait
        jq -e . "$W/shared-state.json" >/dev/null 2>&1 || { res g-shared-state-file FAIL 0 "state file does not parse after round $r"; break; }
        [ "$r" != 10 ] || res g-shared-state-file PASS 0 "10 rounds of 3 concurrent follows, file parsed every round; stderr lines: $(wc -l <"$W/g.log" | tr -d ' ')"
    done
fi
gate_say "== item 6 done: $FAILS failed row(s)"; exit $((FAILS > 0))
