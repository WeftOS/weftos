# shellcheck shell=bash
# diff-gate-test-rv-fixes.sh: fixtures for the fixes to the 2026-11-15 review findings (RV-B1,
# RV-B2, RV-F1, RV-E1, RV-C1, RV-D1), sourced by diff-gate-test.sh (section 13). Uses that
# file's case_run / check_result / fixture_repo helpers and counters, and the review file's CR.
# shellcheck disable=SC2016  # snippets are eval'd inside the fixture repo on purpose

echo "diff-gate review-fix fixtures (2026-11-15)"
A=config/diff-gate-approvals.toml

# ── RV-B1: line endings in Cargo.toml ────────────────────────────────
EXPECT="new-dep" case_run "fix-b1: mixed LF, CRLF and a lone CR, dep after the lone CR" 1 : \
    'printf "[package]\nname = \"fx\"%s\n\n[dependencies]%s\nexisting = \"1\"%sevil = { git = \"https://evil\" }\n" "$CR" "$CR" "$CR" >Cargo.toml'
EXPECT="new-dep" case_run "fix-b1: a lone CR ends a comment, the key after it counts" 1 : \
    'printf "\n# note%sevil = { git = \"https://evil\" }\n" "$CR" >>Cargo.toml'
EXPECT="build-script" case_run "fix-b1: a build key after a lone CR (pass 1)" 1 : \
    'printf "[package]\nname = \"fx\"%sbuild = \"b.rs\"\n\n[dependencies]\nexisting = \"1\"\n" "$CR" >Cargo.toml'
EXPECT="new-dep" case_run "fix-b1: CRLF source change on an existing dependency" 1 : \
    'printf "[package]%s\nname = \"fx\"%s\n%s\n[dependencies]%s\nexisting = { version = \"1\", git = \"https://evil\" }%s\n" "$CR" "$CR" "$CR" "$CR" "$CR" >Cargo.toml'
case_run "fix-b1: control, LF to CRLF conversion alone is not a finding" 0 : \
    'sed -i.bak "s/\$/$CR/" Cargo.toml; rm Cargo.toml.bak'

# ── RV-B2: every spelling of a dependency entry ──────────────────────
EXPECT="new-dep" case_run "fix-b2: [workspace] with an inline dependencies table" 1 : \
    'printf "\n[workspace]\ndependencies = { evil = \"1\" }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "fix-b2: top-level dotted replace key" 1 : \
    '{ printf "replace.\"existing:1.0.0\".git = \"https://evil\"\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
EXPECT="new-dep" case_run "fix-b2: target as one inline table" 1 : \
    '{ printf "target = { \"cfg(unix)\" = { dependencies = { evil = \"1\" } } }\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
EXPECT="new-dep" case_run "fix-b2: [target] with a dotted cfg and dependencies key" 1 : \
    'printf "\n[target]\n\"cfg(unix)\".dev-dependencies.evil = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "fix-b2: [patch.crates-io] entry written as a dotted git key" 1 : \
    'printf "\n[patch.crates-io]\nexisting.git = \"https://evil\"\n" >>Cargo.toml'
EXPECT="new-dep build-script" case_run "fix-b2: a unicode escape spelling dependencies" 1 : \
    '{ printf "\"dep\\\\u0065ndencies\".evil = \"1\"\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
EXPECT="new-dep" case_run "fix-b2: dev_dependencies (underscore spelling)" 1 : 'printf "\n[dev_dependencies]\nevil = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "fix-b2: inline table over two lines, git on the second" 1 : \
    'sed -i.bak "s|^existing = \"1\"|existing = { version = \"1\",\n    git = \"https://evil\" }|" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "fix-b2: an empty table entry still resolves (version *)" 1 : 'printf "\n[dependencies.evil]\n" >>Cargo.toml'
case_run "fix-b2: control, plain to dotted form with the same source" 0 : \
    'sed -i.bak "s|^existing = \"1\"|existing.version = \"1\"|" Cargo.toml; rm Cargo.toml.bak'
case_run "fix-b2: control, [dependencies] to a top-level inline table, same entries" 0 : \
    'printf "dependencies = { existing = \"1\" }\n[package]\nname = \"fx\"\n" >Cargo.toml'
case_run "fix-b2: control, dependencies-like names under package.metadata and features" 0 : \
    'printf "\n[package.metadata.docs.rs]\ntargets = [\"x\"]\ndependencies = 1\n\n[features]\nno-dependencies = []\n" >>Cargo.toml'
EXPECT="cargo-unparsed" case_run "fix-b2: unknown key path naming a dependency table is cargo-unparsed" 1 : \
    'printf "\n[target.x.y.dependencies]\nevil = \"1\"\n" >>Cargo.toml'
EXPECT="cargo-unparsed" case_run "fix-b2: a scalar where a dependency table belongs is cargo-unparsed" 1 : \
    '{ printf "patch = \"x\"\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
EXPECT="cargo-unparsed" case_run "fix-b2: a header it cannot read, and the keys under it" 1 : \
    'printf "\n[dependencies\nevil = \"1\"\n" >>Cargo.toml'
case_run "fix-b2: control, an unparsed statement already on the base is not re-reported" 0 \
    'printf "\n[target.x.y.dependencies]\nfoo = \"1\"\n" >>Cargo.toml' 'printf "# comment\n" >>Cargo.toml'
EXPECT="cargo-unparsed" case_run "fix-b2: a new key under a header the base already could not read" 1 \
    'printf "\n[dependencies\nfoo = \"1\"\n" >>Cargo.toml' 'printf "evil = \"1\"\n" >>Cargo.toml'
case_run "fix-b2: a cargo-unparsed row clears it" 0 \
    'printf "[[approval]]\nrule = \"cargo-unparsed\"\npath = \"Cargo.toml\"\npattern = \"^\\\\[target\\\\.x\"\nreason = \"fixture\"\napproved_by = \"owner\"\n" >'$A \
    'printf "\n[target.x.y.dependencies]\n" >>Cargo.toml'

# The reader itself (--cargo-entries): every spelling of one entry gives the same key.
d="$WORK/fix_tokenizer" && mkdir -p "$d" && cd "$d" && git init -q .
printf '%s\n' 'lib.path = "src/lib.txt"' '[package]' 'name = "fx"' '' '[dependencies]' 'a = "1"' \
    'b = { git = "https://b", version = "1" }' '"c" = { path = "../c" } # comment' 'd.git = "https://d"' \
    '' '[dependencies.e]' "git = '''" 'https://e'"'''" '' '[target."cfg(unix)".dependencies]' 'f = "1"' \
    '' '[target]' "'cfg(windows)'.build-dependencies.g = \"1\"" '' '[workspace]' 'dependencies = { h = "1" }' \
    '' '[patch]' 'crates-io.i = { git = "https://i" }' '' '[replace]' '"j:1.0.0" = { git = "https://j" }' \
    '' '[[bin]]' 'name = "x"' 'path = "src/x.rs"' '' '[target.q.z.dependencies]' 'k = "1"' >Cargo.toml
out="$("$GATE" --cargo-entries "$d/Cargo.toml" 2>&1)"; rc=$?
want="$(printf '%s\n' \
    'target-path|src/lib.txt		0	1	lib.path = "src/lib.txt"' \
    'target-path|src/x.rs		0	32	path = "src/x.rs"' \
    'unparsed|target.q.z.dependencies|[target%2Eq%2Ez%2Edependencies]|1		0	34	[target.q.z.dependencies]' \
    'unparsed|target.q.z.dependencies.k|k = "1"|1		0	35	k = "1"' \
    'dependencies|a		0	6	a = "1"' \
    'dependencies|b	git=https://b;	0	7	b = { git = "https://b", version = "1" }' \
    'dependencies|c	path=../c;	0	8	"c" = { path = "../c" }' \
    'dependencies|d	git=https://d;	0	9	d.git = "https://d"' \
    'dependencies|e	git=https://e;	0	11	[dependencies.e]' \
    'target.cfg(unix).dependencies|f		0	16	f = "1"' \
    'target.cfg(windows).build-dependencies|g		0	19	'"'cfg(windows)'"'.build-dependencies.g = "1"' \
    'workspace.dependencies|h		0	22	dependencies = { h = "1" }' \
    'patch.crates-io|i	git=https://i;	0	25	crates-io.i = { git = "https://i" }' \
    'replace|j:1%2E0%2E0	git=https://j;	0	28	"j:1.0.0" = { git = "https://j" }')"
[ "$out" = "$want" ] || { rc="mismatch"; out="$(printf 'got:\n%s\nwant:\n%s\n' "$out" "$want")"; }
check_result "fix-b2: --cargo-entries resolves every spelling to its key" 0 "$rc" "$out"
cd "$ROOT" || exit 2

# ── RV-F1: test cfgs on module declarations in sensitive crates ──────
# SENS, SUP, SBASE, CALLER, TDECL come from diff-gate-test.sh section 9 and the review file.
export EVIL='pub(crate) fn stop_all() { super::signal_pgid(-1, 9); }'
TBASE="$SBASE"'; printf "%s\n" "$TDECL" >>crates/k/src/supervise.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs'
# Part A: a removed test cfg on a declaration that stays is itself a finding, with no added line.
EXPECT="sensitive-crate" case_run "fix-f1: deleting #[cfg(test)] from a file module declaration alone" 1 "$TBASE" \
    'grep -v "^#\[cfg(test)\]\$" crates/k/src/supervise.rs >s.tmp && mv s.tmp crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "fix-f1: deleting #[cfg(test)] from an inline test module alone" 1 \
    "$SBASE"'; printf "#[cfg(test)]\nmod tests {\n    pub fn t() { super::signal_pgid(1, 0); }\n}\n" >>crates/k/src/supervise.rs' \
    'grep -v "^#\[cfg(test)\]\$" crates/k/src/supervise.rs >s.tmp && mv s.tmp crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "fix-f1: cfg(test) changed to cfg(not(test)) on a declaration" 1 "$TBASE" \
    'sed -i.bak "s/^#\[cfg(test)\]\$/#[cfg(not(test))]/" crates/k/src/supervise.rs; rm crates/k/src/supervise.rs.bak'
EXPECT="sensitive-crate" case_run "fix-f1: cfg(all(test, unix)) changed to cfg(unix) with an attribute between" 1 \
    "$SBASE"'; printf "#[cfg(all(test, unix))]\n#[allow(dead_code)]\n// note\n#[path = \"supervise_tests.rs\"]\nmod helpers;\n" >>crates/k/src/supervise.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' \
    'sed -i.bak "s/^#\[cfg(all(test, unix))\]\$/#[cfg(unix)]/" crates/k/src/supervise.rs; rm crates/k/src/supervise.rs.bak'
case_run "fix-f1: control, deleting a whole test module (cfg line and all) is not a finding" 0 \
    "$SBASE"'; printf "#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n" >>crates/k/src/supervise.rs' \
    'printf "%s\n" "$SUP" >crates/k/src/supervise.rs'
case_run "fix-f1: control, deleting #[cfg(test)] from a fn is not a module exposure" 0 \
    "$SBASE"'; printf "#[cfg(test)]\nfn t() {}\n" >>crates/k/src/supervise.rs' \
    'grep -v "^#\[cfg(test)\]\$" crates/k/src/supervise.rs >s.tmp && mv s.tmp crates/k/src/supervise.rs'
case_run "fix-f1: control, cfg(test) outside a sensitive crate" 0 \
    "$SBASE"'; printf "%s\n" "$TDECL" >>crates/other/src/supervise.rs' \
    'grep -v "^#\[cfg(test)\]\$" crates/other/src/supervise.rs >s.tmp && mv s.tmp crates/other/src/supervise.rs'
# Part B: an exempt file is test-only only if every declaration that can build it is a test one.
EXPECT="sensitive-crate" GREP="is not treated as test-only" case_run "fix-f1: exempt file also pulled in by a non-test #[path] elsewhere" 1 \
    "$TBASE"'; printf "#[path = \"supervise_tests.rs\"]\nmod alias;\n" >>crates/k/src/lib.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
EXPECT="sensitive-crate" case_run "fix-f1: exempt file declared after other code on the same line" 1 \
    "$SBASE"'; printf "fn a() {} mod supervise_tests;\n" >>crates/k/src/lib.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
EXPECT="sensitive-crate" case_run "fix-f1: exempt file pulled in by include! outside a test cfg" 1 \
    "$SBASE"'; printf "include!(\"supervise_tests.rs\");\n" >>crates/k/src/supervise.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
EXPECT="sensitive-crate" GREP="Cargo target path" case_run "fix-f1: exempt file that is a Cargo [[bin]] target on the base" 1 \
    "$SBASE"'; printf "[package]\nname = \"k\"\n\n[[bin]]\nname = \"x\"\npath = \"src/x_tests.rs\"\n" >crates/k/Cargo.toml; echo "fn main() {}" >crates/k/src/x_tests.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/x_tests.rs'
EXPECT="sensitive-crate" case_run "fix-f1: an exempt-named file under src/bin/ is a binary" 1 "$SBASE" \
    'mkdir -p crates/k/src/bin && printf "fn main() {}\n%s\n" "$EVIL" >crates/k/src/bin/x_tests.rs'
EXPECT="sensitive-crate" case_run "fix-f1: a tests/ module declared by an exempt file that is not itself a test" 1 \
    "$SBASE"'; mkdir -p crates/k/src/tests; printf "pub mod helpers;\n" >crates/k/src/tests/mod.rs; echo "pub fn h() {}" >crates/k/src/tests/helpers.rs; printf "pub mod tests;\n" >>crates/k/src/supervise.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/tests/helpers.rs'
case_run "fix-f1: control, a tests/ module declared by a test-declared exempt file is exempt" 0 \
    "$SBASE"'; mkdir -p crates/k/src/tests; printf "pub mod helpers;\n" >crates/k/src/tests/mod.rs; echo "pub fn h() {}" >crates/k/src/tests/helpers.rs; printf "#[cfg(test)]\npub mod tests;\n" >>crates/k/src/supervise.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/tests/helpers.rs'
case_run "fix-f1: control, a multi-line cfg(all(test, ...)) declaration is exempt" 0 \
    "$SBASE"'; printf "#[cfg(all(\n    test,\n    unix\n))]\n#[path = \"supervise_tests.rs\"]\nmod helpers;\n" >>crates/k/src/supervise.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
case_run "fix-f1: control, a declaration inside an inline test module is exempt" 0 \
    "$SBASE"'; printf "#[cfg(test)]\nmod t {\n    #[path = \"../supervise_tests.rs\"]\n    mod helpers;\n}\n" >>crates/k/src/supervise.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
case_run "fix-f1: control, a mention in a comment or string is not a declaration" 0 \
    "$TBASE"'; printf "// mod supervise_tests;\nconst S: &str = \"supervise_tests.rs\";\n" >>crates/k/src/lib.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
# --worktree reads declarations from the working tree, untracked files included.
d="$WORK/fix_f1_wt" && fixture_repo "$d" && eval "$TBASE" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
printf '#[path = "supervise_tests.rs"]\nmod alias;\n' >crates/k/src/untracked_decl.rs && printf '%s\n' "$EVIL" >>crates/k/src/supervise_tests.rs
out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
EXPECT="sensitive-crate" GREP="untracked_decl.rs" check_result "fix-f1: worktree: an untracked non-test declaration counts" 1 "$rc" "$out"
cd "$ROOT" || exit 2

# ── RV-E1: --worktree and git-ignored files ──────────────────────────
wt_ignored() { # NAME WANT SETUP: SETUP runs after the base commit; the gate runs with --worktree
    local d="$WORK/$(echo "$1" | tr -c 'A-Za-z0-9\n' '_')"
    fixture_repo "$d" && git add -A && git commit -qm base && local base; base="$(git rev-parse HEAD)"
    eval "$3"
    local out rc; out="$(DIFF_GATE_BASE="$base" "$GATE" --worktree 2>&1)"; rc=$?
    check_result "$1" "$2" "$rc" "$out"
    cd "$ROOT" || exit 2
}
EXPECT="unsafe" GREP="git-ignored file scanned" wt_ignored "fix-e1: worktree: a .rs file in an ignored directory is scanned" 1 \
    'echo "src/gen/" >.gitignore; mkdir -p src/gen; echo "fn f() { unsafe { g() } }" >src/gen/mod.rs; echo "mod gen;" >>src/lib.rs'
EXPECT="new-dep" wt_ignored "fix-e1: worktree: an ignored Cargo.toml is read" 1 \
    'echo "vendored/" >.gitignore; mkdir -p vendored/x; printf "[package]\nname = \"x\"\n\n[dependencies]\nevil = \"1\"\n" >vendored/x/Cargo.toml'
EXPECT="toolchain-config" wt_ignored "fix-e1: worktree: an ignored .cargo/config.toml is reported" 1 \
    'echo ".cargo/" >.gitignore; mkdir -p .cargo; printf "[build]\nrustc-wrapper = \"/tmp/x\"\n" >.cargo/config.toml'
wt_ignored "fix-e1: control, worktree: build output under target/ is not scanned" 0 \
    'echo "target/" >.gitignore; mkdir -p target/debug/build/x/out; echo "fn f() { unsafe { g() } }" >target/debug/build/x/out/gen.rs'
wt_ignored "fix-e1: control, worktree: ignored node_modules and .embuild are not scanned" 0 \
    'printf "node_modules/\n.embuild/\n" >.gitignore; mkdir -p node_modules/x .embuild/y; echo "fn f() { unsafe { g() } }" | tee node_modules/x/a.rs >.embuild/y/b.rs'
wt_ignored "fix-e1: control, worktree: an ignored .gitattributes is not a finding" 0 \
    'printf ".gitattributes\n" >.gitignore; echo "*.rs -diff" >.gitattributes'
# commit mode never reads untracked files, ignored or not
d="$WORK/fix_e1_commit" && fixture_repo "$d" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
echo "src/gen.rs" >.gitignore && echo "fn f() { unsafe { g() } }" >src/gen.rs && git add .gitignore && git commit -qm change
out="$(DIFF_GATE_BASE="$base" "$GATE" 2>&1)"; rc=$?
check_result "fix-e1: control, commit mode ignores untracked ignored files" 0 "$rc" "$out"
cd "$ROOT" || exit 2

# ── RV-C1: the helper cache must resolve outside the checkout ────────
# Each case: base with tools/diff-gate-tokens, a .rs change, no prebuilt helper, no cargo on PATH.
c1_case() { # NAME WANT GREP SETUP: SETUP runs in the fixture repo after both commits ($tree is set)
    local d="$WORK/$(echo "$1" | tr -c 'A-Za-z0-9\n' '_')" base tree out rc
    fixture_repo "$d" && mkdir -p tools/diff-gate-tokens && echo x >tools/diff-gate-tokens/README.md
    git add -A && git commit -qm base && base="$(git rev-parse HEAD)" && tree="$(git rev-parse HEAD:tools/diff-gate-tokens)"
    echo "fn z() {}" >>src/lib.rs && git add -A && git commit -qm change
    eval "$4"
    out="$(env DIFF_GATE_BASE="$base" DIFF_GATE_TOKENS_BIN="" DIFF_GATE_CACHE="$C1CACHE" PATH="/usr/bin:/bin" "$GATE" 2>&1)"; rc=$?
    EXPECT="rust-tokens" GREP="$3" check_result "$1" "$2" "$rc" "$out"
    cd "$ROOT" || exit 2
}
c1_case "fix-c1: a cache under a parent symlinked into the checkout, not created yet" 1 "inside the checkout" \
    'mkdir -p target/p && ln -s "$PWD/target/p" "$WORK/c1-parent" && C1CACHE="$WORK/c1-parent/new/cache"'
c1_case "fix-c1: tokens-<tree> symlinked into the checkout" 1 "resolves into the checkout" \
    'mkdir -p target/t "$WORK/c1-t" && cp "$STUB" target/t/diff-gate-tokens && ln -s "$PWD/target/t" "$WORK/c1-t/tokens-$tree" && C1CACHE="$WORK/c1-t"'
c1_case "fix-c1: a cached helper that is a symlink is refused" 1 "is a symlink" \
    'mkdir -p "$WORK/c1-l/tokens-$tree" && ln -s "$STUB" "$WORK/c1-l/tokens-$tree/diff-gate-tokens" && C1CACHE="$WORK/c1-l"'
c1_case "fix-c1: a .. in the part of the cache path that does not exist yet" 1 "cannot be resolved" \
    'C1CACHE="$WORK/c1-none/../c1-none2"'
c1_case "fix-c1: control, a symlink to a directory outside the checkout is accepted" 1 "cargo not found" \
    'mkdir -p "$WORK/c1-out" && ln -s "$WORK/c1-out" "$WORK/c1-outlink" && C1CACHE="$WORK/c1-outlink"'
