#!/usr/bin/env bash
# WO-069 lane D: the sweep end to end. The shell sweep (worktree-store.sh sweep) -> a REAL weaver built from this
# branch -> a THROWAWAY user daemon (isolated runtime dir, fake HOME) -> ONE throwaway loopback RustFS, with two
# local principals (writer, follower), then the branch-change case (design note Q9).
#
#   WEFTOS_STORE_TEST_RUSTFS_BIN=/path/to/rustfs [WEFTOS_E2E_WEAVER=target/debug/weaver] [E2E_LOG_DIR=dir] [KEEP=1] \
#     scripts/store/wo069-e2e-test.sh
#
# Never point this at PG or a shared store. It starts its own processes on 127.0.0.1 (random port and credentials)
# under target/, kills only those, and removes its data on exit (trap). It installs nothing and never touches
# ~/.weftos: the daemon runs with WEFTOS_RUNTIME_DIR and HOME both inside the work dir.
set -uo pipefail
RUSTFS_BIN="${WEFTOS_STORE_TEST_RUSTFS_BIN:?set WEFTOS_STORE_TEST_RUSTFS_BIN to the rustfs binary}"
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WEAVER="${WEFTOS_E2E_WEAVER:-$REPO/target/debug/weaver}"
[ -x "$RUSTFS_BIN" ] && [ -x "$WEAVER" ] || { echo "need executable rustfs ($RUSTFS_BIN) and weaver ($WEAVER)" >&2; exit 1; }
mkdir -p "$REPO/target"
WORK="$(mktemp -d "$REPO/target/wo069-e2e.XXXXXX")"
# shellcheck source=wo069-e2e-lib.sh
. "$REPO/scripts/store/wo069-e2e-lib.sh"
trap teardown EXIT INT TERM
S="$REPO/scripts/worktree-store.sh"
ULID_CHARS=0123456789ABCDEFGHJKMNPQRSTVWXYZ
PROJECT="01$(python3 -c "import random; print(''.join(random.choice('$ULID_CHARS') for _ in range(24)))")"
printf '[user]\n\tname = e2e\n\temail = e2e@example.invalid\n[init]\n\tdefaultBranch = master\n' >"$WORK/gitconfig"
G() { git -C "$1" "${@:2}"; }   # G <dir> git-args
jqs() { jq -r "$1" "$WORK/last.out"; }

echo "== setup: project $PROJECT, weaver $WEAVER"
check "throwaway rustfs up on loopback" 'rustfs_start'
check "throwaway user daemon up (isolated runtime dir, store on that rustfs)" 'daemon_start'

# ---- git: one repo, worktree A on feat/e2e
export GIT_CONFIG_GLOBAL="$WORK/gitconfig" GIT_CONFIG_NOSYSTEM=1
git init -q -b master "$WORK/repo"; echo base >"$WORK/repo/README.md"; G "$WORK/repo" add .; G "$WORK/repo" commit -qm base
WA="$WORK/wt-a"; G "$WORK/repo" worktree add -q -b feat/e2e "$WA" master
SA=feat--e2e

# ---- principals: writer (write) and follower (read only) on the project store
check "store-create makes the project and worktree stores" "cd '$WA' && as writer '$S' store-create"
check "stores exist" "as writer '$WEAVER' store ls --json >'$WORK/ls.json' && jq -e '[.. | strings | select(test(\"projects/$PROJECT/store\"))] | length > 0' '$WORK/ls.json' >/dev/null || grep -q '$PROJECT' '$WORK/ls.json'"
PNAME=$(grep -o "weftos://[^\"' ]*projects/$PROJECT/store" "$WORK/ls.json" | head -1)
check "grant writer=write, follower=read on the project store" "as writer '$WEAVER' store grant '$PNAME' agent:writer write && as writer '$WEAVER' store grant '$PNAME' agent:follower read"
check "governed put as writer is allowed" "as writer '$WEAVER' store put '$PNAME' e2e/probe.txt hello --as agent:writer"
check "governed put as follower is refused (read-only grant)" "! as follower '$WEAVER' store put '$PNAME' e2e/nope.txt no --as agent:follower"

# ---- writer: register, edit, sweep
mkdir -p "$WA/docs"; printf '# E2E Plan Alpha\n\nfirst draft\n' >"$WA/docs/e2e-plan.md"
check "register worktree A" "cd '$WA' && as writer '$S' register | grep -q 'registered $SA'"
timed "sweep 1 (publish)" "as writer '$S' sweep"; cp "$WORK/last.out" "$WORK/sweep1.out"
check "sweep 1 published once" "grep -q '^$SA	published' '$WORK/sweep1.out' && [ \$(calls writer publish) = 1 ]"
N0=$(calls writer ''); timed "sweep 2 (idle)" "as writer '$S' sweep"
check "sweep 2 idle: unchanged and ZERO weaver calls" "grep -q '^$SA	unchanged' '$WORK/last.out' && [ \$(calls writer '') = $N0 ]"

# ---- follower: sees it without a checkout
timed "follower follow" "as follower '$S' follow"
check "follow reports the new worktree" "grep -q '$SA' '$WORK/last.out' && grep -qi 'new' '$WORK/last.out'"
timed "follower search" "as follower '$S' search Alpha"
check "search finds the doc" "grep -q 'docs/e2e-plan.md' '$WORK/last.out'"
timed "follower browse (worktree list)" "as follower '$S' browse --json"
check "browse lists the worktree with its branch" "[ \"\$(jq -r '.worktrees[] | select(.slug==\"$SA\") | .branch' '$WORK/last.out')\" = feat/e2e ]"
UM1=$(jq -r ".worktrees[] | select(.slug==\"$SA\") | .updated_ms" "$WORK/last.out")
timed "follower browse docs/" "as follower '$S' browse $SA docs/ --json"
check "browse docs/ lists the file with its title" "[ \"\$(jq -r '.entries[] | select(.path==\"docs/e2e-plan.md\") | .title' '$WORK/last.out')\" = 'E2E Plan Alpha' ]"
timed "follower browse docs/e2e-plan.md" "as follower '$S' browse $SA docs/e2e-plan.md"
check "browse of the file returns its text" "grep -q 'first draft' '$WORK/last.out'"

# ---- second edit: republish once, follower sees the change
printf 'second draft\n' >>"$WA/docs/e2e-plan.md"
timed "sweep 3 (edit)" "as writer '$S' sweep"
check "sweep 3 republished once" "grep -q '^$SA	published' '$WORK/last.out' && [ \$(calls writer publish) = 2 ]"
timed "follower follow after the edit" "as follower '$S' follow"; cp "$WORK/last.out" "$WORK/follow2.out"
check "follow stays quiet for a doc-only edit (head did not move)" "grep -q 'no changes' '$WORK/follow2.out'"
check "browse shows the edit: updated_ms moved" "as follower '$S' browse --json | jq -e \".worktrees[] | select(.slug==\\\"$SA\\\") | .updated_ms > $UM1\""
check "follower reads the new text" "as follower '$S' browse $SA docs/e2e-plan.md | grep -q 'second draft'"

# ---- Q9: branch change, (a) slug fixed at registration vs (b) slug follows the branch (register --follow-branch)
. "$REPO/scripts/store/wo069-e2e-q9.sh"

echo "== summary: $(grep -c '^PASS' "$WORK/steps.tsv") passed, $FAILS failed"
exit $((FAILS > 0))
