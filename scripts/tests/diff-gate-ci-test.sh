#!/usr/bin/env bash
# diff-gate-ci-test.sh — fixture tests for the CI step in .github/workflows/diff-gate.yml that
# picks the trusted gate copy. The step's `run:` block is extracted from the workflow file and
# run as-is against throwaway remotes and clones under target/, simulating push, new-branch,
# force-push, workflow_dispatch and pull_request events.
# Run: scripts/tests/diff-gate-ci-test.sh

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
WORKFLOW="${DIFF_GATE_WORKFLOW:-$ROOT/.github/workflows/diff-gate.yml}"  # override only to mutation-test
GATE="$ROOT/scripts/diff-gate.sh"
mkdir -p "$ROOT/target"
WORK="$(mktemp -d "$ROOT/target/diff-gate-ci-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# The step script: the `run: |` block of the "Run diff gate" step, de-indented.
STEP="$WORK/step.sh"
awk '
/^      - name: Run diff gate/ { instep = 1; next }
instep && /^      - / { instep = 0 }
instep && /^        run: \|/ { inrun = 1; next }
inrun {
    if ($0 ~ /^[ \t]*$/) { print ""; next }
    if ($0 !~ /^          /) { inrun = 0; next }
    print substr($0, 11)
}' "$WORKFLOW" >"$STEP"
grep -q 'exec bash "\$RUNNER_TEMP/diff-gate.sh"' "$STEP" || { echo "diff-gate-ci-test: cannot extract the step from $WORKFLOW" >&2; exit 2; }

# A gh stub so the pull_request label lookup never reaches the network.
mkdir -p "$WORK/bin" && printf '#!/bin/sh\nexit 1\n' >"$WORK/bin/gh" && chmod +x "$WORK/bin/gh"

pass=0; fail=0
EVIL_GATE='#!/usr/bin/env bash
exit 0'
UNSAFE='fn f() { unsafe { g() } }'

check_result() { # NAME WANT RC OUT; EXPECT="rules" must be FAIL findings, GREP="text" must appear
    local name="$1" want="$2" rc="$3" out="$4" r missing=""
    for r in ${EXPECT:-}; do
        echo "$out" | grep -Eq "^  FAIL +$r +" || missing="$missing $r"
    done
    if [ -n "${GREP:-}" ] && ! echo "$out" | grep -qF -- "$GREP"; then missing="$missing text:'$GREP'"; fi
    if [ "$rc" = "$want" ] && [ -z "$missing" ]; then pass=$((pass + 1)); printf '  ok    %s\n' "$name"
    else fail=$((fail + 1)); printf '  FAIL  %s (exit %s, wanted %s; missing:%s)\n%s\n' "$name" "$rc" "$want" "${missing:- none}" "$out" | sed 's/^/        /'; fi
}

# fresh NAME: a bare remote ($R) and a work clone ($W, cwd) with the real gate committed on the
# protected branch target-0.9 as commit $T0, pushed. master exists without the gate ($M0).
fresh() {
    local d="$WORK/$(echo "$1" | tr -c 'A-Za-z0-9\n' '_')"
    R="$d/remote.git"; W="$d/work"
    git init -q --bare "$R" && git -C "$R" symbolic-ref HEAD refs/heads/target-0.9 && git init -q "$W" && cd "$W" || return 1
    git config user.email t@t && git config user.name t
    mkdir -p src config scripts && printf 'pub fn a() {}\n' >src/lib.rs
    printf '[package]\nname = "fx"\n\n[dependencies]\nexisting = "1"\n' >Cargo.toml
    git add -A && git commit -qm m0 && git branch -q -M master && M0="$(git rev-parse HEAD)"
    git checkout -q -b target-0.9 && cp "$GATE" scripts/diff-gate.sh && git add -A && git commit -qm t0
    T0="$(git rev-parse HEAD)"
    git remote add origin "$R" && git push -q origin master target-0.9 2>/dev/null
}

commit() { # MSG SHELL: run SHELL in $W, commit, print the sha
    eval "$2" && git add -A && git commit -qm "$1" && git rev-parse HEAD
}

# ci_run NAME WANT EVENT BRANCH BEFORE SHA [PR_BASE]: clone $R the way actions/checkout does
# (all branches as origin/*, detached at SHA) and run the extracted step there.
# PRREF sets the pull request's base branch name; DEFBR the repository default branch (master).
ci_run() {
    local name="$1" want="$2" ev="$3" br="$4" before="$5" sha="$6" prbase="${7:-}" c out rc
    c="$(mktemp -d "$WORK/ci.XXXXXX")"
    git clone -q "$R" "$c/repo" && git -C "$c/repo" checkout -q --detach "$sha" || { check_result "$name" "$want" 99 "clone failed"; return; }
    mkdir -p "$c/tmp"
    out="$(cd "$c/repo" && env -u GITHUB_STEP_SUMMARY -u DIFF_GATE_OWNER_APPROVED -u DIFF_GATE_BASE -u DIFF_GATE_HEAD \
        PATH="$WORK/bin:$PATH" RUNNER_TEMP="$c/tmp" GH_TOKEN="" REPO="fixture/none" OWNER_VAR="" \
        EVENT_NAME="$ev" PUSH_REF="refs/heads/$br" PUSH_BEFORE="$before" PR_BASE_SHA="$prbase" PR_NUMBER="1" \
        PR_BASE_REF="${PRREF:-}" DEFAULT_BRANCH="${DEFBR:-master}" bash "$STEP" 2>&1)"; rc=$?
    check_result "$name" "$want" "$rc" "$out"
    cd "$W" || exit 2
}

echo "diff-gate CI step fixtures"

# 1. Protected-branch push that rewrites the gate to `exit 0`: the trusted copy still runs.
fresh evil_protected
T1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin target-0.9 2>/dev/null
out="$(bash scripts/diff-gate.sh --base "$T0" 2>&1)"; rc=$?
check_result "control: the pushed gate copy alone passes its own push" 0 "$rc" "$out"
EXPECT="ci-workflow unsafe" GREP="previous tip of protected target-0.9" \
    ci_run "push: gate edited to exit 0 still FAILS (trusted copy from before)" 1 push target-0.9 "$T0" "$T1"

# 2. Normal protected pushes.
fresh plain_protected
T1="$(commit unsafe "echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin target-0.9 2>/dev/null
EXPECT="unsafe" ci_run "push: real content finding FAILS" 1 push target-0.9 "$T0" "$T1"
fresh clean_protected
T1="$(commit docs 'echo hi >README.md')"
git push -q origin target-0.9 2>/dev/null
GREP="diff-gate: OK" ci_run "push: clean push passes" 0 push target-0.9 "$T0" "$T1"

# 3. Unprotected branch: before is not trusted, so two pushes cannot launder a weakened gate.
fresh launder
git checkout -q -b feat/x
F1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh")"
F2="$(commit unsafe "echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin feat/x 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="push to unprotected feat/x" \
    ci_run "push: second push on top of a weakened gate still FAILS" 1 push feat/x "$F1" "$F2"

# 4. Ledger laundering on an unprotected branch: an entry from an earlier push does not count.
fresh ledger_launder
git checkout -q -b feat/l
row='[[approval]]
rule = "unsafe"
path = "src/**"
pattern = "*"
reason = "fixture"
approved_by = "owner"'
h="$(printf 'rule=unsafe\npath=src/**\npattern=*\nreason=fixture\napproved_by=owner\n' | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1)"
F1="$(commit ledger "printf '[[entry]]\ndecision = \"X\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n' $h >config/diff-gate-owner-ledger.toml")"
F2="$(commit row "printf '%s\n' \"\$row\" >config/diff-gate-approvals.toml; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin feat/l 2>/dev/null
EXPECT="approvals-change" ci_run "push: ledger entry from an earlier unprotected push does not approve" 1 push feat/l "$F1" "$F2"

# 5. New branches (before = 0000...).
Z=0000000000000000000000000000000000000000
fresh new_branch
git checkout -q -b feat/n
F1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin feat/n 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="newest protected branch tip" \
    ci_run "new branch: judged against the newest protected tip, FAILS" 1 push feat/n "$Z" "$F1"
fresh new_branch_clean
git checkout -q -b feat/c
F1="$(commit docs 'echo hi >README.md')"
git push -q origin feat/c 2>/dev/null
GREP="diff-gate: OK" ci_run "new branch: clean branch passes" 0 push feat/c "$Z" "$F1"
fresh new_protected
git checkout -q -b target-0.10
P1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin target-0.10 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="new protected branch target-0.10: newest other protected branch tip" \
    ci_run "new protected branch: never trusts itself, FAILS" 1 push target-0.10 "$Z" "$P1"

# 6. Force-push to a protected branch.
fresh force_push
T1="$(commit docs 'echo hi >README.md')"
git push -q origin target-0.9 2>/dev/null && git push -q origin target-0.9:keep/old 2>/dev/null
git reset -q --hard "$T0"
T1b="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q -f origin target-0.9 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="force-push: diff from the merge-base" \
    ci_run "force-push: before not an ancestor, judged from the merge-base, FAILS" 1 push target-0.9 "$T1" "$T1b"
git reset -q --hard "$T0"
T1c="$(commit docs2 'echo bye >README.md')"
git push -q -f origin target-0.9 2>/dev/null
GREP="diff-gate: OK" ci_run "force-push: clean rewrite passes" 0 push target-0.9 "$T1" "$T1c"
fresh force_push_gone
T1="$(commit docs 'echo hi >README.md')"
git push -q origin target-0.9 2>/dev/null
git reset -q --hard "$T0"
T1b="$(commit docs2 'echo bye >README.md')"
git push -q -f origin target-0.9 2>/dev/null
git -C "$R" reflog expire --expire=now --all 2>/dev/null; git -C "$R" gc -q --prune=now 2>/dev/null
GREP="is not in the clone" ci_run "force-push: before gone from the clone fails closed" 2 push target-0.9 "$T1" "$T1b"

# 7. workflow_dispatch on an unprotected branch.
fresh dispatch
git checkout -q -b feat/d
F1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin feat/d 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="workflow_dispatch on feat/d" \
    ci_run "workflow_dispatch: judged against the newest protected tip, FAILS" 1 workflow_dispatch feat/d "" "$F1"

# 8. Pull requests.
fresh pr
git checkout -q -b feat/p
F1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin feat/p 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="pull request base" PRREF=target-0.9 \
    ci_run "pull_request: base copy runs, FAILS" 1 pull_request feat/p "" "$F1" "$T0"
fresh pr_old_base
git checkout -q -b feat/o "$M0"
F1="$(commit evil "mkdir -p scripts; printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh; echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin feat/o 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="trusted gate from $(git rev-parse --short "$T0")" PRREF=master \
    ci_run "pull_request: base without a gate uses another protected branch's, FAILS" 1 pull_request feat/o "" "$F1" "$M0"

# 9. G4 (2026-11-15): a pull request into an UNPROTECTED branch is judged against the newest
#    protected tip, not that branch's own gate (wasm-browser.yml runs on PRs into any branch).
fresh pr_unprotected_base
git checkout -q -b feat/base
B1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh")"
git checkout -q -b feat/p2
P1="$(commit unsafe "echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin feat/base feat/p2 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="pull request into unprotected feat/base" PRREF=feat/base \
    ci_run "pull_request into an unprotected branch: its weakened gate is not trusted, FAILS" 1 pull_request feat/p2 "" "$P1" "$B1"
EXPECT="ci-workflow unsafe" GREP="pull request into unprotected" \
    ci_run "pull_request with no base branch name is treated as unprotected, FAILS" 1 pull_request feat/p2 "" "$P1" "$B1"
grep -q 'uses: ./.github/workflows/diff-gate.yml' "$ROOT/.github/workflows/wasm-browser.yml" \
    && pass=$((pass + 1)) && echo "  ok    wasm-browser.yml (PRs into any branch) runs this reusable gate step" \
    || { fail=$((fail + 1)); echo "  FAIL  wasm-browser.yml does not call diff-gate.yml"; }

# 10. G4: `main` does not exist in this repo, so it is not protected unless it is the default
#     branch; whoever creates it first does not become a trust root.
fresh self_made_main
git checkout -q -b main
N1="$(commit evil "printf '%s\n' \"\$EVIL_GATE\" >scripts/diff-gate.sh")"
git push -q origin main 2>/dev/null
N2="$(commit unsafe "echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin main 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="push to unprotected main" \
    ci_run "main created by a push: a later push is not judged by its gate, FAILS" 1 push main "$N1" "$N2"
git checkout -q -b feat/m && N3="$(commit unsafe2 "echo \"\$UNSAFE\" >>src/lib.rs")" && git push -q origin feat/m 2>/dev/null
EXPECT="ci-workflow unsafe" GREP="pull request into unprotected main" PRREF=main \
    ci_run "pull_request into a self-made main: its gate is not trusted, FAILS" 1 pull_request feat/m "" "$N3" "$N2"
fresh main_default
git checkout -q -b main && git push -q origin main 2>/dev/null && N0="$(git rev-parse HEAD)"
N1="$(commit unsafe "echo \"\$UNSAFE\" >>src/lib.rs")"
git push -q origin main 2>/dev/null
EXPECT="unsafe" GREP="previous tip of protected main" DEFBR=main \
    ci_run "main as the repository default branch is protected" 1 push main "$N0" "$N1"

cd "$ROOT" || exit 2
echo "diff-gate CI step fixtures: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
