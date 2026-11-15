#!/usr/bin/env bash
# Sourced by wo069-drills.sh (uses wo069-e2e-lib.sh helpers and $WORK $PROJECT $S $WEAVER $REPO). Drills d1, d2, d3 and the cargo-driven ones.

# The same as wo069-e2e-lib.sh's rustfs_start with a longer wait (up to 3 min): on a loaded machine the server took over 30 s to answer.
rustfs_start() { # sets RPORT RPID; writes $WORK/live.env (0600)
    local access secret code up=""
    RPORT=$(free_port); access="drl$(rand_hex 6)"; secret=$(rand_hex 20)
    # WEFTOS_DRILLS_DATA_DIR puts the server's data under another directory (a throwaway subdirectory is made there and removed on
    # exit). On the build machine the external volume costs ~70 ms per create+fsync against ~0.1 ms on the internal disk, which
    # dominates every PUT; the latency numbers say which disk they ran on.
    DRILL_DATA="$WORK/rfs-data"
    if [ -n "${WEFTOS_DRILLS_DATA_DIR:-}" ]; then mkdir -p "$WEFTOS_DRILLS_DATA_DIR"; DRILL_DATA="$(mktemp -d "$WEFTOS_DRILLS_DATA_DIR/wo069-drills-data.XXXXXX")"; DRILL_DATA_EXTERNAL_TO_WORK=1; fi
    mkdir -p "$DRILL_DATA"
    env RUSTFS_ACCESS_KEY="$access" RUSTFS_SECRET_KEY="$secret" "$RUSTFS_BIN" server --address "127.0.0.1:$RPORT" "$DRILL_DATA" >"$WORK/rustfs.log" 2>&1 &
    RPID=$!
    for _ in $(seq 1 360); do
        code=$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$RPORT/" || true)
        if [ "$code" != 000 ] && [ "$code" != 503 ]; then up=1; break; fi   # 000 not listening, 503 still starting
        kill -0 "$RPID" 2>/dev/null || { echo "rustfs exited early"; return 1; }
        sleep 0.5
    done
    [ -n "$up" ] || return 1
    sleep 2
    ( umask 077; printf 'MESH_S3_ENDPOINT=http://127.0.0.1:%s\nMESH_S3_ACCESS_KEY=%s\nMESH_S3_SECRET_KEY=%s\n' "$RPORT" "$access" "$secret" >"$WORK/live.env" )
}

# teardown of the e2e lib, plus the data directory when it lives outside $WORK
cleanup_all() {
    teardown
    [ -z "${DRILL_DATA_EXTERNAL_TO_WORK:-}" ] || { rm -rf "$DRILL_DATA"; echo "teardown: $DRILL_DATA removed"; }
}

use_external_rustfs() { # <env file>: a THROWAWAY RustFS on another host; never PG
    local f=$1 ep
    [ -r "$f" ] || { echo "cannot read $f" >&2; return 1; }
    ep=$(sed -n 's/^MESH_S3_ENDPOINT=//p' "$f" | head -1)
    case "$ep" in *100.90.170.87*|*photo-gallery*) echo "refusing: $ep is the production store, never a drill target" >&2; return 1 ;; esac
    [ -n "$ep" ] || { echo "$f has no MESH_S3_ENDPOINT" >&2; return 1; }
    ( umask 077; cp "$f" "$WORK/live.env" )
    echo "external throwaway rustfs: $ep"
}

setup_project() {
    G() { git -C "$1" "${@:2}"; }
    git init -q -b master "$WORK/repo"; echo base >"$WORK/repo/README.md"; G "$WORK/repo" add .; G "$WORK/repo" commit -qm base
    check "store-create (project + worktree store) as writer" "mkdir -p '$WORK/wt-seed' && git -C '$WORK/repo' worktree add -q -b feat/seed '$WORK/wt-seed' master && cd '$WORK/wt-seed' && as writer '$S' store-create"
    as writer "$WEAVER" store ls --json >"$WORK/ls.json" 2>/dev/null
    PNAME=$(grep -o "weftos://[^\"' ]*projects/$PROJECT/store" "$WORK/ls.json" | head -1)
    check "grant writer=write, follower=read on the project store" "as writer '$WEAVER' store grant '$PNAME' agent:writer write && as writer '$WEAVER' store grant '$PNAME' agent:follower read && as writer '$WEAVER' store grant '$PNAME' agent:merger write"
}

# new_wt <letter> <branch> <doc text>: a git worktree with a doc, store created and registered for the writer sweep
new_wt() {
    local d="$WORK/wt-$1"
    G "$WORK/repo" worktree add -q -b "$2" "$d" master
    mkdir -p "$d/docs"; printf '# %s\n\n%s\n' "$3" "$3 body" >"$d/docs/$1.md"
    ( cd "$d" && as writer "$S" store-create && as writer "$S" register ) >/dev/null 2>&1
}

drill1() {
    echo "== D1: two principals (writer RW, follower read grants only), one machine"
    local WA="$WORK/wt-a" SA=feat--a
    new_wt a feat/a "Drill One Alpha"
    echo 'fn main() {}' >"$WA/main.rs"; G "$WA" add -A; G "$WA" commit -qm "second commit"
    timed "writer sweep (publish tree + bundle)" "as writer '$S' sweep --only $SA"
    check "writer published once" "grep -q '^$SA	published' '$WORK/last.out'"
    local t_pub; t_pub=$(now_ms)
    timed "follower follow (first poll)" "as follower '$S' follow"
    check "follow reports the new worktree" "grep -q '$SA' '$WORK/last.out' && grep -qi new '$WORK/last.out'"
    # publish -> poll latency: commit (head moves), writer sweep, then time the follower's first poll that reports it
    local i lats=""
    for i in 1 2 3 4 5; do
        printf 'v%s\n' "$i" >>"$WA/docs/a.md"; G "$WA" add -A; G "$WA" commit -qm "edit $i"
        as writer "$S" sweep --only $SA >/dev/null 2>&1
        local t0; t0=$(now_ms)
        as follower "$S" follow >"$WORK/last.out" 2>&1
        local ms=$(( $(now_ms) - t0 ))
        grep -q 'head_moved' "$WORK/last.out" && lats="$lats $ms" || lats="$lats MISSED($ms)"
    done
    check "every head move reported by the first poll after the publish (ms per poll:$lats)" "! echo '$lats' | grep -q MISSED"
    echo "BENCH	d1	publish->first-poll-that-reports	polls_ms:$lats"
    timed "follower search" "as follower '$S' search Alpha"
    check "follower search finds the doc" "grep -q 'docs/a.md' '$WORK/last.out'"
    timed "follower sync-docs" "as follower '$S' sync-docs $SA '$WORK/cache-f'"
    check "sync-docs mirrored the doc" "grep -q 'v5' '$WORK/cache-f/$SA/docs/a.md' 2>/dev/null || find '$WORK/cache-f' -name a.md | head -1 | xargs grep -q v5"
    timed "follower browse (list, ls docs/, get)" "as follower '$S' browse --json && as follower '$S' browse $SA docs/ --json && as follower '$S' browse $SA docs/a.md"
    check "browse get returns the doc" "as follower '$S' browse $SA docs/a.md | grep -q v5"
    # a bundle needs the base commit it was cut from, so the bundle half goes into a clone; the tree half into a plain fresh directory
    timed "follower fetch (tree only) into a fresh plain directory" "as follower '$S' fetch $SA '$WORK/fetch-plain'"
    check "fetched tree has the files" "test -f '$WORK/fetch-plain/main.rs' && test -f '$WORK/fetch-plain/docs/a.md' && grep -q v5 '$WORK/fetch-plain/docs/a.md'"
    git clone -q "$WORK/repo" "$WORK/fetch-f"
    timed "follower fetch (tree + bundle) into a fresh clone" "as follower '$S' fetch $SA '$WORK/fetch-f'"
    check "fetched tree has the files and the bundle fetched under refs/shared" "test -f '$WORK/fetch-f/main.rs' && test -f '$WORK/fetch-f/docs/a.md' && [ -n \"\$(git -C '$WORK/fetch-f' for-each-ref refs/shared)\" ]"
    drill1_refusals
}

# What a follower with only read grants can do through the weaver. The governed data plane (`store put`) is grant-gated; the S2
# worktree verbs (`store worktree ...`) run on the daemon's operator path, so this prints each verb's real answer.
drill1_refusals() {
    local SA=feat--a WA="$WORK/wt-a" WN
    WN=$(grep -o "weftos://[^\"' ]*projects/$PROJECT/worktrees/$SA" "$WORK/ls.json" | head -1)
    as writer "$WEAVER" store ls --json >"$WORK/ls.json" 2>/dev/null
    WN=$(grep -o "weftos://[^\"' ]*projects/$PROJECT/worktrees/[^\"' ]*$SA[^\"' ]*" "$WORK/ls.json" | head -1)
    check "follower: governed put into the project store is refused" "! as follower '$WEAVER' store put '$PNAME' d1/nope.txt no --as agent:follower"
    [ -z "$WN" ] || check "follower: governed put into the worktree store is refused" "! as follower '$WEAVER' store put '$WN' d1/nope.txt no --as agent:follower"
    check "follower: governed put into a registry key of the project store is refused" "! as follower '$WEAVER' store put '$PNAME' worktrees/forged.json '{}' --as agent:follower"
    # the S2 verbs: record the answer to each write verb as the follower principal
    local verb res
    if as follower "$WEAVER" store grant "$PNAME" agent:follower write >/dev/null 2>&1; then
        printf 'D1\tfollower principal, `store grant` (admin, on itself): ALLOWED\n' | tee -a "$WORK/steps.tsv"
        finding "D1-2 \`weaver store grant\` has no caller identity: the follower principal granted ITSELF write on the project store and the daemon accepted it (same root cause as D1-1; anyone who can reach the daemon socket is one trust domain)"
    fi
    for verb in "claim|claim $SA" "publish|share"; do
        res=ALLOWED
        case "${verb%%|*}" in
            claim) ( cd "$WA" && as follower "$S" claim $SA ) >"$WORK/d1-$verb.out" 2>&1 || res=REFUSED ;;
            publish) ( cd "$WA" && as follower "$S" share ) >"$WORK/d1-publish.out" 2>&1 || res=REFUSED ;;
        esac
        printf 'D1\tfollower principal, `%s`: %s\n' "${verb%%|*}" "$res" | tee -a "$WORK/steps.tsv"
        [ "$res" = ALLOWED ] || head -c 300 "$WORK/d1-${verb%%|*}.out" | sed 's/^/    | /' 
    done
    if grep -q "^D1	follower principal.*ALLOWED" "$WORK/steps.tsv"; then
        finding "D1-1 the S2 worktree verbs (publish, claim, orphan, gc, create, promote) are not gated by the caller's grants: the daemon runs them on its operator path and the principal (WEFTOS_AGENT) is a self-asserted label, so a principal holding only a read grant is not refused. Read-only is enforced only by the S3 credential scope (library drill 1a: a real read-only RustFS key IS refused every write). Blocking for any claim about several principals on ONE daemon; not blocking for two machines that each hold scoped keys"
    fi
}

# cargo-driven drills: run an ignored live test file against $WORK/live.env and surface its DRILL / FINDING / BENCH / DETAIL lines.
cargo_live() { # <label> <test target> <extra env...>
    local label=$1 target=$2; shift 2
    local out="$WORK/cargo-$label.out" t0 rc=0
    t0=$(now_ms)
    ( cd "$REPO" && env "$@" cargo test -p weftos-store --features rustfs $REL --test "$target" -- --ignored --nocapture --test-threads=1 ) >"$out" 2>&1 || rc=$?
    grep -E '^(DRILL|FINDING|BENCH|DETAIL)\||^test |test result|panicked' "$out" | sed 's/^/    /'
    printf '%s\t%s ms\tcargo %s (%s)\n' "$([ $rc = 0 ] && echo PASS || echo FAIL)" "$(( $(now_ms) - t0 ))" "$target" "$label" | tee -a "$WORK/steps.tsv"
    if [ $rc != 0 ]; then FAILS=$((FAILS + 1)); tail -30 "$out" | sed 's/^/    | /'; fi
    grep -E '^FINDING\|' "$out" | sed 's/^FINDING|/FINDING\t/' >>"$WORK/findings.txt"
    [ -z "${E2E_LOG_DIR:-}" ] || { mkdir -p "$E2E_LOG_DIR"; cp "$out" "$E2E_LOG_DIR/drills-cargo-$label.log"; }
}

lib_drills() {
    echo "== library drills on the real RustFS (1a, 2l, 3a, 3b, 3c, W1, W2)"
    local bundle=""; [ -f "$WORK/fetch-f/.wt-branch.bundle" ] && bundle="WEFTOS_DRILL_BUNDLE=$WORK/fetch-f/.wt-branch.bundle"   # a real git bundle from d1, when d1 ran in this run
    cargo_live drills-1a-2l s2_drills_live "WEFTOS_STORE_TEST_RUSTFS_ENV=$WORK/live.env" ${bundle:+"$bundle"}
    cargo_live drills-3 s2_drills_fail "WEFTOS_STORE_TEST_RUSTFS_ENV=$WORK/live.env"
    cargo_live drills-window s2_drills_window "WEFTOS_STORE_TEST_RUSTFS_ENV=$WORK/live.env"
}

scale_drills() {
    local sizes=1000,10000
    [ -z "$SCALE" ] || sizes=1000,10000,20000,50000
    sizes=${WEFTOS_SCALE_SIZES:-$sizes}   # override, e.g. WEFTOS_SCALE_SIZES=300 for a smoke run
    echo "== D4: client-scale trees, sizes $sizes (release build: $([ -n "$REL" ] && echo yes || echo NO, pass --release for latency numbers))"
    mkdir -p "$WORK/scale-tmp"
    cargo_live scale s2_scale_bench "WEFTOS_STORE_TEST_RUSTFS_ENV=$WORK/live.env" "WEFTOS_SCALE_SIZES=$sizes" "WEFTOS_SCALE_TMP=$WORK/scale-tmp"
}


# helper <op>: one test-only operation on the THROWAWAY store (see helper_throwaway_store_op in s2_drills_live.rs); prints HELPER| lines
helper() {
    ( cd "$REPO" && env "WEFTOS_STORE_TEST_RUSTFS_ENV=$WORK/live.env" "WEFTOS_DRILL_OP=$1" cargo test -p weftos-store --features rustfs --test s2_drills_live helper_throwaway_store_op -- --ignored --nocapture 2>&1 ) | grep -E '^HELPER\||panicked|error' | head -5
}

pbucket() { printf 'p-%s' "$(printf '%s' "$PROJECT" | tr 'A-Z' 'a-z')"; }
wbucket() { sed -n 's/.*"bucket": *"\([^"]*\)".*/\1/p' "$1" | head -1; }  # <entry json file>
entry_json() { as writer "$WEAVER" store get "$PNAME" "worktrees/$1.json" --as agent:writer; }  # <slug>

drill2() {
    echo "== D2: leftover drill (orphan + forgotten) and handoff drill"
    local WO="$WORK/wt-o" SO=feat--o SB=feat--b PB; PB=$(pbucket)
    new_wt o feat/o "Drill Orphan"; new_wt b feat/b "Drill Forgotten"
    check "publish both worktrees" "as writer '$S' sweep >'$WORK/last.out' 2>&1 && grep -q '^$SO	published' '$WORK/last.out' && grep -q '^$SB	published' '$WORK/last.out'"
    entry_json $SO >"$WORK/entry-o.json"; local WBK; WBK=$(wbucket "$WORK/entry-o.json")
    timed "leftovers (nothing is old yet)" "as writer '$S' leftovers"
    check "leftovers: none yet" "grep -q 'no leftovers' '$WORK/last.out'"
    # the daemon verb has no clock override: the registry entry's updated_ms is moved back 4 days (threshold: open 3 d) in the throwaway store
    helper "backdate|$PB|worktrees/$SB.json|updated_ms|4" | tee -a "$WORK/steps.tsv"
    timed "leftovers (feat--b idle 4 days)" "as writer '$S' leftovers"
    check "leftovers reports feat--b as forgotten_open past the 3-day threshold, and only it" "grep -q '$SB' '$WORK/last.out' && grep -qi 'forgotten' '$WORK/last.out' && ! grep -q '$SO' '$WORK/last.out'"
    # orphan: remove the git worktree without landing
    G "$WORK/repo" worktree remove --force "$WO"
    timed "sweep after git worktree remove" "as writer '$S' sweep --only $SO"
    check "sweep reports the worktree as orphan" "grep -q '^$SO	orphan' '$WORK/last.out'"
    check "daemon refused to mark it yet (24 h grace, marked:false) and the entry is still open" "grep -q '\"marked\": false' '$WORK/last.err' && as writer '$S' browse --json | jq -e '.worktrees[] | select(.slug==\"$SO\") | .state==\"open\"'"
    # inject the clock: the sweep stamped missing_since=now; make it 2 days ago in this machine's sweep state
    local rf; rf=$(grep -l "^slug=$SO\$" "$WORK"/xdg-writer/wts/reg/*/* | head -1)
    sed -i '' "s/^missing_since=.*/missing_since=$(date -u -v-2d +%Y-%m-%dT%H:%M:%SZ)/" "$rf"
    timed "sweep with missing_since 2 days ago" "as writer '$S' sweep --only $SO"
    check "the daemon marks it orphan (marked:true, new:true)" "grep -q '\"marked\": true' '$WORK/last.err' && as writer '$S' browse --json | jq -e '.worktrees[] | select(.slug==\"$SO\") | .state==\"orphan\"'"
    timed "leftovers after marking" "as writer '$S' leftovers"
    if ! grep -q "$SO" "$WORK/last.out"; then
        printf 'D2\tleftovers does not list the orphan (it lists forgotten/unclaimed/abandoned only)\n' | tee -a "$WORK/steps.tsv"
        finding "D2-1 \`leftovers\` has no arm for the orphan state, so an entry the daemon marked orphan drops OUT of \`leftovers\` (it was ForgottenOpen a moment before) and only \`gc\` lists it. Not blocking (gc dry run lists it); proposed: report State::Orphan from leftovers with its orphan age (follower.rs, lane G owns that file)"
    fi
    timed "gc dry run" "as writer '$WEAVER' store worktree gc --project $PROJECT"
    check "gc dry run lists the orphan, not yet due, changes nothing (bucket still there)" "jq -e '.orphans[] | select(.slug==\"$SO\") | .due==false' '$WORK/last.out' && helper 'exists|$WBK' | grep -q true"
    helper "list|$WBK|" | tee -a "$WORK/steps.tsv"
    helper "backdate|$PB|worktrees/$SO.json|orphan.orphaned_at_ms|15" | tee -a "$WORK/steps.tsv"
    timed "gc dry run (orphan now 15 days old)" "as writer '$WEAVER' store worktree gc --project $PROJECT"
    check "gc dry run: due" "jq -e '.orphans[] | select(.slug==\"$SO\") | .due==true' '$WORK/last.out'"
    timed "gc --confirmed" "as writer '$WEAVER' store worktree gc --project $PROJECT --confirmed"
    check "gc --confirmed retired the orphan" "jq -e '.orphans_retired | index(\"$SO\") != null' '$WORK/last.out' && jq -e '.orphans_failed | length == 0' '$WORK/last.out'"
    check "its registry entry is gone and the other worktree's stays" "as writer '$S' browse --json | jq -e '([.worktrees[].slug] | index(\"$SO\") == null) and ([.worktrees[].slug] | index(\"$SB\") != null)'"
    check "its bucket is deleted" "helper 'exists|$WBK' | grep -q false"
    check "the archive holds the tree (docs/o.md under the bucket's archive prefix)" "helper 'list|weftos-archive|$WBK@' | tee '$WORK/arch.out' | grep -q 'docs/o.md'"
    cat "$WORK/arch.out" | sed 's/^/    /'
    drill2_handoff c gc
    drill2_handoff d archive
}

drill2_handoff() { # <letter> <gc|archive>: the cleanup route after promote
    echo "-- D2 handoff half ($1, cleanup by $2): hand off, a second principal claims, lands with plain git in a second clone, promotes, bucket deleted"
    local L=$1 route=$2 WC="$WORK/wt-$1" SC=feat--$1 PB; PB=$(pbucket)
    new_wt $L feat/$L "Drill Handoff $L"
    echo 'pub fn landed() {}' >"$WC/lib-$L.rs"; G "$WC" add -A; G "$WC" commit -qm "work to land"
    local head; head=$(G "$WC" rev-parse HEAD)
    timed "writer share --handoff (tree + bundle + handoff note)" "cd '$WC' && as writer '$S' share --handoff 'ready for merge'"
    check "registry says handoff" "as writer '$S' browse --json | jq -e '.worktrees[] | select(.slug==\"$SC\") | .state==\"handoff\"'"
    timed "merger claim" "as merger '$S' claim $SC"
    check "claim took it at epoch 1" "grep -q '\"epoch\": 1' '$WORK/last.out'"
    local epoch; epoch=$(sed -n 's/.*"epoch": *\([0-9]*\).*/\1/p' "$WORK/last.out" | head -1)
    check "a third party is refused while the claim is live (writer re-claim by another agent)" "! as follower '$S' claim $SC"
    git clone -q "$WORK/repo" "$WORK/fetch-$L"; git clone -q "$WORK/repo" "$WORK/clone-$L"
    timed "merger fetch of the shared tree + bundle (second machine's view)" "as merger '$S' fetch $SC '$WORK/fetch-$L'"
    check "second clone lands the branch with plain git (fetch bundle, merge --ff-only)" "git -C '$WORK/clone-$L' fetch -q '$WORK/fetch-$L/.wt-branch.bundle' '+refs/heads/*:refs/shared/$SC/*' && git -C '$WORK/clone-$L' merge -q --ff-only refs/shared/$SC/feat/$L && [ \"\$(git -C '$WORK/clone-$L' rev-parse HEAD)\" = '$head' ] && test -f '$WORK/clone-$L/lib-$L.rs'"
    timed "merger promote (fenced with epoch $epoch)" "as merger '$WEAVER' store worktree promote --project $PROJECT --slug $SC --epoch $epoch"
    check "promote reported promoted objects" "jq -e '.promoted | tonumber? // . | . >= 0' '$WORK/last.out'"
    check "registry entry says merged (cleanup pending)" "as writer '$S' browse --json | jq -e '.worktrees[] | select(.slug==\"$SC\") | .state==\"merged\"'"
    local wbk; entry_json $SC >"$WORK/entry-c.json"; wbk=$(wbucket "$WORK/entry-c.json")
    sleep 1
    if [ "$route" = gc ]; then
        timed "gc --keep-days 0 --confirmed (landed worktree store archived and deleted)" "as writer '$WEAVER' store worktree gc --project $PROJECT --keep-days 0 --confirmed"
        check "gc retired the landed worktree store" "jq -e '.retired | index(\"weftos://local/projects/$PROJECT/worktrees/$SC/store\") != null or (.retired | length >= 1)' '$WORK/last.out'"
    else
        timed "store archive --delete (the documented cleanup)" "as writer '$WEAVER' store archive 'weftos://local/projects/$PROJECT/worktrees/$SC/store' --delete"
    fi
    check "landed worktree's bucket is deleted" "helper 'exists|$wbk' | grep -q false"
    as writer "$S" browse --json >"$WORK/after-gc.json"
    if jq -e '[.worktrees[].slug] | index("'$SC'") == null' "$WORK/after-gc.json" >/dev/null; then
        printf 'PASS\t0 ms\tlanded worktree %s: registry entry disappears (cleanup by %s)\n' "$SC" "$route" | tee -a "$WORK/steps.tsv"
    else
        FAILS=$((FAILS + 1)); printf 'FAIL\t0 ms\tlanded worktree %s: registry entry STILL THERE after cleanup by %s\n' "$SC" "$route" | tee -a "$WORK/steps.tsv"
        [ "$route" != gc ] || finding "D2-2 \`store worktree gc --confirmed\` archives and deletes a landed worktree's bucket but leaves its registry entry (state merged) pointing at the deleted bucket; ADR-126 section 4a says the daemon removes the entry after promotion and bucket deletion. \`store archive --delete\` does remove it (drill d). Repro: handoff, claim, promote, \`store worktree gc --keep-days 0 --confirmed\`, then \`browse --json\` still lists the slug as merged; \`leftovers\` reports stuck_cleanup after 24 h. Cause: the landed-retire loop in store_worktree.rs calls svc.archive(..) without forget(svc, &name) (the orphan retire path does remove the entry). Proposed fix, in lane G's file: call forget(svc, &nm) after a successful svc.archive in that loop. Not blocking for the gate but a visible leftover"
    fi
    jq -c '.worktrees[] | select(.slug=="'$SC'")' "$WORK/after-gc.json" | sed 's/^/    registry after cleanup: /'
    as follower "$S" follow | grep "$SC" | sed 's/^/    follower sees: /' || true
}

drill3() {
    echo "== D3: the daemon is killed during a publish and restarted (gate item 5c)"
    local WK="$WORK/wt-k" SK=feat--k i d
    new_wt k feat/k "Drill Kill"
    check "baseline publish" "as writer '$S' sweep --only $SK >'$WORK/last.out' 2>&1 && grep -q '^$SK	published' '$WORK/last.out'"
    if ! grep -q -- '--full' "$S"; then finding "D3-2 \`worktree-store.sh follow\` does not take --full (it passes only --state); the verb \`store worktree follow --full\` exists and the ADR documents \`follow --full\` as the way to skip a stale digest, so the documented command does not exist in the shell. Non-blocking; the drill calls the verb directly"; fi
    for d in 0.8 2.5 4.5; do
        # 200 files of ~21 kB: a publish of ~6 s with the data on the internal disk (~55 s on the external volume), so the kills land early, mid and late, so the kill lands inside the upload loop or the refresh
        mkdir -p "$WK/gen$d"; for i in $(seq 1 200); do head -c 16000 /dev/urandom | base64 >"$WK/gen$d/f$i.txt"; done
        local t0; t0=$(now_ms)
        ( as writer "$S" sweep --only $SK >"$WORK/kill-$d.out" 2>&1 ) & local sp=$!
        sleep "$d"
        local kids; kids=$(descendants "$DPID"); kill -9 "$DPID" 2>/dev/null; for i in $kids; do kill -9 "$i" 2>/dev/null; done
        wait "$sp" 2>/dev/null
        printf 'D3\tdaemon killed %ss into a publish [%s]; sweep said: %s (%s ms after start)\n' "$d" "$(grep -q 'failed\|Error' "$WORK/kill-$d.out" && echo 'KILLED MID-PUBLISH' || echo 'publish had already finished')" "$(tr '\t\n' '  ' <"$WORK/kill-$d.out" | cut -c1-110)" "$(( $(now_ms) - t0 ))" | tee -a "$WORK/steps.tsv"
        timed "daemon restart" "rm -f '$WORK/run'/*.sock 2>/dev/null; daemon_start"
        check "after restart: digest follower and --full follower agree on the registry" "as follower '$S' follow --state '$WORK/fs-digest-$d.json' >'$WORK/f1.out' 2>&1; as follower '$WEAVER' store worktree follow --project '$PROJECT' --state '$WORK/fs-full-$d.json' --full --json >'$WORK/f2.json' 2>&1; jq -r '.changes[] | \"\\(.change) \\(.worktree.slug)\"' '$WORK/f2.json' | sort >'$WORK/f2.out'; sed -E 's/^([a-z_]+) +([^ ]+) .*/\\1 \\2/' '$WORK/f1.out' | sort >'$WORK/f1n.out'; diff '$WORK/f1n.out' '$WORK/f2.out'"
        check "after restart: the registry entry is readable and names the old or the new file count" "as follower '$S' browse --json | jq -e '.worktrees[] | select(.slug==\"$SK\") | .file_count >= 2'"
        timed "rerun sweep completes the publish" "as writer '$S' sweep --only $SK"
        check "rerun left the worktree published (published, or unchanged when the kill came after the publish finished); follower sees all files (browse root lists gen$d)" "grep -qE '^$SK	(published|unchanged)' '$WORK/last.out' && as follower '$S' browse $SK gen$d/ --json | jq -e '.entries | length >= 1'"
        check "no checksum mismatch anywhere: fetch of the whole tree into a fresh dir has every file" "rm -rf '$WORK/fk-$d'; as follower '$S' fetch $SK '$WORK/fk-$d' >'$WORK/fk-$d.out' 2>&1; grep -qi 'checksum' '$WORK/fk-$d.out' && exit 1; [ \"\$(ls '$WORK/fk-$d'/gen$d | wc -l | tr -d ' ')\" = 200 ]"
        if grep -q 'published without a git bundle' "$WORK/fk-$d.out" && ! grep -q 'no commits to fetch' "$WORK/fk-$d.out"; then finding "D1-3 \`worktree-store.sh fetch\` of a worktree with no commits past its base (uncommitted-only edits, so no bundle) writes the whole tree and then exits 1 ('this tree was published without a git bundle'), because the shell always passes --bundle-out and the verb treats a missing bundle as fatal. Expected: tree written, exit 0 with a note. Non-blocking; fix in the verb (return bundle_head null) or the shell (skip --bundle-out when the manifest has none)"; fi
    done
}
