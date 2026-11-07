#!/usr/bin/env bash
# Compartmentalized test for scripts/worktree-store.sh: real git repo, fake weaver and fake agf.
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
pass=0; fail=0
ok() { if eval "$2"; then pass=$((pass+1)); else fail=$((fail+1)); echo "FAIL: $1"; fi; }

# fake weaver records calls; fake agf land.sh succeeds or fails on demand
mkdir -p "$t/bin" "$t/agf"
cat >"$t/bin/weaver" <<'W'
#!/usr/bin/env bash
printf '%q ' "$@" >>"$WTS_LOG"; echo >>"$WTS_LOG"
if [ "$1 $2 $3" = "store worktree buckets" ]; then echo "w-01abc-aaaaaaaaaaaa p-01abc"; fi
if [ "$1 $2" = "project id" ]; then echo 01ABC; fi
if [ "$1 $2 $3" = "store worktree fetch" ]; then : ; fi
exit "${WEAVER_RC:-0}"
W
cat >"$t/agf/land.sh" <<'L'
#!/usr/bin/env bash
exit "${LAND_RC:-0}"
L
chmod +x "$t/bin/weaver" "$t/agf/land.sh"
export WTS_WEAVER="$t/bin/weaver" WTS_AGF="$t/agf" WTS_LOG="$t/log" WTS_PROJECT=01ABC
: >"$t/log"

# repo with a base commit on master, a lane with one commit, a tracked change, an untracked doc, a secret
r="$t/repo"; git init -q -b master "$r"; cd "$r"
git config user.email t@t; git config user.name t
echo a >a.txt; git add a.txt; git commit -qm base
git checkout -qb feat/s2
echo b >b.txt; git add b.txt; git commit -qm lane
echo changed >>a.txt
mkdir -p docs; echo "# Rooms" >docs/rooms.md; echo TOKEN=1 >.env; echo ignored >ig.log; echo '*.log' >.gitignore
export AGF_TARGET_REF=master

"$here/worktree-store.sh" share --handoff "please merge"
ok "share calls publish" "grep -q '^store worktree publish' '$t/log'"
ok "slug derived from branch" "grep -q -- '--slug feat-s2' '$t/log'"
ok "handoff note passed as one argument" "grep -qF -- '--handoff please\\ merge' '$t/log'"
ok "bundle passed" "grep -q -- '--bundle ' '$t/log'"
ok "base is the master tip" "grep -q -- \"--base $(git rev-parse master)\" '$t/log'"
# the path list handed to weaver includes the untracked doc and excludes ignored files
pz=$(grep -o -- '--paths-z [^ ]*' "$t/log" | cut -d' ' -f2)
ok "paths file removed after run (trap)" "[ ! -e '$pz' ]"

# land only promotes when land.sh succeeds
: >"$t/log"; LAND_RC=2 "$here/worktree-store.sh" land && rc=0 || rc=$?
ok "land failure propagates" "[ $rc = 2 ]"
ok "no promote after failed land" "! grep -q promote '$t/log'"
: >"$t/log"; "$here/worktree-store.sh" land
ok "promote after successful land" "grep -q '^store worktree promote' '$t/log'"

# a lost claim, a failed share and a missing weaver must fail; promote after land is best-effort
WEAVER_RC=1 "$here/worktree-store.sh" claim some-slug 2>/dev/null && rc=0 || rc=$?
ok "lost claim exits non-zero" "[ $rc != 0 ]"
WEAVER_RC=1 "$here/worktree-store.sh" share 2>/dev/null && rc=0 || rc=$?
ok "failed share exits non-zero" "[ $rc != 0 ]"
WTS_WEAVER=/nonexistent "$here/worktree-store.sh" claim some-slug 2>/dev/null && rc=0 || rc=$?
ok "missing weaver fails claim" "[ $rc != 0 ]"
WEAVER_RC=1 "$here/worktree-store.sh" land 2>/dev/null && rc=0 || rc=$?
ok "promote failure after land is a warning" "[ $rc = 0 ]"
WEAVER_RC=1 WTS_STRICT=1 "$here/worktree-store.sh" land 2>/dev/null && rc=0 || rc=$?
ok "WTS_STRICT makes it fatal" "[ $rc != 0 ]"

# input validation
for bad in 'A' '../x' 'a b' '-x' "$(printf 'a%.0s' {1..60})"; do
    "$here/worktree-store.sh" claim "$bad" 2>/dev/null && rc=0 || rc=$?
    ok "bad slug rejected: $bad" "[ $rc != 0 ]"
done

# union mount config
out=$("$here/worktree-store.sh" mount-config s2)
ok "union remote present" "grep -q '^type = union' <<<'$out'"
ok "project upstream is read-only" "grep -q 'p-01abc:ro' <<<'$out'"
ok "worktree upstream first" "grep -q 'upstreams = wts-s2-wt:w-01abc-aaaaaaaaaaaa wts-s2-pr' <<<'$out'"
ok "no secrets in config" "! grep -qi 'secret_access_key\|access_key_id' <<<'$out'"
# end-to-end bundle: share with a weaver that copies the bundle aside, then fetch it into a clone
cat >"$t/bin/weaver" <<'W'
#!/usr/bin/env bash
printf '%q ' "$@" >>"$WTS_LOG"; echo >>"$WTS_LOG"
case "$*" in
  "store worktree publish"*) while [ $# -gt 0 ]; do [ "$1" = --bundle ] && cp "$2" "$WTS_STORE/bundle"; [ "$1" = --changes-z ] && cp "$2" "$WTS_STORE/changes.z"; shift; done ;;
  "store worktree fetch"*) while [ $# -gt 0 ]; do [ "$1" = --bundle-out ] && cp "$WTS_STORE/bundle" "$2"; shift; done ;;
  "project id"*) echo 01ABC ;;
esac
exit "${WEAVER_RC:-0}"
W
export WTS_STORE="$t/store"; mkdir -p "$WTS_STORE"; : >"$t/log"
cd "$r"; "$here/worktree-store.sh" share
ok "changes list passed to publish" "grep -q -- '--changes-z' '$t/log'"
ok "modified file recorded" "tr '\\0' '\\n' <'$t/store/changes.z' | grep -qx a.txt"
ok "untracked doc recorded as added" "tr '\\0' '\\n' <'$t/store/changes.z' | grep -qx docs/rooms.md"
git rm -qf a.txt; "$here/worktree-store.sh" share
ok "deletion recorded as D" "tr '\\0' '\\n' <'$t/store/changes.z' | grep -x -A1 D | grep -qx a.txt"
git checkout -q HEAD -- a.txt
clone="$t/clone"; git clone -q "$r" "$clone" 2>/dev/null; git -C "$clone" remote remove origin
"$here/worktree-store.sh" fetch feat-s2 "$clone" 2>/dev/null
ok "fetched commits appear under refs/shared" "git -C '$clone' rev-parse --verify -q refs/shared/feat-s2/feat/s2 >/dev/null"
"$here/worktree-store.sh" fetch feat-s2 "$clone" --bundle-ref refs/heads 2>/dev/null && rc=0 || rc=$?
ok "bundle ref outside refs/shared rejected" "[ $rc != 0 ]"
# follower verbs: strict, validated, correct weaver shape
: >"$t/log"
"$here/worktree-store.sh" search "rooms and doors"
ok "search passes the query as one argument" "grep -qF -- '--query rooms\\ and\\ doors' '$t/log'"
"$here/worktree-store.sh" leftovers --json; ok "leftovers --json forwarded" "grep -q 'leftovers --project 01ABC --json' '$t/log'"
"$here/worktree-store.sh" follow --state "$t/fs/state.json"; ok "follow creates the state dir" "[ -d '$t/fs' ]"
"$here/worktree-store.sh" leftovers --project X 2>/dev/null && rc=0 || rc=$?; ok "leftovers rejects extra flags" "[ $rc != 0 ]"
WTS_PROJECT='../x' "$here/worktree-store.sh" follow 2>/dev/null && rc=0 || rc=$?; ok "follow rejects a path-like project id" "[ $rc != 0 ]"
"$here/worktree-store.sh" overlaps 'a b' c 2>/dev/null && rc=0 || rc=$?; ok "overlaps validates slugs" "[ $rc != 0 ]"
"$here/worktree-store.sh" sync-docs ../x "$t/c" 2>/dev/null && rc=0 || rc=$?; ok "sync-docs rejects traversal slug" "[ $rc != 0 ]"
WEAVER_RC=1 "$here/worktree-store.sh" search q 2>/dev/null && rc=0 || rc=$?; ok "search failure is an error" "[ $rc != 0 ]"
echo "passed=$pass failed=$fail"; [ "$fail" = 0 ]
