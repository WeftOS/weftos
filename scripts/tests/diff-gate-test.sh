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
    if [ "$rc" = "$want" ]; then pass=$((pass + 1)); printf '  ok    %s\n' "$name"
    else fail=$((fail + 1)); printf '  FAIL  %s (exit %s, wanted %s)\n%s\n' "$name" "$rc" "$want" "$out" | sed 's/^/        /'; fi
    cd "$ROOT" || exit 2
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

echo "diff-gate fixtures: $pass passed, $fail failed"
[ "$fail" -eq 0 ]
