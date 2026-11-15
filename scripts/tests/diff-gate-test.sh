#!/usr/bin/env bash
# diff-gate-test.sh — fixture tests for scripts/diff-gate.sh.
# Builds throwaway git repos under target/ in this checkout and deletes them after.
# Run: scripts/tests/diff-gate-test.sh

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="$ROOT/scripts/diff-gate.sh"
mkdir -p "$ROOT/target"
WORK="$(mktemp -d "$ROOT/target/diff-gate-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

pass=0; fail=0
APPROVE_UNSAFE='[[approval]]
rule = "unsafe"
path = "src/**"
pattern = "*"
reason = "fixture"
approved_by = "owner"'

# case NAME EXPECTED_EXIT BASE_SNIPPET CHANGE_SNIPPET [ENV=VAL ...]
# Snippets run inside the fixture repo: BASE_SNIPPET before the base commit,
# CHANGE_SNIPPET before the change commit.
case_run() {
    local name="$1" want="$2" base_sh="$3" change_sh="$4"; shift 4
    local d="$WORK/$(echo "$name" | tr -c 'A-Za-z0-9\n' '_')"
    mkdir -p "$d" && cd "$d" || return
    git init -q . && git config user.email t@t && git config user.name t
    mkdir -p src config && printf 'pub fn a() {}\n' >src/lib.rs
    printf '[package]\nname = "fx"\n\n[dependencies]\nexisting = "1"\n' >Cargo.toml
    eval "$base_sh"
    git add -A && git commit -qm base && local base; base="$(git rev-parse HEAD)"
    eval "$change_sh"
    git add -A && git commit -qm change
    local out rc
    out="$(env DIFF_GATE_BASE="$base" "$@" "$GATE" 2>&1)"; rc=$?
    check_result "$name" "$want" "$rc" "$out"
    cd "$ROOT" || exit 2
}

# check_result NAME WANT RC OUT. With EXPECT="rule1 rule2" set, each rule must also be
# reported as a FAIL finding in OUT.
check_result() {
    local name="$1" want="$2" rc="$3" out="$4" r missing=""
    for r in ${EXPECT:-}; do
        echo "$out" | grep -Eq "^  FAIL +$r +" || missing="$missing $r"
    done
    if [ "$rc" = "$want" ] && [ -z "$missing" ]; then pass=$((pass + 1)); printf '  ok    %s\n' "$name"
    else fail=$((fail + 1)); printf '  FAIL  %s (exit %s, wanted %s; missing FAIL:%s)\n%s\n' "$name" "$rc" "$want" "${missing:- none}" "$out" | sed 's/^/        /'; fi
}

# fixture_repo DIR: an empty fixture repo with src/lib.rs, Cargo.toml and config/.
fixture_repo() {
    mkdir -p "$1" && cd "$1" || return 1
    git init -q . && git config user.email t@t && git config user.name t
    mkdir -p src config && printf 'pub fn a() {}\n' >src/lib.rs
    printf '[package]\nname = "fx"\n\n[dependencies]\nexisting = "1"\n' >Cargo.toml
}

echo "diff-gate fixtures"
# clean and comments/strings
case_run "clean docs-only diff passes" 0 : 'echo hi >README.md'
case_run "unsafe in a comment passes" 0 : 'echo "// unsafe { x }" >>src/lib.rs'
case_run "unsafe in a string passes" 0 : 'echo "const S: &str = \"unsafe\";" >>src/lib.rs'
case_run "unsafe_code attribute passes" 0 : 'echo "#![forbid(unsafe_code)]" >>src/lib.rs'

# each rule: added -> fail, already on base -> pass
case_run "unsafe added fails" 1 : 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
case_run "unsafe on base passes" 0 'echo "fn f() { unsafe { g() } }" >>src/lib.rs' 'echo "fn z() {}" >>src/lib.rs'
case_run "new-dep added fails" 1 : 'printf "serde = \"1\"\n" >>Cargo.toml'
case_run "new-dep already on base passes" 0 'printf "serde = \"1\"\n" >>Cargo.toml' 'sed -i.bak "s/serde = \"1\"/serde = \"1.1\"/" Cargo.toml; rm Cargo.toml.bak'
case_run "new-dep changed git source fails" 1 'printf "foo = { git = \"https://a\" }\n" >>Cargo.toml' 'sed -i.bak "s#https://a#https://b#" Cargo.toml; rm Cargo.toml.bak'
case_run "new-dep dev table fails" 1 : 'printf "\n[dev-dependencies]\ntempfile = \"3\"\n" >>Cargo.toml'
case_run "new-dep table form fails" 1 : 'printf "\n[dependencies.tokio]\nversion = \"1\"\n" >>Cargo.toml'
case_run "new-dep target table fails" 1 : 'printf "\n[target.\x27cfg(unix)\x27.dependencies]\nlibc = \"0.2\"\n" >>Cargo.toml'
case_run "new-dep workspace table fails" 1 : 'printf "\n[workspace.dependencies]\nserde = \"1\"\n" >>Cargo.toml'
case_run "new-dep workspace=true of known dep passes" 0 \
    'printf "\n[workspace.dependencies]\nserde = \"1\"\n" >Cargo.toml' \
    'mkdir -p crates/x && printf "[package]\nname=\"x\"\n[dependencies]\nserde = { workspace = true }\n" >crates/x/Cargo.toml'
case_run "build.rs added fails" 1 : 'echo "fn main() {}" >build.rs'
case_run "build.rs on base passes" 0 'echo "fn main() {}" >build.rs' 'echo "// x" >>build.rs'
case_run "build key in Cargo.toml fails" 1 : 'sed -i.bak "s/^name = \"fx\"/name = \"fx\"\nbuild = \"b.rs\"/" Cargo.toml; rm Cargo.toml.bak'
case_run "proc-macro added fails" 1 : 'printf "\n[lib]\nproc-macro = true\n" >>Cargo.toml'
case_run "proc-macro on base passes" 0 'printf "\n[lib]\nproc-macro = true\n" >>Cargo.toml' 'echo hi >README.md'
case_run "net TcpStream added fails" 1 : 'echo "use std::net::TcpStream;" >>src/lib.rs'
case_run "net reqwest added fails" 1 : 'echo "fn g() { let _ = reqwest::Client::new(); }" >>src/lib.rs'
case_run "net address types only passes" 0 : 'echo "use std::net::{IpAddr, SocketAddr};" >>src/lib.rs'
case_run "net on base passes" 0 'echo "use std::net::UdpSocket;" >>src/lib.rs' 'echo "fn z() {}" >>src/lib.rs'
case_run "process Command::new added fails" 1 : 'echo "fn g() { std::process::Command::new(\"ls\"); }" >>src/lib.rs'
case_run "process tokio added fails" 1 : 'echo "use tokio::process::Child;" >>src/lib.rs'
case_run "process on base passes" 0 'echo "fn g() { Command::new(\"ls\"); }" >>src/lib.rs' 'echo "fn z() {}" >>src/lib.rs'
case_run "ffi extern C added fails" 1 : 'echo "extern \"C\" { fn abs(x: i32) -> i32; }" >>src/lib.rs'
case_run "ffi no_mangle added fails" 1 : 'echo "#[no_mangle] pub fn q() {}" >>src/lib.rs'
case_run "ffi on base passes" 0 'echo "#[no_mangle] pub fn q() {}" >>src/lib.rs' 'echo "fn z() {}" >>src/lib.rs'
case_run "ci-workflow added fails" 1 : 'mkdir -p .github/workflows && echo "name: x" >.github/workflows/x.yml'
case_run "ci-workflow edited fails" 1 'mkdir -p .github/workflows && echo "name: x" >.github/workflows/x.yml' 'echo "# c" >>.github/workflows/x.yml'
case_run "gate script change fails" 1 'mkdir -p scripts && echo "#!/bin/sh" >scripts/diff-gate.sh' 'echo "# weaker" >>scripts/diff-gate.sh'

# approvals
case_run "approved entry passes" 0 "printf '%s\n' '$APPROVE_UNSAFE' >config/diff-gate-approvals.toml" 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
case_run "approval for other path does not pass" 1 \
    "printf '%s\n' '$APPROVE_UNSAFE' | sed 's#src/\*\*#other/**#' >config/diff-gate-approvals.toml" 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
export APPROVE_REGEX="[[approval]]
rule = \"unsafe\"
path = \"src/*.rs\"
pattern = 'unsafe \\{ g\\('
reason = \"r\"
approved_by = \"o\""
case_run "approval pattern is a regex on the line" 0 'printf "%s\n" "$APPROVE_REGEX" >config/diff-gate-approvals.toml' 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
case_run "approvals-change fails without owner flag" 1 : "printf '%s\n' '$APPROVE_UNSAFE' >config/diff-gate-approvals.toml"
case_run "approvals-change passes with owner flag" 0 : "printf '%s\n' '$APPROVE_UNSAFE' >config/diff-gate-approvals.toml" DIFF_GATE_OWNER_APPROVED=1
case_run "agent adds approval plus violation fails" 1 : \
    "printf '%s\n' '$APPROVE_UNSAFE' >config/diff-gate-approvals.toml; echo 'fn f() { unsafe { g() } }' >>src/lib.rs"
case_run "owner adds approval plus violation passes" 0 : \
    "printf '%s\n' '$APPROVE_UNSAFE' >config/diff-gate-approvals.toml; echo 'fn f() { unsafe { g() } }' >>src/lib.rs" DIFF_GATE_OWNER_APPROVED=1
case_run "malformed approval is a setup error" 2 \
    "printf '[[approval]]\nrule = \"unsafe\"\n' >config/diff-gate-approvals.toml" 'echo hi >README.md'

# owner ledger (config/diff-gate-owner-ledger.toml): rows the owner already approved
# Canonical row text and hash are pinned here independently of the gate.
row_sha() { # rule path pattern reason approved_by
    printf 'rule=%s\npath=%s\npattern=%s\nreason=%s\napproved_by=%s\n' "$@" |
        { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; } | cut -d' ' -f1
}
H_UNSAFE="$(row_sha unsafe 'src/**' '*' fixture owner)"
H_NARROW="$(row_sha unsafe 'src/lib.rs' '*' fixture owner)"
H_OTHER="$(row_sha net 'src/**' '*' fixture owner)"
export LEDGER_UNSAFE="[[entry]]
decision = \"owner-2026-11-14-test\"
date     = \"2026-11-14\"
sha256   = \"$H_UNSAFE\""
export LEDGER_NARROW="[[entry]]
decision = \"owner-2026-11-14-narrow\"
date     = \"2026-11-14\"
sha256   = \"$H_NARROW\""
export LEDGER_OTHER="[[entry]]
decision = \"owner-2026-11-14-other\"
date     = \"2026-11-14\"
sha256   = \"$H_OTHER\""
export APPROVE_UNSAFE
export APPROVE_NARROW="$(printf '%s\n' "$APPROVE_UNSAFE" | sed 's#src/\*\*#src/lib.rs#')"
L=config/diff-gate-owner-ledger.toml
A=config/diff-gate-approvals.toml
case_run "ledger: added row on base ledger passes" 0 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L \
    'printf "%s\n" "$APPROVE_UNSAFE" >'$A
case_run "ledger: added row on base ledger applies to the same diff" 0 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L \
    'printf "%s\n" "$APPROVE_UNSAFE" >'$A'; echo "fn f() { unsafe { g() } }" >>src/lib.rs'
case_run "ledger: added row plus comment lines passes" 0 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L \
    'printf "# owner 2026-11-14\n%s\n" "$APPROVE_UNSAFE" >'$A
case_run "ledger: comment-only approvals edit passes" 0 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; printf "%s\n" "$APPROVE_UNSAFE" >'$A \
    'printf "# note\n" >>'$A
case_run "ledger: reordered rows pass" 0 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; printf "%s\n\n%s\n" "$APPROVE_UNSAFE" "$APPROVE_NARROW" >'$A \
    'printf "%s\n\n%s\n" "$APPROVE_NARROW" "$APPROVE_UNSAFE" >'$A
case_run "ledger: added row with no entry fails" 1 \
    'printf "%s\n" "$LEDGER_OTHER" >'$L \
    'printf "%s\n" "$APPROVE_UNSAFE" >'$A
case_run "ledger: added row with no ledger file fails" 1 : \
    'printf "%s\n" "$APPROVE_UNSAFE" >'$A
case_run "ledger: one matching and one unmatched row fails" 1 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L \
    'printf "%s\n\n%s\n" "$APPROVE_UNSAFE" "$APPROVE_NARROW" >'$A
case_run "ledger: removed row fails" 1 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; printf "%s\n" "$APPROVE_UNSAFE" >'$A \
    'printf "# emptied\n" >'$A
case_run "ledger: deleted approvals file fails" 1 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; printf "%s\n" "$APPROVE_UNSAFE" >'$A \
    'rm '$A
case_run "ledger: loosened row fails even when the wider row is on the ledger" 1 \
    'printf "%s\n\n%s\n" "$LEDGER_UNSAFE" "$LEDGER_NARROW" >'$L'; printf "%s\n" "$APPROVE_NARROW" >'$A \
    'printf "%s\n" "$APPROVE_UNSAFE" >'$A
case_run "ledger: edited reason fails" 1 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; printf "%s\n" "$APPROVE_UNSAFE" >'$A \
    'sed -i.bak "s/fixture/edited/" '$A'; rm '$A'.bak'
case_run "ledger: entry and row added in the same diff fails" 1 : \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; printf "%s\n" "$APPROVE_UNSAFE" >'$A
case_run "ledger: ledger-only edit fails" 1 \
    'printf "%s\n" "$LEDGER_OTHER" >'$L \
    'printf "\n%s\n" "$LEDGER_UNSAFE" >>'$L
case_run "ledger: ledger deleted fails" 1 \
    'printf "%s\n" "$LEDGER_OTHER" >'$L \
    'rm '$L
case_run "ledger: gate script edit fails with ledger present" 1 \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; mkdir -p scripts && echo "#!/bin/sh" >scripts/diff-gate.sh' \
    'echo "# weaker" >>scripts/diff-gate.sh'
case_run "ledger: gate script renamed away fails" 1 \
    'mkdir -p scripts && echo "#!/bin/sh" >scripts/diff-gate.sh' \
    'git mv scripts/diff-gate.sh scripts/other.sh'
case_run "ledger: malformed base ledger is a setup error" 2 \
    'printf "[[entry]]\ndecision = \"x\"\nsha256 = \"abc\"\n" >'$L 'echo hi >README.md'
case_run "override: added row with no entry passes" 0 : \
    'printf "%s\n" "$APPROVE_UNSAFE" >'$A DIFF_GATE_OWNER_APPROVED=1
case_run "override: removed row passes" 0 \
    'printf "%s\n" "$APPROVE_UNSAFE" >'$A 'printf "# emptied\n" >'$A DIFF_GATE_OWNER_APPROVED=1
case_run "override: loosened row passes" 0 \
    'printf "%s\n" "$APPROVE_NARROW" >'$A 'printf "%s\n" "$APPROVE_UNSAFE" >'$A DIFF_GATE_OWNER_APPROVED=1
case_run "override: ledger edit passes" 0 : \
    'printf "%s\n" "$LEDGER_UNSAFE" >'$L'; printf "%s\n" "$APPROVE_UNSAFE" >'$A DIFF_GATE_OWNER_APPROVED=1
case_run "override: gate script edit still fails" 1 \
    'mkdir -p scripts && echo "#!/bin/sh" >scripts/diff-gate.sh' 'echo "# weaker" >>scripts/diff-gate.sh' DIFF_GATE_OWNER_APPROVED=1

# --ledger-hash prints the canonical hash the ledger expects
d="$WORK/ledger_hash" && mkdir -p "$d/config" && cd "$d" && git init -q . && git config user.email t@t && git config user.name t
printf '%s\n' "$APPROVE_UNSAFE" >config/diff-gate-approvals.toml
out="$("$GATE" --ledger-hash 2>&1)"; rc=$?
if [ "$rc" = 0 ] && echo "$out" | grep -q "^$H_UNSAFE"; then pass=$((pass + 1)); echo "  ok    --ledger-hash prints the canonical row hash"; else fail=$((fail + 1)); echo "  FAIL  --ledger-hash: rc=$rc $out"; fi
cd "$ROOT" || exit 2

# json output and worktree mode
d="$WORK/json" && mkdir -p "$d" && cd "$d" && git init -q . && git config user.email t@t && git config user.name t
mkdir src && echo "pub fn a() {}" >src/lib.rs && git add -A && git commit -qm base
base="$(git rev-parse HEAD)"
echo "fn f() { unsafe { g() } }" >>src/lib.rs
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree --json 2>&1)"; rc=$?
if [ "$rc" = 1 ] && echo "$out" | grep -q '"failed":1' && echo "$out" | grep -q '"rule":"unsafe"'; then pass=$((pass + 1)); echo "  ok    worktree + json reports tracked change"; else fail=$((fail + 1)); echo "  FAIL  worktree + json: rc=$rc $out"; fi
git checkout -q src/lib.rs; echo "fn f() { unsafe { g() } }" >src/new.rs
DIFF_GATE_BASE="$base" "$GATE" --worktree >/dev/null 2>&1; rc=$?
DIFF_GATE_BASE="$base" "$GATE" >/dev/null 2>&1; rc2=$?
if [ "$rc" = 1 ] && [ "$rc2" = 0 ]; then pass=$((pass + 1)); echo "  ok    worktree sees untracked files, default mode does not"; else fail=$((fail + 1)); echo "  FAIL  untracked: worktree rc=$rc default rc=$rc2"; fi
cd "$ROOT" || exit 2

# review hardening (2026-11-15)
# 1. .gitattributes must not hide hunks: "*.rs -diff" makes plain git diff print "Binary files differ".
VIOL='echo "fn f() { unsafe { g() } }" >>src/lib.rs; echo "fn h() { std::process::Command::new(\"sh\"); }" >>src/lib.rs'
EXPECT="unsafe process" case_run "gitattributes -diff on base still shows unsafe and process" 1 \
    'echo "*.rs -diff" >.gitattributes' "$VIOL"
EXPECT="unsafe process gitattributes-change" case_run "gitattributes -diff added with violations fails on all three" 1 : \
    'echo "*.rs -diff" >.gitattributes; '"$VIOL"
EXPECT="unsafe" case_run "gitattributes binary macro on base still shows unsafe" 1 \
    'echo "*.rs binary" >.gitattributes' 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
EXPECT="unsafe" case_run "gitattributes textconv driver on base still shows unsafe" 1 \
    'echo "*.rs diff=hide" >.gitattributes; git config diff.hide.textconv "sed s/unsafe/safe/"' \
    'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
EXPECT="gitattributes-change" case_run "gitattributes added alone fails" 1 : 'echo "*.md text" >.gitattributes'
EXPECT="gitattributes-change" case_run "nested gitattributes edited fails" 1 \
    'mkdir -p crates/x && echo "*.md text" >crates/x/.gitattributes' 'echo "*.rs -diff" >>crates/x/.gitattributes'
EXPECT="gitattributes-change" case_run "gitattributes deleted fails" 1 'echo "*.md text" >.gitattributes' 'git rm -q .gitattributes'
export APPROVE_GA='[[approval]]
rule = "gitattributes-change"
path = ".gitattributes"
pattern = "*"
reason = "fixture"
approved_by = "owner"'
case_run "gitattributes change approved by a row passes" 0 'printf "%s\n" "$APPROVE_GA" >config/diff-gate-approvals.toml' \
    'echo "*.md text" >.gitattributes'
EXPECT="gitattributes-change" case_run "override does not approve gitattributes-change" 1 : \
    'echo "*.md text" >.gitattributes' DIFF_GATE_OWNER_APPROVED=1

# worktree mode: tracked and untracked (--no-index) files under a -diff attribute
d="$WORK/ga_worktree" && fixture_repo "$d" && echo "*.rs -diff" >.gitattributes && git add -A && git commit -qm base
base="$(git rev-parse HEAD)"
echo "fn f() { unsafe { g() } }" >>src/lib.rs
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
EXPECT="unsafe" check_result "worktree: gitattributes -diff tracked change still shows unsafe" 1 "$rc" "$out"
git checkout -q src/lib.rs; echo "fn h() { std::process::Command::new(\"sh\"); }" >src/new.rs
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
EXPECT="process" check_result "worktree: gitattributes -diff untracked file still shows process" 1 "$rc" "$out"
cd "$ROOT" || exit 2

# 2. The ledger is read at the target tip, so revoking an entry there takes effect for
#    branches forked before the revoke.
d="$WORK/ledger_revoke" && fixture_repo "$d"
git add -A && git commit -qm root && git branch -q -M main
printf '%s\n' "$LEDGER_UNSAFE" >$L && git add -A && git commit -qm "L: add entry E"
git checkout -q -b X && printf '%s\n' "$APPROVE_UNSAFE" >$A && git add -A && git commit -qm "X: add row R"
git checkout -q main && printf '# revoked\n' >$L && git add -A && git commit -qm "T: revoke E"
git checkout -q X
out="$("$GATE" --base main --head X 2>&1)"; rc=$?
EXPECT="approvals-change" check_result "ledger: entry revoked on the target tip no longer approves" 1 "$rc" "$out"
out="$("$GATE" --base main~1 --head X 2>&1)"; rc=$?
check_result "ledger: control, same branch against the pre-revoke tip passes" 0 "$rc" "$out"
cd "$ROOT" || exit 2

d="$WORK/ledger_tip_add" && fixture_repo "$d"
git add -A && git commit -qm root && git branch -q -M main
git checkout -q -b X && printf '%s\n' "$APPROVE_UNSAFE" >$A && git add -A && git commit -qm "X: add row R"
git checkout -q main && mkdir -p config && printf '%s\n' "$LEDGER_UNSAFE" >$L && git add -A && git commit -qm "T: add entry E"
git checkout -q X
out="$("$GATE" --base main --head X 2>&1)"; rc=$?
check_result "ledger: entry landed on the tip after the fork approves" 0 "$rc" "$out"
cd "$ROOT" || exit 2

# 3. Classification reads approvals from the head commit, not the checkout.
d="$WORK/head_approvals" && fixture_repo "$d"
git add -A && git commit -qm base && git branch -q -M main && base="$(git rev-parse HEAD)"
git checkout -q -b C && echo "fn f() { unsafe { g() } }" >>src/lib.rs && git add -A && git commit -qm "C: unsafe"
git checkout -q -b wide main && printf '%s\n' "$APPROVE_UNSAFE" >$A && git add -A && git commit -qm "wide approvals"
out="$("$GATE" --base "$base" --head C 2>&1)"; rc=$?
EXPECT="unsafe" check_result "head: approvals in the checkout do not approve a different --head" 1 "$rc" "$out"
git checkout -q C && mkdir -p config && printf '%s\n' "$APPROVE_UNSAFE" >$A
out="$("$GATE" --base "$base" 2>&1)"; rc=$?
EXPECT="unsafe" check_result "head: uncommitted approvals do not approve HEAD in commit mode" 1 "$rc" "$out"
out="$("$GATE" --base "$base" --worktree 2>&1)"; rc=$?
EXPECT="approvals-change" check_result "head: --worktree reads the working-tree approvals and flags the change" 1 "$rc" "$out"
cd "$ROOT" || exit 2

echo "diff-gate fixtures: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
