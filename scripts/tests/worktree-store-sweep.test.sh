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
case "$*" in "store worktree publish"*) [ -z "${WEAVER_WARN:-}" ] || echo "$WEAVER_WARN" >&2; [ -z "${WEAVER_REPLY:-}" ] || printf '%s\n' "$WEAVER_REPLY" ;; esac
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
ok "a missing path makes no publish call" "[ \$(calls) = 0 ]"
ok "it tells the daemon once, with the first-missing time in ms (policy is the daemon's)" "[ \$(grep -c '^store worktree orphan' '$t/log') = 1 ] && grep -q -- 'orphan --project 01ABC --slug lane--b --since 1794650400000' '$t/log'"
: >"$t/log"; WTS_NOW=2026-11-15T10:00:00Z "$S" sweep >"$t/o"
ok "first-missing time is persisted, not refreshed" "grep -q 'missing_since=2026-11-14T10:00:00Z' '$t/o' && grep -q -- '--since 1794650400000' '$t/log'"
: >"$t/log"; WTS_DRY_RUN=1 "$S" sweep >"$t/o" 2>&1
ok "a dry run never calls orphan" "! grep -q orphan '$t/log' && ! grep -q 'DRY.*orphan' '$t/o'"
wd="$t/wt-e"; git -C "$r" worktree add -q -b lane/e "$wd" master; ( cd "$wd" && "$S" register >/dev/null ); rm -rf "$wd"; git -C "$r" worktree prune
: >"$t/log"; "$S" sweep >"$t/o"
ok "a worktree that never published has nothing to orphan: no call" "grep -q 'lane--e	orphan' '$t/o' && ! grep -q 'slug lane--e' '$t/log'"
: >"$t/log"; WEAVER_RC=1 "$S" sweep >"$t/o" 2>"$t/err" && rc=0 || rc=$?
ok "a failing orphan call warns and the sweep still exits 0 (best effort, like every store step)" "[ $rc = 0 ] && grep -q 'store step failed: orphan' '$t/err'"
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

# ---- branch change (design note Q9): default keeps the registered slug; --follow-branch re-derives it per sweep
cd "$r"; wd="$t/wt-d"; git worktree add -q -b lane/d "$wd" master; wf="$t/wt-f"; git worktree add -q -b lane/f "$wf" master
( cd "$wd" && "$S" register >/dev/null ); : >"$t/log"; "$S" sweep --only lane--d >/dev/null
git -C "$wd" switch -q -c other; echo o >"$wd/o.md"; : >"$t/log"; out=$("$S" sweep --only lane--d)
ok "DEFAULT: after a branch switch the sweep still publishes under the registered slug" "grep -q -- '--slug lane--d' '$t/log' && grep -q -- '--branch other' '$t/log' && ! grep -q 'store worktree create' '$t/log' && echo '$out' | grep -q '^lane--d	published'"
ok "DEFAULT: the sweep line says the branch drifted" "echo '$out' | grep -q 'branch-drift=lane/d->other'"
( cd "$wf" && "$S" register --follow-branch >/dev/null )
ok "register --follow-branch records the flag" "grep -q '^follow_branch=1\$' '$t/state/reg/01ABC/lane--f'"
: >"$t/log"; out=$("$S" sweep --only lane--f)
ok "follow: no drift note on a follow-branch registration" "! echo '$out' | grep -q branch-drift"
ok "follow: same branch = no create, one publish" "[ \$(calls) = 1 ] && ! grep -q 'store worktree create' '$t/log'"
git -C "$wf" switch -q -c feat/g; echo g >"$wf/docs-g.md"; : >"$t/log"; out=$("$S" sweep --only lane--f)
ok "follow: a moved slug creates the new store, then publishes to it" "[ \$(sed -n 1p '$t/log' | grep -c 'store worktree create') = 1 ] && sed -n 1p '$t/log' | grep -q -- '--slug feat--g --branch feat/g' && sed -n 2p '$t/log' | grep -q 'store worktree publish.*--slug feat--g'"
ok "follow: the line reports the move" "echo '$out' | grep -q 'feat--g	published	slug-moved=lane--f->feat--g'"
ok "follow: registration now says feat--g and remembers lane--f" "grep -q '^slug=feat--g\$' '$t/state/reg/01ABC/lane--f' && grep -q '^prev_slug=lane--f\$' '$t/state/reg/01ABC/lane--f'"
ok "follow: nothing published under the old slug" "! grep -q -- '--slug lane--f ' '$t/log'"
: >"$t/log"; "$S" sweep --only feat--g >/dev/null; ok "follow: then idle again (zero calls)" "[ ! -s '$t/log' ]"
git -C "$wf" switch -q lane/f; : >"$t/log"; "$S" sweep >/dev/null
ok "follow: switching back re-creates (idempotent) and publishes to the old slug" "grep -q 'store worktree create.*--slug lane--f' '$t/log' && grep -q 'store worktree publish.*--slug lane--f' '$t/log'"
git -C "$wf" switch -q -c feat/h; echo h >"$wf/h.md"; : >"$t/log"; WEAVER_RC=1 "$S" sweep --only lane--f >"$t/o" 2>/dev/null || true
ok "follow: a failed create does not publish and is retried" "grep -q 'lane--f	failed' '$t/o' && ! grep -q 'store worktree publish' '$t/log'"
: >"$t/log"; "$S" sweep --only lane--f >/dev/null; ok "follow: the retry creates and publishes" "grep -q 'create.*--slug feat--h' '$t/log' && [ \$(calls) = 1 ]"
git -C "$wf" switch -q --detach; : >"$t/log"; out=$("$S" sweep --only feat--h)
ok "follow: a detached HEAD is reported, nothing is published" "echo '$out' | grep -q 'feat--h	failed	cannot derive' && [ ! -s '$t/log' ]"
git -C "$wf" switch -q -C lane/d; : >"$t/log"; out=$("$S" sweep --only feat--h)
ok "follow: a slug already registered elsewhere is refused" "echo '$out' | grep -q 'failed	slug lane--d is already registered' && [ ! -s '$t/log' ]"

# ---- loud skips (scale-1): a publish that skipped files must say so, without changing what is published
cd "$wa"; echo s1 >>"$wa/docs/plan.md"
NOSKIP='{"uploaded":1,"unchanged":0,"removed":0,"skipped":[],"bundle_uploaded":false,"handoff":false}'
WEAVER_REPLY=$NOSKIP "$S" sweep >"$t/o" 2>"$t/e"; rc=$?
ok "no skips: plain published, no warning, exit 0" "[ $rc = 0 ] && grep -q 'lane--a	published	head=[0-9a-f]*$' '$t/o' && ! grep -qi 'not shared\|denylist' '$t/e'"
DENY='{"uploaded":1,"skipped":[".env: Secret","keys/id_rsa: Secret"]}'
echo s2 >>"$wa/docs/plan.md"
WEAVER_REPLY=$DENY "$S" sweep >"$t/o" 2>"$t/e"; rc=$?
ok "denylist-only skips: published (not partial), counted as expected, exit 0" "[ $rc = 0 ] && grep -q 'lane--a	published	.*skipped_secret=2' '$t/o' && ! grep -q 'published-partial' '$t/o' && grep -q 'note: 2 path(s) matched the secret denylist' '$t/e' && ! grep -q 'WARNING' '$t/e'"
BUD='{"uploaded":1,"skipped":["big/a.bin: TooLarge","big/b.bin: OverBudget","c.txt: Secret"]}'
echo s3 >>"$wa/docs/plan.md"
WEAVER_REPLY=$BUD "$S" sweep >"$t/o" 2>"$t/e"; rc=$?
ok "budget skips: published-partial with counts, exit 0" "[ $rc = 0 ] && grep -q 'lane--a	published-partial	.*skipped_budget=2 skipped_other=0 skipped_secret=1' '$t/o'"
ok "budget skips: warning names the paths and says followers are missing" "grep -q 'WARNING: published, but 2 file(s) were NOT shared' '$t/e' && grep -q 'not shared (TooLarge): big/a.bin' '$t/e' && grep -q 'not shared (OverBudget): big/b.bin' '$t/e' && ! grep -q 'not shared (Secret)' '$t/e'"
ok "the partial publish is recorded (digest stored: next sweep is idle)" ": >'$t/log'; $S sweep --only lane--a >/dev/null 2>&1; [ ! -s '$t/log' ]"
MANY=$(perl -MJSON::PP -e 'print encode_json({skipped=>[map {"f$_.bin: OverBudget"} 1..13]})')
echo s4 >>"$wa/docs/plan.md"; WEAVER_REPLY=$MANY "$S" sweep >"$t/o" 2>"$t/e"
ok "long skip lists are capped at 10 then 'and N more'" "[ \$(grep -c 'not shared (OverBudget)' '$t/e') = 10 ] && grep -q '\.\.\. and 3 more' '$t/e'"
echo s5 >>"$wa/docs/plan.md"
WEAVER_REPLY=$BUD WTS_STRICT_SKIPS=1 "$S" sweep >"$t/o" 2>"$t/e" && rc=0 || rc=$?
ok "WTS_STRICT_SKIPS=1 + budget skips: still published-partial, exit non-zero" "[ $rc != 0 ] && grep -q 'lane--a	published-partial' '$t/o' && grep -q 'WTS_STRICT_SKIPS=1' '$t/e'"
echo s6 >>"$wa/docs/plan.md"
WEAVER_REPLY=$DENY WTS_STRICT_SKIPS=1 "$S" sweep >"$t/o" 2>"$t/e" && rc=0 || rc=$?
ok "WTS_STRICT_SKIPS=1 + denylist-only skips: exit 0" "[ $rc = 0 ]"
echo s7 >>"$wa/docs/plan.md"
WEAVER_REPLY=$BUD "$S" sweep --json >"$t/o" 2>/dev/null
ok "sweep --json reports the partial status" "grep -q '\"status\":\"published-partial\"' '$t/o'"
# share on its own
cd "$wa"; echo s8 >>"$wa/docs/plan.md"
WEAVER_REPLY=$BUD "$S" share >"$t/o" 2>"$t/e"; rc=$?
ok "share: warns on stderr, still prints the reply, exit 0" "[ $rc = 0 ] && grep -q 'NOT shared' '$t/e' && grep -q 'TooLarge' '$t/o'"
WEAVER_REPLY=$BUD WTS_STRICT_SKIPS=1 "$S" share >/dev/null 2>&1 && rc=0 || rc=$?
ok "share: WTS_STRICT_SKIPS=1 exits 3 on budget skips" "[ $rc = 3 ]"
WEAVER_REPLY=$NOSKIP WTS_STRICT_SKIPS=1 "$S" share >"$t/o" 2>"$t/e"; rc=$?
ok "share: nothing skipped, strict on: exit 0, silent stderr" "[ $rc = 0 ] && [ ! -s '$t/e' ]"

# ---- older-writer warnings (lane F1): the weaver's stderr line is forwarded once per affected worktree
cd "$wa"; echo f1a >>"$wa/docs/plan.md"
WARN='warning: worktree lane--a was last written by an older daemon (the record was rewritten). Upgrade that writer before a client project uses shared worktrees (ADR-126 section 4a, gate item 8).'
WEAVER_WARN=$WARN WEAVER_REPLY=$NOSKIP "$S" sweep >"$t/o" 2>"$t/e"; rc=$?
ok "sweep forwards the older-writer warning once, status published, exit 0" "[ $rc = 0 ] && [ \$(grep -c 'gate item 8' '$t/e') = 1 ] && grep -q 'lane--a	published' '$t/o' && ! grep -q 'gate item 8' '$t/o'"
echo f1b >>"$wa/docs/plan.md"
WEAVER_WARN=$WARN WEAVER_REPLY=$NOSKIP "$S" sweep --json >"$t/o" 2>/dev/null; rc=$?
ok "sweep --json: the warning does not touch the JSON on stdout" "[ $rc = 0 ] && grep -q '\"status\":\"published\"' '$t/o' && ! grep -q 'gate item 8' '$t/o'"
echo f1c >>"$wa/docs/plan.md"
WEAVER_REPLY=$NOSKIP "$S" sweep >"$t/o" 2>"$t/e"
ok "no warning from a clean weaver" "! grep -q 'gate item 8' '$t/e'"

echo "passed=$pass failed=$fail"; [ "$fail" = 0 ]
