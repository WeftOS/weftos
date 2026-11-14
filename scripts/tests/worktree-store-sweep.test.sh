#!/usr/bin/env bash
# WO-069 trigger tests: register / sweep / install-hook of scripts/worktree-store.sh (real git, stub weaver).
set -euo pipefail
here=$(cd "$(dirname "$0")/.." && pwd)
t=$(mktemp -d); trap 'rm -rf "$t"' EXIT
pass=0; fail=0
ok() { if eval "$2"; then pass=$((pass+1)); else fail=$((fail+1)); echo "FAIL: $1"; fi; }
S="$here/worktree-store.sh"

mkdir -p "$t/bin"
cat >"$t/bin/weaver" <<'W'
#!/usr/bin/env bash
printf '%q ' "$@" >>"$WTS_LOG"; echo >>"$WTS_LOG"
exit "${WEAVER_RC:-0}"
W
chmod +x "$t/bin/weaver"
export WTS_WEAVER="$t/bin/weaver" WTS_LOG="$t/log" WTS_PROJECT=01ABC WTS_STATE_DIR="$t/state"
export GIT_CONFIG_GLOBAL="$t/gitconfig" GIT_CONFIG_NOSYSTEM=1 AGF_TARGET_REF=master
unset WTS_SLUG AGF_HOOK_BRANCH AGF_HOOK_SLUG
: >"$t/log"
calls() { grep -c '^store worktree publish' "$t/log" || true; }

r="$t/repo"; git init -q -b master "$r"; cd "$r"
git config user.email t@t; git config user.name t
echo a >a.txt; echo '*.log' >.gitignore; git add .; git commit -qm base
wa="$t/wt-a"; git worktree add -q -b lane/a "$wa" master
wb="$t/wt-b"; git worktree add -q -b lane/b "$wb" master
mkdir -p "$wa/docs"; echo "# plan" >"$wa/docs/plan.md"   # untracked doc: the card's case

# ---- register / registered / unregister
cd "$wa"; "$S" register >/dev/null
ok "register records the worktree" "[ -f '$t/state/reg/01ABC/lane--a' ] && grep -q \"^path=\$(cd '$wa' && pwd -P)\$\" '$t/state/reg/01ABC/lane--a'"
ok "registered lists it" "$S registered | grep -q '^lane--a	01ABC	'"
ok "registered --json is a JSON array with the slug" "$S registered --json | grep -q '^\[{\"slug\":\"lane--a\"'"
"$S" register 2>"$t/err" >/dev/null; ok "re-register is a no-op notice" "grep -q 'already registered' '$t/err'"
( cd "$wb" && "$S" register --slug custom-b >/dev/null )
ok "register --slug overrides" "[ -f '$t/state/reg/01ABC/custom-b' ]"
"$S" unregister custom-b >/dev/null
ok "unregister removes it" "[ ! -e '$t/state/reg/01ABC/custom-b' ] && ! $S registered | grep -q custom-b"
"$S" register --path /nonexistent 2>/dev/null && rc=0 || rc=$?; ok "register refuses a non-worktree" "[ $rc != 0 ]"

# ---- sweep: publish once, then a no-op
: >"$t/log"; out=$("$S" sweep)
ok "first sweep publishes" "[ \$(calls) = 1 ] && echo '$out' | grep -q 'lane--a	published'"
ok "publish carries the registered slug and project" "grep -q -- '--slug lane--a' '$t/log' && grep -q -- '--project 01ABC' '$t/log'"
: >"$t/log"; out=$("$S" sweep)
ok "IDEMPOTENT: second sweep makes zero weaver calls" "[ ! -s '$t/log' ]"
ok "second sweep reports unchanged" "echo '$out' | grep -q 'lane--a	unchanged'"
: >"$t/log"; WTS_DRY_RUN=1 "$S" sweep >"$t/o2" 2>&1
ok "IDEMPOTENT under WTS_DRY_RUN: no DRY publish line" "! grep -q DRY '$t/o2' && grep -q unchanged '$t/o2'"

# ---- change detection
echo x >"$wa/docs/plan.md"; : >"$t/log"; "$S" sweep >/dev/null
ok "editing a doc republishes once" "[ \$(calls) = 1 ]"
: >"$t/log"; "$S" sweep >/dev/null; ok "and then settles" "[ ! -s '$t/log' ]"
echo "# new" >"$wa/docs/new.md"; : >"$t/log"; "$S" sweep >/dev/null; ok "a new untracked doc republishes" "[ \$(calls) = 1 ]"
echo junk >"$wa/ignored.log"; : >"$t/log"; "$S" sweep >/dev/null; ok "an ignored file does not" "[ ! -s '$t/log' ]"
rm "$wa/docs/new.md"; : >"$t/log"; "$S" sweep >/dev/null; ok "deleting a file republishes" "[ \$(calls) = 1 ]"
( cd "$wa" && git add -A && git commit -qm c1 ); : >"$t/log"; "$S" sweep >/dev/null
ok "a commit (new head) republishes" "[ \$(calls) = 1 ]"
: >"$t/log"; "$S" sweep >/dev/null; ok "and settles" "[ ! -s '$t/log' ]"
head -c 1100000 /dev/zero >"$wa/big.bin"; : >"$t/log"; "$S" sweep >/dev/null; ok "a >1 MiB file is seen (size+mtime)" "[ \$(calls) = 1 ]"
touch -t 202001010000 "$wa/big.bin"; : >"$t/log"; "$S" sweep >/dev/null; ok "its mtime change republishes" "[ \$(calls) = 1 ]"

# ---- --only and failures
( cd "$wb" && "$S" register >/dev/null ); : >"$t/log"; "$S" sweep --only lane--b >/dev/null
ok "--only sweeps just that slug" "[ \$(calls) = 1 ] && grep -q -- '--slug lane--b' '$t/log' && ! grep -q -- '--slug lane--a' '$t/log'"
echo y >>"$wb/a.txt"; : >"$t/log"
WEAVER_RC=1 "$S" sweep >"$t/o" 2>/dev/null && rc=0 || rc=$?
ok "a failed publish exits 0 and reports failed" "[ $rc = 0 ] && grep -q 'lane--b	failed' '$t/o'"
: >"$t/log"; "$S" sweep >/dev/null; ok "a failed publish is retried (digest not stored)" "[ \$(calls) = 1 ]"
echo z >>"$wb/a.txt"; WEAVER_RC=1 WTS_STRICT=1 "$S" sweep >/dev/null 2>&1 && rc=0 || rc=$?
ok "WTS_STRICT=1 makes a failed publish fatal" "[ $rc != 0 ]"
"$S" sweep >/dev/null

# ---- orphan: mark only, persisted, no store call, no deletion
rm -rf "$wb"; git -C "$r" worktree prune
: >"$t/log"; WTS_NOW=2026-11-14T10:00:00Z "$S" sweep >"$t/o"
ok "removed path is reported orphan with last head and first-missing time" "grep -q 'lane--b	orphan	last_head=[0-9a-f]\{40\} .*missing_since=2026-11-14T10:00:00Z' '$t/o'"
ok "orphan marking makes no weaver call" "[ ! -s '$t/log' ] || ! grep -q lane--b '$t/log'"
WTS_NOW=2026-11-15T10:00:00Z "$S" sweep >"$t/o"
ok "first-missing time is persisted, not refreshed" "grep -q 'missing_since=2026-11-14T10:00:00Z' '$t/o'"
ok "the registration survives (nothing deleted)" "[ -f '$t/state/reg/01ABC/lane--b' ]"
ok "sweep --json carries the orphan status" "$S sweep --only lane--b --json | grep -q '\"status\":\"orphan\"'"

# ---- hooks, shared mode (no extensions.worktreeConfig)
cd "$wa"; hk="$r/.git/hooks/post-commit"
"$S" install-hook >/dev/null
ok "shared hook installed in the common hooks dir" "grep -q 'wts-managed' '$hk' && [ -x '$hk' ]"
echo hooked >"$wa/docs/h.md"; git add -A; : >"$t/log"; WTS_HOOK_SYNC=1 git commit -qm h
ok "commit in a registered worktree sweeps it" "[ \$(calls) = 1 ] && grep -q -- '--slug lane--a' '$t/log'"
wc2="$t/wt-c"; git -C "$r" worktree add -q -b lane/c "$wc2" master
( cd "$wc2" && echo c >c.txt && git add c.txt && : >"$t/log" && WTS_HOOK_SYNC=1 git commit -qm c )
ok "commit in an UNREGISTERED worktree does nothing" "[ ! -s '$t/log' ]"
"$S" uninstall-hook >/dev/null; ok "uninstall removes the shared hook" "[ ! -e '$hk' ]"
printf '#!/bin/sh\necho foreign >>"%s/foreign"\n' "$t" >"$hk"; chmod +x "$hk"; cp "$hk" "$t/before"
"$S" install-hook 2>"$t/err" && rc=0 || rc=$?
ok "a foreign hook is refused, not overwritten" "[ $rc != 0 ] && cmp -s '$hk' '$t/before' && grep -q -- '--chain' '$t/err'"
"$S" install-hook --chain >/dev/null
echo more >>"$wa/docs/h.md"; git add -A; : >"$t/log"; WTS_HOOK_SYNC=1 git commit -qm h2
ok "--chain keeps the foreign hook running AND sweeps" "[ -f '$t/foreign' ] && [ \$(calls) = 1 ]"
"$S" uninstall-hook >/dev/null
ok "uninstall restores the foreign hook byte for byte" "cmp -s '$hk' '$t/before'"
rm -f "$hk"; rm -f "$t/foreign"

# ---- hooks, per-worktree mode (extensions.worktreeConfig)
git -C "$r" config extensions.worktreeConfig true
printf '#!/bin/sh\necho foreign >>"%s/foreign"\n' "$t" >"$hk"; chmod +x "$hk"
printf '#!/bin/sh\nexit 0\n' >"$r/.git/hooks/pre-commit"; chmod +x "$r/.git/hooks/pre-commit"
cd "$wa"; "$S" install-hook >/dev/null
gd=$(git rev-parse --absolute-git-dir)
ok "per-worktree core.hooksPath points at this worktree's wts-hooks" "[ \"\$(git config --worktree --get core.hooksPath)\" = '$gd/wts-hooks' ]"
ok "the shared hooks dir is untouched (foreign hook still foreign)" "! grep -q wts-managed '$hk'"
ok "other hooks are carried over by symlink" "[ -L '$gd/wts-hooks/pre-commit' ]"
ok "the other worktree is not affected" "[ -z \"\$(git -C '$wc2' config --get core.hooksPath || true)\" ]"
echo wt >"$wa/docs/h.md"; git add -A; : >"$t/log"; rm -f "$t/foreign"; WTS_HOOK_SYNC=1 git commit -qm h3
ok "per-worktree hook sweeps and still runs the foreign post-commit" "[ \$(calls) = 1 ] && [ -f '$t/foreign' ]"
"$S" uninstall-hook >/dev/null
ok "uninstall unsets core.hooksPath and removes wts-hooks" "[ -z \"\$(git config --worktree --get core.hooksPath || true)\" ] && [ ! -d '$gd/wts-hooks' ]"
ok "uninstall leaves the foreign shared hook alone" "grep -q foreign '$hk'"
( cd "$wc2" && "$S" install-hook 2>/dev/null ) && rc=0 || rc=$?; ok "install-hook refuses an unregistered worktree" "[ $rc != 0 ]"

echo "passed=$pass failed=$fail"; [ "$fail" = 0 ]
