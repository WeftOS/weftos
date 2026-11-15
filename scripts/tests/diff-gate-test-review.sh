# shellcheck shell=bash
# diff-gate-test-review.sh: review-round fixtures, sourced by diff-gate-test.sh (section 12).
# Uses that file's case_run / check_result / fixture_repo / appr / hobj helpers and counters.
# KNOWN_GAP=<id> marks a case the gate misclassifies today (expected-failure, reported).
# shellcheck disable=SC2016  # snippets are eval'd inside the fixture repo on purpose

echo "diff-gate review fixtures (2026-11-15)"
CR=$'\r'
export CR
A=config/diff-gate-approvals.toml

# ── (a) Rust spellings, line lexer (stub helper) ─────────────────────
EXPECT="unsafe" case_run "rv-a: asm! inside unsafe" 1 : 'echo "fn f() { unsafe { core::arch::asm!(\"nop\") } }" >>src/lib.rs'
EXPECT="unsafe ffi" case_run "rv-a: 2024 #[unsafe(no_mangle)]" 1 : 'echo "#[unsafe(no_mangle)] pub extern \"C\" fn q() {}" >>src/lib.rs'
EXPECT="ffi" case_run "rv-a: export_name attribute" 1 : 'echo "#[export_name = \"q\"] pub fn q() {}" >>src/lib.rs'
EXPECT="ffi" case_run "rv-a: link_section attribute" 1 : 'echo "#[link_section = \".init_array\"] static S: u8 = 0;" >>src/lib.rs'
EXPECT="ffi" case_run "rv-a: extern block inside macro_rules body" 1 : 'echo "macro_rules! m { () => { extern \"C\" { fn abs(x: i32) -> i32; } } }" >>src/lib.rs'
EXPECT="process" case_run "rv-a: extern crate std alias reaching process::Command" 1 : 'echo "extern crate std as s; fn f() { s::process::Command::new(\"sh\"); }" >>src/lib.rs'
EXPECT="process" case_run "rv-a: use std::{process} brace import" 1 : 'echo "use std::{io, process}; fn f() { let _ = io::stdout(); }" >>src/lib.rs'
EXPECT="net" case_run "rv-a: use std::{io, net} brace import" 1 : 'echo "use std::{io, net}; fn f() {}" >>src/lib.rs'
EXPECT="net" case_run "rv-a: extern crate reqwest as r" 1 : 'echo "extern crate reqwest as r;" >>src/lib.rs'
EXPECT="net" case_run "rv-a: tokio_tungstenite path" 1 : 'echo "fn f() { let _ = tokio_tungstenite::connect_async(\"ws://x\"); }" >>src/lib.rs'
EXPECT="include" case_run "rv-a: include ! with spaces of a non-.rs file" 1 : 'echo "include ! (\"x.txt\");" >>src/lib.rs'
EXPECT="include" case_run "rv-a: include! over two lines" 1 : 'printf "include!(\n    \"x.txt\"\n);\n" >>src/lib.rs'
EXPECT="include" case_run "rv-a: single-line cfg_attr path to a non-.rs file" 1 : 'echo "#[cfg_attr(unix, path = \"x.txt\")] mod m;" >>src/lib.rs'
case_run "rv-a: include_str! of a .rs file in code is data, not code" 0 : 'echo "pub fn g() {}" >src/gen.rs; echo "const S: &str = include_str!(\"gen.rs\");" >>src/lib.rs'
EXPECT="unsafe" case_run "rv-a: CRLF .rs file" 1 : 'printf "fn f() {%s\n    unsafe { g() }%s\n}%s\n" "$CR" "$CR" "$CR" >src/crlf.rs'

# ── (b) Cargo manifest forms ─────────────────────────────────────────
EXPECT="new-dep" case_run "rv-b: renamed package under a new key" 1 : 'printf "foo = { package = \"existing\", version = \"1\" }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "rv-b: path source on an existing dep" 1 : 'sed -i.bak "s|^existing = \"1\"|existing = { path = \"../evil\" }|" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "rv-b: target triple dependencies table" 1 : 'printf "\n[target.x86_64-unknown-linux-gnu.dependencies]\nevil = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "rv-b: build-dependencies entry" 1 : 'printf "\n[build-dependencies]\ncc = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "rv-b: header with inner spaces and a comment" 1 : 'printf "\n[ dev-dependencies ] # tests\nevil = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "rv-b: dotted git key on an existing dep" 1 : 'sed -i.bak "s|^existing = \"1\"|existing.version = \"1\"\nexisting.git = \"https://evil\"|" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "rv-b: multi-line string git source in a dep table" 1 : 'printf "\n[dependencies.evil2]\nversion = \"1\"\n" >>Cargo.toml' \
    'printf "git = \"\"\"\nhttps://evil\"\"\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "rv-b: workspace = true for a dep the base workspace lacks" 1 'printf "\n[workspace.dependencies]\nserde = \"1\"\n" >Cargo.toml' \
    'mkdir -p crates/x && printf "[package]\nname=\"x\"\n[dependencies]\nevil = { workspace = true }\n" >crates/x/Cargo.toml'
EXPECT="new-dep" case_run "rv-b: [patch.\"https://...\"] git URL key" 1 : 'printf "\n[patch.\"https://github.com/x/y\"]\nexisting = { path = \"../evil\" }\n" >>Cargo.toml'
EXPECT="build-script" case_run "rv-b: CRLF build key" 1 : 'printf "[package]%s\nname = \"fx\"%s\nbuild = \"b.rs\"%s\n" "$CR" "$CR" "$CR" >Cargo.toml'
# Gaps: a CRLF manifest hides its tables; deps declared through dotted keys of a parent table.
KNOWN_GAP=RV-B1 EXPECT="new-dep" case_run "rv-b: CRLF Cargo.toml new dependency" 1 \
    'printf "[package]%s\nname = \"fx\"%s\n%s\n[dependencies]%s\nexisting = \"1\"%s\n" "$CR" "$CR" "$CR" "$CR" "$CR" >Cargo.toml' \
    'printf "evil = { git = \"https://evil\" }%s\n" "$CR" >>Cargo.toml'
KNOWN_GAP=RV-B1 EXPECT="new-dep" case_run "rv-b: LF to CRLF conversion plus a new dependency" 1 : \
    'sed -i.bak "s/\$/$CR/" Cargo.toml; rm Cargo.toml.bak; printf "evil = { git = \"https://evil\" }%s\n" "$CR" >>Cargo.toml'
# [patch.X] still matches with a trailing CR (the table regex is loose), [replace] does not.
EXPECT="new-dep" case_run "rv-b: CRLF [patch.crates-io] git redirect" 1 \
    'printf "[package]%s\nname = \"fx\"%s\n" "$CR" "$CR" >Cargo.toml' \
    'printf "%s\n[patch.crates-io]%s\nexisting = { git = \"https://evil\" }%s\n" "$CR" "$CR" "$CR" >>Cargo.toml'
KNOWN_GAP=RV-B1 EXPECT="new-dep" case_run "rv-b: CRLF [replace] entry" 1 : \
    'printf "%s\n[replace]%s\n\"existing:1.0.0\" = { git = \"https://evil\" }%s\n" "$CR" "$CR" "$CR" >>Cargo.toml'
KNOWN_GAP=RV-B1 EXPECT="include" case_run "rv-b: CRLF [lib] path to a non-.rs file" 1 : \
    'printf "%s\n[lib]%s\npath = \"src/lib.txt\"%s\n" "$CR" "$CR" "$CR" >>Cargo.toml'
KNOWN_GAP=RV-B2 EXPECT="new-dep" case_run "rv-b: top-level dotted dependencies key" 1 : \
    '{ printf "dependencies.evil = { git = \"https://evil\" }\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
KNOWN_GAP=RV-B2 EXPECT="new-dep" case_run "rv-b: top-level inline dependencies table" 1 \
    'printf "[package]\nname = \"fx\"\n" >Cargo.toml' \
    '{ printf "dependencies = { evil = { git = \"https://evil\" } }\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
KNOWN_GAP=RV-B2 EXPECT="new-dep" case_run "rv-b: [target.cfg] table with a dotted dependencies key" 1 : \
    'printf "\n[target.\x27cfg(unix)\x27]\ndependencies.evil = { git = \"https://evil\" }\n" >>Cargo.toml'
KNOWN_GAP=RV-B2 EXPECT="new-dep" case_run "rv-b: [patch] table with a dotted registry key" 1 : \
    'printf "\n[patch]\ncrates-io.existing = { git = \"https://evil\" }\n" >>Cargo.toml'
KNOWN_GAP=RV-B2 EXPECT="new-dep" case_run "rv-b: [workspace] table with a dotted dependencies key" 1 : \
    'printf "\n[workspace]\ndependencies.evil = { git = \"https://evil\" }\n" >>Cargo.toml'
EXPECT="include" case_run "rv-b: top-level dotted lib.path to a non-.rs file" 1 : \
    '{ printf "lib.path = \"src/lib.txt\"\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
case_run "rv-b: control, top-level lib.path to a .rs file passes" 0 : \
    '{ printf "lib.path = \"src/lib.rs\"\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'

# ── (c) Trust inputs ─────────────────────────────────────────────────
case_run "rv-c: gate script mode change only is ci-workflow" 1 'mkdir -p scripts && echo "#!/bin/sh" >scripts/diff-gate.sh' 'chmod +x scripts/diff-gate.sh'
EXPECT="ci-workflow" case_run "rv-c: workflow replaced by a symlink" 1 'mkdir -p .github/workflows && echo "name: x" >.github/workflows/x.yml' \
    'rm .github/workflows/x.yml; echo "name: y" >y.yml; ln -s ../../y.yml .github/workflows/x.yml'
EXPECT="approvals-change" case_run "rv-c: approvals file moved away" 1 "printf '%s\n' \"\$APPROVE_UNSAFE\" >$A" "git mv $A config/old-approvals.toml"
case_run "rv-c: approvals file turned into a symlink fails closed" 2 "printf '%s\n' \"\$APPROVE_UNSAFE\" >$A" \
    "cp $A config/x.toml; rm $A; ln -s x.toml $A"
EXPECT="sensitive-crate approvals-change" case_run "rv-c: an exempt glob added in the diff does not exempt the same diff" 1 "$SBASE" \
    'printf "\n[[exempt]]\npath = \"crates/k/**\"\nreason = \"x\"\n" >>'$S'; printf "%s\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="approvals-change" case_run "rv-c: ledger moved away" 1 'printf "%s\n" "$LEDGER_UNSAFE" >'$L "git mv $L config/old-ledger.toml"
EXPECT="rust-tokens" GREP="inside the checkout" case_run "rv-c: relative cache path that climbs out of the checkout root is still refused" 1 \
    'mkdir -p tools/diff-gate-tokens && echo x >tools/diff-gate-tokens/README.md' "$ADD_RS" DIFF_GATE_TOKENS_BIN= DIFF_GATE_CACHE=target/../target/c
d="$WORK/rv_cache_link" && fixture_repo "$d" && mkdir -p tools/diff-gate-tokens && echo x >tools/diff-gate-tokens/README.md
git add -A && git commit -qm base && base="$(git rev-parse HEAD)" && echo "fn z() {}" >>src/lib.rs && git add -A && git commit -qm change
mkdir -p "$d/target/c" && ln -s "$d/target/c" "$WORK/rv-cache-link"
out="$(DIFF_GATE_BASE="$base" DIFF_GATE_TOKENS_BIN="" DIFF_GATE_CACHE="$WORK/rv-cache-link" PATH="/usr/bin:/bin" "$GATE" 2>&1)"; rc=$?
KNOWN_GAP=RV-C1 EXPECT="rust-tokens" GREP="inside the checkout" check_result "rv-c: a cache path that is a symlink into the checkout is refused" 1 "$rc" "$out"
cd "$ROOT" || exit 2
# The helper is built from the base tree, never from the change: a base helper that reports a
# finding still reports it when the change rewrites the helper to report nothing. Needs cargo;
# opt in with DIFF_GATE_TEST_BUILD_HELPER=1 (the owner keeps cargo builds off the Mac).
if [ "${DIFF_GATE_TEST_BUILD_HELPER:-0}" = 1 ] && command -v cargo >/dev/null 2>&1; then
    export TINY_TOML='[package]
name = "diff-gate-tokens"
version = "0.1.0"
edition = "2021"

[workspace]
'
    export TINY_LOCK='version = 4

[[package]]
name = "diff-gate-tokens"
version = "0.1.0"
'
    export TINY_FLAGS='use std::io::BufRead;
fn main() {
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(String::as_str) == Some("--protocol") { println!("diff-gate-tokens protocol 1"); return; }
    let f = std::fs::File::open(&a[2]).unwrap();
    for l in std::io::BufReader::new(f).lines() {
        let l = l.unwrap();
        let v: Vec<&str> = l.split(char::from(9)).collect();
        if let Some(n) = v.get(3).and_then(|s| s.split(char::from(44)).next()).filter(|s| !s.is_empty()) {
            println!("process{}{}{}{}{}base-helper", char::from(9), v[0], char::from(9), n, char::from(9));
        }
    }
}
'
    export TINY_QUIET='fn main() {
    if std::env::args().nth(1).as_deref() == Some("--protocol") { println!("diff-gate-tokens protocol 1"); }
}
'
    TINY_BASE='mkdir -p tools/diff-gate-tokens/src && printf %s "$TINY_TOML" >tools/diff-gate-tokens/Cargo.toml && printf %s "$TINY_LOCK" >tools/diff-gate-tokens/Cargo.lock && printf %s "$TINY_FLAGS" >tools/diff-gate-tokens/src/main.rs'
    EXPECT="process ci-workflow" case_run "rv-c: a change that silences the helper is still judged by the base helper" 1 "$TINY_BASE" \
        'printf %s "$TINY_QUIET" >tools/diff-gate-tokens/src/main.rs; echo "fn z() {}" >>src/lib.rs' DIFF_GATE_TOKENS_BIN= DIFF_GATE_CACHE="$WORK/rv-tiny-cache"
fi

# ── (d) Approval rows ────────────────────────────────────────────────
export APPROVE_DOT='[[approval]]
rule = "unsafe"
path = "src/**"
pattern = ".*"
reason = "fixture"
approved_by = "owner"'
KNOWN_GAP=RV-D1 GREP="unscoped approval row" case_run "rv-d: a match-anything regex row without blob is named as unscoped" 0 \
    'printf "%s\n" "$APPROVE_DOT" >'$A 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
case_run "rv-d: content row with blob approves that file version" 0 \
    'appr unsafe src/lib.rs "$(hobj "pub fn a() {}
fn f() { unsafe { g() } }
")" >'$A 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
EXPECT="unsafe" case_run "rv-d: content row with blob does not approve another version" 1 \
    'appr unsafe src/lib.rs "$(hobj "pub fn a() {}
fn f() { unsafe { g() } }
")" >'$A 'printf "fn f() { unsafe { g() } }\nfn h() { unsafe { k() } }\n" >>src/lib.rs'
EXPECT="unsafe" case_run "rv-d: blob row for one path does not approve the same content elsewhere" 1 \
    'appr unsafe src/lib.rs "$(hobj "fn f() { unsafe { g() } }
")" >'$A 'echo "fn f() { unsafe { g() } }" >src/other.rs'
case_run "rv-d: upper-case blob is the same row (lower-cased, same ledger hash)" 0 \
    'B="$(hobj "x")"; printf "[[entry]]\ndecision = \"d\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n" "$(row_sha6 ci-workflow .github/workflows/x.yml "*" "$B" fixture owner)" >'$L \
    'appr ci-workflow .github/workflows/x.yml "$(hobj "x" | tr a-f A-F)" >'$A
EXPECT="approvals-change" case_run "rv-d: a duplicate pattern key is hashed as parsed (last wins), not as the narrow row" 1 \
    'printf "[[entry]]\ndecision = \"d\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n" "$(row_sha unsafe src/lib.rs x fixture owner)" >'$L \
    'printf "[[approval]]\nrule = \"unsafe\"\npath = \"src/lib.rs\"\npattern = \"x\"\npattern = \"*\"\nreason = \"fixture\"\napproved_by = \"owner\"\n" >'$A
EXPECT="rust-parse" case_run "rv-d: helper rust-parse finding is reported (stub)" 1 : "$ADD_RS" DIFF_GATE_TOKENS_BIN="$STUB" DIFF_GATE_STUB_OUT="$(printf 'rust-parse\tsrc/lib.rs\t2\tcannot parse')"
case_run "rv-d: a rust-parse row clears it and is named as unscoped" 0 \
    'appr rust-parse src/lib.rs "" >'$A "$ADD_RS" DIFF_GATE_TOKENS_BIN="$STUB" DIFF_GATE_STUB_OUT="$(printf 'rust-parse\tsrc/lib.rs\t2\tcannot parse')" GREP="unscoped approval row"
EXPECT="approvals-change" case_run "rv-d: CRLF approvals row is not a silent pass" 1 : \
    'printf "%s\n" "$APPROVE_UNSAFE" | sed "s/\$/$CR/" >'$A'; echo "fn f() { unsafe { g() } }" >>src/lib.rs'

# ── (e) Input robustness ─────────────────────────────────────────────
EXPECT="unsafe process" case_run "rv-e: file name with a leading dash" 1 : 'mkdir -p src/-x && printf "%s\n" "$VIOL2" >src/-x/-rf.rs'
EXPECT="unsafe" case_run "rv-e: file name with spaces" 1 : 'printf "%s\n" "$VIOL2" >"src/a b c.rs"'
case_run "rv-e: newline in a path is a setup error" 2 : 'printf "fn f() {}\n" >"src/a
b.rs"'
EXPECT="unsafe" case_run "rv-e: copy of a file is scanned in full" 1 'echo "fn f() { unsafe { g() } }" >src/u.rs' 'cp src/u.rs src/u2.rs'
case_run "rv-e: mode change of a .rs file is clean" 0 : 'chmod +x src/lib.rs'
EXPECT="ci-workflow" case_run "rv-e: mode change of a workflow" 1 'mkdir -p .github/workflows && echo "name: x" >.github/workflows/x.yml' 'chmod +x .github/workflows/x.yml'
EXPECT="special-file" case_run "rv-e: type change, .rs file to symlink" 1 'echo "fn f() { unsafe { g() } }" >evil.txt' 'rm src/lib.rs; ln -s ../evil.txt src/lib.rs'
EXPECT="special-file" case_run "rv-e: submodule gitlink added" 1 : \
    'git config advice.addEmbeddedRepo false; git init -q vendor/sub && git -C vendor/sub -c user.email=t@t -c user.name=t commit -q --allow-empty -m x'
EXPECT="gitattributes-change unsafe" case_run "rv-e: nested .gitattributes -diff does not hide code" 1 : \
    'echo "*.rs -diff" >src/.gitattributes; echo "fn f() { unsafe { g() } }" >>src/lib.rs'
EXPECT="unsafe" case_run "rv-e: .rs file with NUL bytes" 1 : 'printf "fn f() { unsafe { g() } }\n// \000\001\n" >src/bin.rs'
EXPECT="unsafe" case_run "rv-e: large diff, one unsafe at the end" 1 : \
    'awk "BEGIN { for (i = 0; i < 20000; i++) print \"pub fn f\" i \"() {}\" }" >src/big.rs; echo "fn z() { unsafe { g() } }" >>src/big.rs'
EXPECT="unsafe" case_run "rv-e: many files, one unsafe" 1 : \
    'mkdir -p src/m && for i in $(seq 1 300); do echo "pub fn f() {}" >src/m/f$i.rs; done; echo "fn z() { unsafe { g() } }" >>src/m/f300.rs'
d="$WORK/rv_missing_appr" && fixture_repo "$d" && printf '%s\n' "$APPROVE_UNSAFE" >$A && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
echo "fn z() {}" >>src/lib.rs && git add -A && git commit -qm change
blob="$(git rev-parse HEAD:$A)" && rm -f ".git/objects/${blob:0:2}/${blob:2}"
out="$(DIFF_GATE_BASE="$base" "$GATE" 2>&1)"; rc=$?
check_result "rv-e: unreadable unchanged approvals blob fails closed" 2 "$rc" "$out"
cd "$ROOT" || exit 2
d="$WORK/rv_missing_sens" && fixture_repo "$d" && printf '%s\n' "$SENS" >$S && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
echo "fn z() {}" >>src/lib.rs && git add -A && git commit -qm change
blob="$(git rev-parse HEAD:$S)" && rm -f ".git/objects/${blob:0:2}/${blob:2}"
out="$(DIFF_GATE_BASE="$base" "$GATE" 2>&1)"; rc=$?
check_result "rv-e: unreadable sensitive list fails closed" 2 "$rc" "$out"
cd "$ROOT" || exit 2
# --worktree: an untracked file hidden by a .gitignore edit is still code a local build reads.
d="$WORK/rv_wt_ignored" && fixture_repo "$d" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
echo "src/gen.rs" >.gitignore && echo "fn f() { unsafe { g() } }" >src/gen.rs && echo "mod gen;" >>src/lib.rs
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
KNOWN_GAP=RV-E1 EXPECT="unsafe" check_result "rv-e: worktree: git-ignored untracked .rs file is scanned" 1 "$rc" "$out"
cd "$ROOT" || exit 2

# ── (f) sensitive-crate rule ─────────────────────────────────────────
EXPECT="sensitive-crate" case_run "rv-f: re-export of moved code is flagged where it lands in the sensitive crate" 1 "$SBASE" \
    'printf "pub fn helper() {}\n" >>crates/other/src/supervise.rs; printf "pub use other::helper;\n" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "rv-f: #[path] declaration of an exempt-named file without cfg(test)" 1 "$SBASE" \
    'printf "%s\n" "$CALLER" >crates/k/src/real_tests.rs; printf "#[path = \"real_tests.rs\"]\nmod real;\n" >>crates/k/src/supervise.rs'
case_run "rv-f: cfg( test ) with spaces and a doc comment before mod is exempt" 0 "$SBASE" \
    'printf "#[cfg( test )]\n/// tests\nmod tests {\n    fn t() {}\n}\n" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "rv-f: cfg_attr(test, ...) is not test-only" 1 "$SBASE" \
    'printf "#[cfg_attr(test, allow(dead_code))]\nmod t {\n    fn a() {}\n}\n" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "rv-f: inner #![cfg(test)] is not exempt" 1 "$SBASE" \
    'printf "#![cfg(test)]\n%s\n" "$CALLER" >crates/k/src/inner.rs'
EXPECT="sensitive-crate" case_run "rv-f: deleting the cfg(test) line of an inline module exposes its new code" 1 \
    "$SBASE"'; printf "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n" >>crates/k/src/supervise.rs' \
    'sed -i.bak "/^#\[cfg(test)\]\$/d" crates/k/src/supervise.rs; rm crates/k/src/supervise.rs.bak; sed -i.bak "s/^    fn t() {}/    fn t() {}\n    pub fn evil() { super::signal_pgid(1, 9); }/" crates/k/src/supervise.rs; rm crates/k/src/supervise.rs.bak'
EXPECT="sensitive-crate" case_run "rv-f: Cargo [[bin]] path in a sensitive crate" 1 "$SBASE"'; printf "[package]\nname = \"k\"\n" >crates/k/Cargo.toml' \
    'printf "\n[[bin]]\nname = \"x\"\npath = \"src/x_tests.rs\"\n" >>crates/k/Cargo.toml'
EXPECT="sensitive-crate" case_run "rv-f: existing build.rs in a sensitive crate is not exempt" 1 "$SBASE"'; echo "fn main() {}" >crates/k/build.rs' \
    'echo "fn x() { std::fs::write(\"../other/src/lib.rs\", \"\").ok(); }" >>crates/k/build.rs'
# Gap: name-based exemption trusts the module declaration's cfg(test). Deleting that cfg line
# adds no line, so new code in the exempt file becomes non-test code with no finding.
export TDECL='#[cfg(test)]
#[path = "supervise_tests.rs"]
pub(crate) mod helpers;'
KNOWN_GAP=RV-F1 EXPECT="sensitive-crate" case_run "rv-f: deleting #[cfg(test)] from a *_tests.rs declaration exposes new code" 1 \
    "$SBASE"'; printf "%s\n" "$TDECL" >>crates/k/src/supervise.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' \
    'grep -v "^#\[cfg(test)\]\$" crates/k/src/supervise.rs >s.tmp && mv s.tmp crates/k/src/supervise.rs; printf "pub(crate) fn stop_all() { super::signal_pgid(-1, 9); }\n" >>crates/k/src/supervise_tests.rs'
KNOWN_GAP=RV-F1 EXPECT="sensitive-crate" case_run "rv-f: deleting #[cfg(test)] from a mod tests; (tests/ dir) declaration exposes new code" 1 \
    "$SBASE"'; printf "#[cfg(test)]\n#[path = \"tests/mod.rs\"]\nmod tests;\n" >>crates/k/src/supervise.rs; mkdir -p crates/k/src/tests; echo "fn t() {}" >crates/k/src/tests/mod.rs' \
    'grep -v "^#\[cfg(test)\]\$" crates/k/src/supervise.rs >s.tmp && mv s.tmp crates/k/src/supervise.rs; printf "pub fn stop_all() { super::super::signal_pgid(-1, 9); }\n" >>crates/k/src/tests/mod.rs'
case_run "rv-f: control, the same edit with cfg(test) kept is exempt" 0 \
    "$SBASE"'; printf "%s\n" "$TDECL" >>crates/k/src/supervise.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' \
    'printf "pub(crate) fn stop_all() { super::signal_pgid(-1, 9); }\n" >>crates/k/src/supervise_tests.rs'

# ── Real helper only ─────────────────────────────────────────────────
if [ "$REAL_TOKENS" = 1 ]; then
    EXPECT="unsafe" case_run "rv-real: doctest in a #[doc = \"...\"] string attribute" 1 : \
        'printf "#[doc = \"\`\`\`\nunsafe { core::hint::unreachable_unchecked() }\n\`\`\`\"]\npub fn d() {}\n" >>src/lib.rs'
    EXPECT="process" case_run "rv-real: doctest in an inner //! doc" 1 : \
        '{ printf "//! \`\`\`\n//! std::process::Command::new(\"sh\");\n//! \`\`\`\n"; cat src/lib.rs; } >s.tmp && mv s.tmp src/lib.rs'
    EXPECT="process" case_run "rv-real: doctest in a /** */ block doc" 1 : \
        'printf "/**\n\`\`\`\nstd::process::Command::new(\"sh\");\n\`\`\`\n*/\npub fn d() {}\n" >>src/lib.rs'
    EXPECT="process" case_run "rv-real: doctest fence with tildes" 1 : \
        'printf "/// ~~~\n/// std::process::Command::new(\"sh\");\n/// ~~~\npub fn d() {}\n" >>src/lib.rs'
    EXPECT="process" case_run "rv-real: doctest fence with rust,no_run info" 1 : \
        'printf "/// \`\`\`rust,no_run\n/// std::process::Command::new(\"sh\");\n/// \`\`\`\npub fn d() {}\n" >>src/lib.rs'
    EXPECT="process" case_run "rv-real: aliased Command used as a type only" 1 \
        'printf "use std::process as p;\nfn g() {}\n" >src/t.rs' 'printf "use std::process as p;\nfn g() {}\nfn f() -> p::Command { todo!() }\n" >src/t.rs'
    EXPECT="process" case_run "rv-real: qualified path <p::Command>::new" 1 \
        'printf "use std::process as p;\n" >src/t.rs' 'printf "use std::process as p;\nfn f() { let _ = <p::Command>::new(\"sh\"); }\n" >src/t.rs'
    EXPECT="net" case_run "rv-real: use ::{reqwest} brace import from the root" 1 : 'printf "use ::{reqwest};\nfn f() { let _ = reqwest::get(1); }\n" >src/r.rs'
    EXPECT="net" case_run "rv-real: use self::... of an aliased std" 1 : 'printf "use std as s;\nuse s::net as n;\nfn f() { let _ = n::TcpStream::connect(1); }\n" >src/n.rs'
    EXPECT="include" case_run "rv-real: paste! builds identifiers" 1 : 'echo "paste::paste! { fn [<a b>]() {} }" >>src/lib.rs'
    EXPECT="include" case_run "rv-real: concat_idents!" 1 : 'echo "fn f() { let _ = concat_idents!(a, b); }" >>src/lib.rs'
    EXPECT="ffi" case_run "rv-real: naked_asm" 1 : 'echo "fn f() { core::arch::naked_asm!(\"ret\"); }" >>src/lib.rs'
    EXPECT="ffi" case_run "rv-real: global_asm via use alias" 1 : 'printf "use core::arch::global_asm as ga;\nga!(\"nop\");\n" >src/a.rs'
    EXPECT="include" case_run "rv-real: include via a use alias in a block" 1 : 'printf "fn f() { use core::include as i; i!(\"x.txt\"); }\n" >src/i.rs'
    EXPECT="include" case_run "rv-real: #![doc = include_str!] in an existing file" 1 : '{ printf "#![doc = include_str!(\"../README.md\")]\n"; cat src/lib.rs; } >s.tmp && mv s.tmp src/lib.rs'
    EXPECT="unsafe" case_run "rv-real: method added inside an existing unsafe impl" 1 \
        'printf "pub unsafe trait T { fn m(&self) {} }\npub struct S;\nunsafe impl T for S {\n}\n" >src/s.rs' \
        'printf "pub unsafe trait T { fn m(&self) {} }\npub struct S;\nunsafe impl T for S {\n    fn m(&self) { g(); }\n}\nfn g() {}\n" >src/s.rs'
    EXPECT="unsafe" case_run "rv-real: line added inside an existing unsafe fn body" 1 \
        'printf "unsafe fn u() {\n    a();\n}\nfn a() {}\n" >src/s.rs' 'printf "unsafe fn u() {\n    a();\n    b();\n}\nfn a() {}\nfn b() {}\n" >src/s.rs'
    EXPECT="process" case_run "rv-real: std::r#process raw identifier" 1 : 'printf "use std::r#process::Command as K;\nfn f() { K::new(\"sh\"); }\n" >src/v2.rs'
    EXPECT="process" case_run "rv-real: use std::*; then use process as p" 1 : \
        'printf "use std::*;\nuse process as p;\nuse p::Command as K;\nfn f() { K::new(\"sh\"); }\n" >src/v1.rs'
    # Documented limits (spec "Known limits"): a new caller of a construct that is already on
    # the base through a type alias or a macro is not a content finding. Pinned at exit 0 so a
    # change in either direction is noticed; outside sensitive crates nothing else catches it.
    case_run "rv-real: limit, new caller through an existing type alias" 0 \
        'printf "type K = std::process::Command;\n" >src/v6.rs' 'printf "type K = std::process::Command;\nfn f() { K::new(\"sh\"); }\n" >src/v6.rs'
    case_run "rv-real: limit, new caller of an existing unsafe-wrapping macro" 0 \
        'printf "macro_rules! u { (\$e:expr) => { unsafe { \$e } } }\nfn k() {}\n" >src/m.rs' \
        'printf "macro_rules! u { (\$e:expr) => { unsafe { \$e } } }\nfn k() {}\nfn f() { u!(k()); }\n" >src/m.rs'
    EXPECT="ffi" case_run "rv-real: declaration added inside an existing extern block" 1 \
        'printf "extern \"C\" {\n    fn abs(x: i32) -> i32;\n}\n" >src/e.rs' 'printf "extern \"C\" {\n    fn abs(x: i32) -> i32;\n    fn system(c: *const u8) -> i32;\n}\n" >src/e.rs'
    EXPECT="process" case_run "rv-real: glob import line edited, unchanged use lines change meaning" 1 \
        'printf "use mock::*;\nfn f() { Command::args; }\n" >src/c.rs' 'printf "use std::process::*;\nfn f() { Command::args; }\n" >src/c.rs'
    EXPECT="unsafe" case_run "rv-real: CRLF .rs file" 1 : 'printf "fn f() {%s\n    unsafe { g() }%s\n}%s\n" "$CR" "$CR" "$CR" >src/crlf.rs'
    EXPECT="unsafe" case_run "rv-real: .rs file with NUL bytes still read" 1 : 'printf "fn f() { unsafe { g() } }\n// \000\001\n" >src/bin.rs'
fi
