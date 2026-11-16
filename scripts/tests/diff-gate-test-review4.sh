# shellcheck shell=bash
# diff-gate-test-review4.sh: review round 4 fixtures (2026-11-15), sourced by diff-gate-test.sh
# (section 15). Uses that file's case_run / check_result / fixture_repo / appr helpers and
# counters, section 9's SBASE / SENS / EVIL and the round 3 file's A and S. KNOWN_GAP=<id> marks
# a case the gate misclassified; see reports/review-diff-gate-combined-r4-20261115.md. The round 4
# fixes (w2/diff-gate-r4-fixes) removed every marker; the cases after each "R4 fix" comment were
# added with them. R4G <id> gives the id only with the real helper.
# shellcheck disable=SC2016  # snippets are eval'd inside the fixture repo on purpose

echo "diff-gate review round 4 fixtures (2026-11-15)"
A=config/diff-gate-approvals.toml
R4G() { [ "$REAL_TOKENS" = 1 ] && echo "$1"; }

# ── (a) the cargo metadata step ──────────────────────────────────────
# A workspace: a virtual root with crates/* members and [workspace.dependencies].
export WS_ROOT='[workspace]
members = ["crates/*"]
resolver = "2"

[workspace.dependencies]
serde = "1"'
WSB='printf "%s\n" "$WS_ROOT" >Cargo.toml; rm -f src/lib.rs; for c in x y; do mkdir -p crates/$c/src; echo "pub fn a() {}" >crates/$c/src/lib.rs; printf "[package]\nname = \"%s\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { workspace = true }\n" $c >crates/$c/Cargo.toml; done'
EXPECT="new-dep" case_run "r4-a: a [workspace.dependencies] source change reaches every inheriting member" 1 "$WSB" \
    'sed -i.bak "s|^serde = \"1\"|serde = { git = \"https://evil\" }|" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "r4-a: [workspace.dependencies.serde] table form with a git source" 1 "$WSB" \
    'grep -v "^serde = \"1\"" Cargo.toml >c.tmp && mv c.tmp Cargo.toml; printf "\n[workspace.dependencies.serde]\nversion = \"1\"\ngit = \"https://evil\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r4-a: a package added through a glob member brings a new dependency" 1 "$WSB" \
    'mkdir -p crates/z/src && echo "pub fn a() {}" >crates/z/src/lib.rs && printf "[package]\nname = \"z\"\nversion = \"0.1.0\"\n\n[dependencies]\nevil = { git = \"https://evil\" }\n" >crates/z/Cargo.toml'
EXPECT="build-script" case_run "r4-a: a package added through a glob member with its own build.rs" 1 "$WSB" \
    'mkdir -p crates/z/src && echo "pub fn a() {}" >crates/z/src/lib.rs && echo "fn main() {}" >crates/z/build.rs && printf "[package]\nname = \"z\"\nversion = \"0.1.0\"\n" >crates/z/Cargo.toml'
case_run "r4-a: control, removing a package from the workspace is not a finding" 0 "$WSB" 'git rm -rq crates/y'
case_run "r4-a: control, default-members is not a finding" 0 "$WSB" \
    'sed -i.bak "s|^resolver = \"2\"|resolver = \"2\"\ndefault-members = [\"crates/x\"]|" Cargo.toml; rm Cargo.toml.bak'
case_run "r4-a: control, a workspace = true entry the root declares is not new" 0 "$WSB" \
    'printf "\n[build-dependencies]\nserde = { workspace = true }\n" >>crates/x/Cargo.toml'
# A nested workspace (tools/w) whose own [workspace.dependencies] names serde from git. The
# repo-root workspace also declares serde (crates.io), so a name check against the root alone
# would call a new `serde = { workspace = true }` in tools/w/m not new; Cargo resolves it to git.
export NW_ROOT='[workspace]
members = ["m"]

[workspace.dependencies]
serde = { git = "https://evil" }'
NWB="$WSB"'; mkdir -p tools/w/m/src; printf "%s\n" "$NW_ROOT" >tools/w/Cargo.toml; echo "pub fn a() {}" >tools/w/m/src/lib.rs; printf "[package]\nname = \"m\"\nversion = \"0.1.0\"\n\n[dependencies]\n" >tools/w/m/Cargo.toml; sed -i.bak "s|^resolver = \"2\"|resolver = \"2\"\nexclude = [\"tools\"]|" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "r4-a: workspace = true in a nested workspace resolves to its own (git) declaration" 1 "$NWB" \
    'printf "serde = { workspace = true }\n" >>tools/w/m/Cargo.toml'
# [patch] / [replace]: only the TOML reader sees them (cargo metadata --no-deps does not).
EXPECT="new-dep" case_run "r4-a: top-level dotted patch.crates-io.<name>.git" 1 : \
    '{ printf "patch.crates-io.existing.git = \"https://evil\"\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
EXPECT="new-dep" case_run "r4-a: [patch.<registry URL>] quoted table name" 1 : \
    'printf "\n[patch.\"https://github.com/rust-lang/crates.io-index\"]\nexisting = { git = \"https://evil\" }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r4-a: [replace] with a quoted package id key" 1 : \
    'printf "\n[replace]\n\"existing:1.0.0\" = { git = \"https://evil\" }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r4-a: [patch] in the workspace root reaches members" 1 "$WSB" \
    'printf "\n[patch.crates-io]\nserde = { path = \"vendor/serde\" }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r4-a: an existing dependency moved from cfg(windows) to cfg(unix)" 1 \
    'printf "\n[target.\x27cfg(windows)\x27.dependencies]\nwinonly = { git = \"https://evil\" }\n" >>Cargo.toml' \
    'sed -i.bak "s/cfg(windows)/cfg(unix)/" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "r4-a: [target.<triple>.dependencies] new entry" 1 : \
    'printf "\n[target.x86_64-unknown-linux-gnu.dependencies]\nevil = \"1\"\n" >>Cargo.toml'
# build key moved into [package] with a path cargo normalizes (./, ..): Cargo reports the
# normalized path, so the moved key is still seen.
EXPECT="build-script" case_run "r4-a: a ./-prefixed build key moved into [package] by deleting a header" 1 \
    'printf "[package]\nname = \"fx\"\n\n[package.metadata.x]\nbuild = \"./src/b.rs\"\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml; echo "fn main() {}" >src/b.rs' \
    'grep -v "^\[package.metadata.x\]" Cargo.toml >c.tmp && mv c.tmp Cargo.toml'
EXPECT="build-script" case_run "r4-a: a build key through .. moved into [package] by deleting a header" 1 \
    'printf "[package]\nname = \"fx\"\n\n[package.metadata.x]\nbuild = \"src/../src/b.rs\"\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml; echo "fn main() {}" >src/b.rs' \
    'grep -v "^\[package.metadata.x\]" Cargo.toml >c.tmp && mv c.tmp Cargo.toml'
# A build.rs symlink that dangles on the base builds nothing (cargo checks is_file); the diff
# adds its target, and cargo now runs it. The placeholder tree reproduces symlinks as symlinks.
EXPECT="build-script" case_run "r4-a: adding the target of a dangling build.rs symlink turns the build script on" 1 \
    'ln -s tools/b.rs build.rs' 'mkdir -p tools && echo "fn main() {}" >tools/b.rs'
# R4 fix: a target that is not .rs, a link that leaves the tree, and the reverse direction.
EXPECT="build-script" case_run "r4-a: adding the non-.rs target of a dangling build.rs symlink" 1 \
    'ln -s tools/b.txt build.rs' 'mkdir -p tools && echo "fn main() {}" >tools/b.txt'
case_run "r4-a: control, an unchanged build.rs symlink that leaves the tree is not a new build script" 0 \
    'ln -s /nonexistent/b.rs build.rs' 'echo "pub fn n() {}" >src/n.rs'
case_run "r4-a: control, removing the target of a build.rs symlink is not a finding" 0 \
    'mkdir -p tools && echo "fn main() {}" >tools/b.rs && ln -s tools/b.rs build.rs' 'git rm -q tools/b.rs'
# Auto-discovered targets: autobins / autoexamples / autotests.
case_run "r4-a: control, deleting autobins = false outside a sensitive crate is not a finding" 0 \
    'sed -i.bak "s/^name = \"fx\"/name = \"fx\"\nautobins = false/" Cargo.toml; rm Cargo.toml.bak; mkdir -p src/bin; echo "fn main() {}" >src/bin/t.rs' \
    'grep -v "^autobins" Cargo.toml >c.tmp && mv c.tmp Cargo.toml'
SAB="$SBASE"'; printf "[package]\nname = \"k\"\nautobins = false\n" >crates/k/Cargo.toml; mkdir -p crates/k/src/bin; echo "fn main() {}" >crates/k/src/bin/tool_tests.rs'
EXPECT="sensitive-crate" case_run "r4-a: deleting autobins = false in a sensitive crate turns an existing file into a bin" 1 "$SAB" \
    'grep -v "^autobins" crates/k/Cargo.toml >c.tmp && mv c.tmp crates/k/Cargo.toml'
EXPECT="sensitive-crate" case_run "r4-a: deleting autobins = false exposes a name-exempt src/bin file as a root" 1 "$SAB" \
    'grep -v "^autobins" crates/k/Cargo.toml >c.tmp && mv c.tmp crates/k/Cargo.toml; printf "%s\n" "$EVIL" >>crates/k/src/bin/tool_tests.rs'
case_run "r4-a: control, deleting autoexamples = false in a sensitive crate (example targets are test roots)" 0 \
    "$SBASE"'; printf "[package]\nname = \"k\"\nautoexamples = false\n" >crates/k/Cargo.toml; mkdir -p crates/k/examples; echo "fn main() {}" >crates/k/examples/e.rs' \
    'grep -v "^autoexamples" crates/k/Cargo.toml >c.tmp && mv c.tmp crates/k/Cargo.toml'
# A build script spelled Build.RS: a case-insensitive disk (macOS default) lets cargo find it as
# build.rs. The path rule compares lower-cased names.
EXPECT="build-script" case_run "r4-a: control, a new Build.RS is a build script (case-insensitive disks)" 1 : 'echo "fn main() {}" >Build.RS'
# cargo absent, failing, or giving an empty answer: fail closed.
EXPECT="cargo-unparsed" GREP="cargo metadata unavailable" case_run "r4-e: cargo absent (DIFF_GATE_CARGO names nothing)" 1 : \
    'printf "serde = \"1\"\n" >>Cargo.toml' DIFF_GATE_CARGO=/nonexistent/cargo
export FAKE_FAIL="$WORK/fake-cargo-fail" FAKE_EMPTY="$WORK/fake-cargo-empty" FAKE_GARBAGE="$WORK/fake-cargo-garbage"
printf '#!/bin/sh\necho "error: simulated failure" >&2\nexit 101\n' >"$FAKE_FAIL"
printf '#!/bin/sh\necho "{\\"packages\\":[],\\"workspace_members\\":[],\\"workspace_root\\":\\"/\\",\\"version\\":1}"\n' >"$FAKE_EMPTY"
printf '#!/bin/sh\necho "{\\"packages\\":[{\\"manifest_path\\""\n' >"$FAKE_GARBAGE"
chmod +x "$FAKE_FAIL" "$FAKE_EMPTY" "$FAKE_GARBAGE"
EXPECT="cargo-unparsed" GREP="simulated failure" case_run "r4-e: cargo fails on every run" 1 : \
    'printf "serde = \"1\"\n" >>Cargo.toml' DIFF_GATE_CARGO="$FAKE_FAIL"
EXPECT="cargo-unparsed" case_run "r4-e: cargo prints truncated JSON" 1 : \
    'printf "serde = \"1\"\n" >>Cargo.toml' DIFF_GATE_CARGO="$FAKE_GARBAGE"
# An answer with no package for the manifest it was asked about: with the build key moved by a
# deleted header, only cargo can see the build script. DIFF_GATE_CARGO is an operator setting,
# and real cargo always reports the package it was pointed at (LOW). Now cargo-unparsed.
EXPECT="cargo-unparsed" GREP="did not report the package" case_run "r4-e: cargo answers with no package for the manifest it was asked about" 1 \
    'printf "[package]\nname = \"fx\"\n\n[package.metadata.x]\nbuild = \"src/b.rs\"\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml; echo "fn main() {}" >src/b.rs' \
    'grep -v "^\[package.metadata.x\]" Cargo.toml >c.tmp && mv c.tmp Cargo.toml' DIFF_GATE_CARGO="$FAKE_EMPTY"
# Scale: 150 workspace members, one adds a git dependency; and a 3000-entry manifest.
MANY="$WSB"'; for i in $(seq 1 150); do mkdir -p crates/m$i/src; : >crates/m$i/src/lib.rs; printf "[package]\nname = \"m%s\"\nversion = \"0.1.0\"\n\n[dependencies]\nserde = { workspace = true }\n" $i >crates/m$i/Cargo.toml; done'
t0=$(date +%s)
EXPECT="new-dep" case_run "r4-e: 150 workspace members, one new git dependency" 1 "$MANY" \
    'printf "evil = { git = \"https://evil\" }\n" >>crates/m77/Cargo.toml'
echo "        (150-member case took $(( $(date +%s) - t0 )) s)"
t0=$(date +%s)
EXPECT="new-dep" case_run "r4-e: a 3000-entry [dependencies] table, one new git source" 1 \
    'for i in $(seq 1 3000); do printf "d%s = \"1\"\n" $i; done >>Cargo.toml' \
    'sed -i.bak "s|^d1500 = \"1\"|d1500 = { git = \"https://evil\" }|" Cargo.toml; rm Cargo.toml.bak'
echo "        (3000-entry case took $(( $(date +%s) - t0 )) s)"

# Package directories with unusual names: only cargo sees a build key moved by a deleted header,
# so each case needs cargo's answer to be mapped back to the right package. R4-A2: the records
# escape "%" as "%25" and the build-script check compares them with the raw file list.
for dn in "my crate" "a%2Eb" "a%25b" "100%" "a|b" 'a\b' 'a"b' "$(printf '\303\251t\303\251')" "-x" "%0A"; do
    export R4DN="$dn"
    EXPECT="build-script" case_run "r4-e: moved build key in a package directory named [$dn]" 1 \
        'mkdir -p "crates/$R4DN/src" && : >"crates/$R4DN/src/lib.rs" && echo "fn main() {}" >"crates/$R4DN/b.rs" && printf "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[package.metadata.x]\nbuild = \"b.rs\"\n" >"crates/$R4DN/Cargo.toml"' \
        'grep -v "^\[package.metadata.x\]" "crates/$R4DN/Cargo.toml" >c.tmp && mv c.tmp "crates/$R4DN/Cargo.toml"'
done

# R4 fix: a path holding a tab or a newline is refused before anything runs (exit 2); the cargo
# records refuse any other value holding one (cargo-unparsed).
export R4TAB="$(printf 'a\tb')"
GREP="control character" case_run "r4-e: a package directory whose name holds a tab is refused" 2 \
    'mkdir -p "crates/$R4TAB/src" && : >"crates/$R4TAB/src/lib.rs" && echo "fn main() {}" >"crates/$R4TAB/b.rs" && printf "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[package.metadata.x]\nbuild = \"b.rs\"\n" >"crates/$R4TAB/Cargo.toml"' \
    'grep -v "^\[package.metadata.x\]" "crates/$R4TAB/Cargo.toml" >c.tmp && mv c.tmp "crates/$R4TAB/Cargo.toml"'
case_run "r4-e: a package directory whose name holds a newline is refused" 2 : \
    'mkdir -p "crates/a
b/src" && : >"crates/a
b/src/lib.rs" && printf "[package]\nname = \"x\"\n" >"crates/a
b/Cargo.toml"'
# A virtual workspace root is the one manifest cargo answers for without a package of its own.
case_run "r4-e: control, a virtual workspace root changed (cargo reports members only)" 0 "$WSB" \
    'printf "\n# a comment\n" >>Cargo.toml'

# ── (b) the reachability walk (sensitive crates) ─────────────────────
# KB: crate k declares supervise; supervise_tests.rs is exempt by name.
R4KB="$SBASE"'; printf "pub mod supervise;\n" >crates/k/src/lib.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs'
R4EV='printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
# rustc builds supervise_tests.rs in each of these (checked on PG, cargo build, 1.95).
EXPECT="sensitive-crate" case_run "r4-b: mod \$name inside macro_rules, the name passed by the call" 1 \
    "$R4KB"'; printf "macro_rules! decl {\n    (\$n:ident) => {\n        pub mod \$n;\n    };\n}\ndecl!(supervise_tests);\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: a repeated mod \$name list inside macro_rules" 1 \
    "$R4KB"'; echo "pub fn a() {}" >crates/k/src/a.rs; printf "macro_rules! decl {\n    (\$(\$n:ident),*) => { \$(pub mod \$n;)* };\n}\ndecl!(a, supervise_tests);\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: #[path = \$p] inside macro_rules, the path passed by the call" 1 \
    "$R4KB"'; printf "macro_rules! at {\n    (\$p:literal) => {\n        #[path = \$p]\n        pub mod helpers;\n    };\n}\nat!(\"supervise_tests.rs\");\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: #[r#path = ...] (a raw identifier attribute name)" 1 \
    "$R4KB"'; printf "#[r#path = \"supervise_tests.rs\"]\nmod helpers;\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: #[r#cfg_attr(not(test), path = ...)]" 1 \
    "$R4KB"'; printf "#[r#cfg_attr(not(test), path = \"supervise_tests.rs\")]\nmod helpers;\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: include! with concat!/env! fails closed" 1 \
    "$R4KB"'; printf "include!(concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/src/supervise_tests.rs\"));\n" >>crates/k/src/supervise.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: include! of a macro_rules literal fails closed" 1 \
    "$R4KB"'; printf "macro_rules! inc { (\$p:literal) => { include!(\$p); }; }\ninc!(\"supervise_tests.rs\");\n" >>crates/k/src/supervise.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: #[path] inside an inline module of a non-mod.rs file" 1 \
    "$R4KB"'; printf "pub mod a;\n" >>crates/k/src/lib.rs; printf "pub mod inner {\n    #[path = \"x_tests.rs\"]\n    pub mod c;\n}\n" >crates/k/src/a.rs; mkdir -p crates/k/src/a/inner; echo "fn t() {}" >crates/k/src/a/inner/x_tests.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/a/inner/x_tests.rs'
EXPECT="sensitive-crate" case_run "r4-b: mod inside a macro call body (cfg_if style) is followed" 1 \
    "$R4KB"'; printf "cfg_if::cfg_if! {\n    if #[cfg(unix)] {\n        mod supervise_tests;\n    }\n}\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: mod r#name; is a reference" 1 "$R4KB"'; printf "mod r#supervise_tests;\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: a file reached only from build.rs (a non-test root)" 1 \
    "$R4KB"'; printf "#[path = \"src/supervise_tests.rs\"]\nmod h;\nfn main() {}\n" >crates/k/build.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: a proc-macro library declaring the exempt file" 1 \
    "$R4KB"'; printf "[package]\nname = \"k\"\n\n[lib]\nproc-macro = true\n" >crates/k/Cargo.toml; printf "mod supervise_tests;\n" >>crates/k/src/lib.rs' "$R4EV"
# R4-A2 again: a root whose path holds "%" reaches the helper as "%25" and is silently skipped.
EXPECT="sensitive-crate" case_run "r4-b: a lib root whose file name holds % declares the exempt file" 1 \
    "$SBASE"'; printf "[package]\nname = \"k\"\n\n[lib]\npath = \"src/lib%%.rs\"\n" >crates/k/Cargo.toml; printf "pub mod supervise;\nmod supervise_tests;\n" >"crates/k/src/lib%.rs"; rm crates/k/src/lib.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs' "$R4EV"
# R4 fix: a macro invoked in item position that the walk cannot see, and a local macro that
# hands its input to one, fail closed; std's thread_local! and a local macro that only uses std
# macros do not.
EXPECT="sensitive-crate" GREP="$(R4G 'may declare a module')" case_run "r4-b: a macro from another crate invoked in item position" 1 \
    "$R4KB"'; printf "some_dep::decl!(supervise_tests);\n" >>crates/k/src/lib.rs' "$R4EV"
EXPECT="sensitive-crate" case_run "r4-b: a local macro that forwards its input to another crate's macro" 1 \
    "$R4KB"'; printf "macro_rules! wrap {\n    (\$n:ident) => { some_dep::decl!(\$n); };\n}\nwrap!(supervise_tests);\n" >>crates/k/src/lib.rs' "$R4EV"
NEEDS_REACH=1 case_run "r4-b: control, thread_local! and a std-only local macro keep the exemption" 0 \
    "$R4KB"'; printf "thread_local! { static X: u8 = 0; }\nmacro_rules! k { () => { pub const K: &str = stringify!(k); }; }\nk!();\n" >>crates/k/src/lib.rs' "$R4EV"
# A nested package whose directory holds a backslash: awk -v read "\b" as a backspace, so its
# Cargo roots went unmatched and its exempt files passed. Paths now reach awk through ENVIRON,
# and a package with no non-test root fails closed.
export R4BS='crates/k/a\b'
EXPECT="sensitive-crate" case_run "r4-b: a nested package whose directory holds a backslash declares the exempt file" 1 \
    "$SBASE"'; mkdir -p "$R4BS/src"; printf "[package]\nname = \"ab\"\n" >"$R4BS/Cargo.toml"; printf "mod x_tests;\n" >"$R4BS/src/lib.rs"; echo "fn t() {}" >"$R4BS/src/x_tests.rs"' \
    'printf "%s\n" "$EVIL" >>"$R4BS/src/x_tests.rs"'
NEEDS_REACH=1 case_run "r4-b: control, a file reached only from an example stays exempt" 0 \
    "$R4KB"'; mkdir -p crates/k/examples; printf "#[path = \"../src/supervise_tests.rs\"]\nmod h;\nfn main() {}\n" >crates/k/examples/e.rs' "$R4EV"
NEEDS_REACH=1 case_run "r4-b: control, a file reached only from a bench stays exempt" 0 \
    "$R4KB"'; mkdir -p crates/k/benches; printf "#[path = \"../src/supervise_tests.rs\"]\nmod h;\nfn main() {}\n" >crates/k/benches/b.rs' "$R4EV"
NEEDS_REACH=1 case_run "r4-b: control, a cfg(test) outer module with a re-export inside stays exempt" 0 \
    "$R4KB"'; printf "#[cfg(test)]\npub mod t {\n    pub use super::supervise::*;\n    pub mod supervise_tests;\n}\n" >>crates/k/src/lib.rs; mkdir -p crates/k/src/t; echo "fn t() {}" >crates/k/src/t/supervise_tests.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/src/t/supervise_tests.rs'

# #[path] to a non-.rs file is an include finding; a raw identifier attribute name is the same
# attribute to rustc (#[r#path] built on PG).
EXPECT="include" case_run "r4-b: #[r#path = ...] to a non-.rs file" 1 : 'echo "pub fn g() {}" >src/gen.txt; printf "#[r#path = \"gen.txt\"]\nmod gen;\n" >>src/lib.rs'
EXPECT="include" case_run "r4-b: #[cfg_attr(unix, r#path = ...)] to a non-.rs file" 1 : 'echo "pub fn g() {}" >src/gen.txt; printf "#[cfg_attr(unix, r#path = \"gen.txt\")]\nmod gen;\n" >>src/lib.rs'

# ── (c) row trust ────────────────────────────────────────────────────
# r4row FILE PATTERN [BLOB] [EXTRA]: an unsafe row for src/**; EXTRA is appended inside the row.
r4row() { printf '[[approval]]\nrule = "unsafe"\npath = "src/**"\npattern = "%s"\n' "$1"; [ -n "${2:-}" ] && printf 'blob = "%s"\n' "$2"; printf 'reason = "fixture"\napproved_by = "owner"\n%s' "${3:-}"; }
export -f r4row
R4U='echo "fn f() { unsafe { g() } }" >>src/lib.rs'
case_run "r4-c: control, the base-tip row approves" 0 'r4row "unsafe { g() }" >'$A "$R4U"
EXPECT="unsafe approvals-change" GREP="approves nothing" case_run "r4-c: a base-tip row widened on the head approves nothing" 1 \
    'r4row "unsafe { g() }" >'$A 'r4row "unsafe" >'$A'; '"$R4U"
EXPECT="unsafe approvals-change" case_run "r4-c: a second pattern key appended to a trusted row (last value wins) approves nothing" 1 \
    'r4row "unsafe { g() }" >'$A 'r4row "unsafe { g() }" "" "pattern = \"*\"
" >'$A'; '"$R4U"
case_run "r4-c: control, the same row re-quoted, re-ordered and commented stays trusted" 0 \
    'r4row "unsafe { g() }" >'$A \
    'printf "# reviewed\n[[approval]]\napproved_by = \x27owner\x27 # x\nreason = \"fixture\"\npattern = \x27unsafe { g() }\x27\npath = \"src/**\"\nrule = \"unsafe\"\n" >'$A'; '"$R4U"
EXPECT="unsafe approvals-change" case_run "r4-c: a trusted blob row re-pointed at the new content approves nothing" 1 \
    'r4row "*" "$(printf "pub fn a() {}\n" | git hash-object --stdin)" >'$A \
    'r4row "*" "$(printf "pub fn a() {}\nfn f() { unsafe { g() } }\n" | git hash-object --stdin)" >'$A'; '"$R4U"
case_run "r4-c: control, an upper-case blob id equals its lower-case row" 0 \
    'r4row "*" "$(printf "pub fn a() {}\nfn f() { unsafe { g() } }\n" | git hash-object --stdin)" >'$A \
    'r4row "*" "$(printf "pub fn a() {}\nfn f() { unsafe { g() } }\n" | git hash-object --stdin | tr a-f A-F)" >'$A'; '"$R4U"
EXPECT="unsafe" case_run "r4-c: a ledger entry for the row without blob does not trust the row with one" 1 \
    "printf '[[entry]]\ndecision = \"d\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n' \"\$(printf 'rule=unsafe\npath=src/**\npattern=*\nreason=fixture\napproved_by=owner\n' | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1)\" >config/diff-gate-owner-ledger.toml" \
    'r4row "*" "$(printf "pub fn a() {}\nfn f() { unsafe { g() } }\n" | git hash-object --stdin)" >'$A'; '"$R4U"
case_run "r4-c: control, a tip ledger entry trusts the row it hashes" 0 \
    "printf '[[entry]]\ndecision = \"d\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n' \"\$(printf 'rule=unsafe\npath=src/**\npattern=*\nreason=fixture\napproved_by=owner\n' | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1)\" >config/diff-gate-owner-ledger.toml" \
    'r4row "*" >'$A'; '"$R4U"

# ── (d) doc attributes and doctests ──────────────────────────────────
# Each of these doctests is run by `cargo test --doc` (checked on PG, rustdoc 1.95: a panicking
# doctest in each form ran). rustdoc reads code blocks anywhere in the Markdown, including list
# items and block quotes, strips the leading " * " of block doc comments, joins an item's outer
# and inner docs into one text, and treats r#doc / r#cfg_attr like doc / cfg_attr.
export DOC_LIST_OWN='/// Steps:
///
/// - run it:
///   ```
///   std::process::Command::new("sh").status().unwrap();
///   ```
pub fn d() {}'
export DOC_LIST='/// - ```
///   std::process::Command::new("sh").status().unwrap();
///   ```
pub fn d() {}'
export DOC_QUOTE='/// > ```
/// > std::process::Command::new("sh").status().unwrap();
/// > ```
pub fn d() {}'
export DOC_STAR='/**
 * ```
 * let _s = r"
 * "; std::process::Command::new("sh").status().unwrap();
 * ```
 */
pub fn d() {}'
export DOC_STAR_PLAIN='/**
 * ```
 * std::process::Command::new("sh").status().unwrap();
 * ```
 */
pub fn d() {}'
export DOC_MERGE='/// ```
pub mod m {
    //! std::process::Command::new("sh").status().unwrap();
    //! ```
}'
export DOC_RAWID='#[r#doc = "```\nstd::process::Command::new(\"sh\").status().unwrap();\n```"]
pub fn d() {}'
export DOC_RAWID_CONCAT='#[r#doc = concat!("```\n", "std::process::Command::new(\"sh\").status().unwrap();", "\n```")]
pub fn d() {}'
export DOC_RAWID_CFG='#[r#cfg_attr(all(), doc = "```\nstd::process::Command::new(\"sh\").status().unwrap();\n```")]
pub fn d() {}'
export DOC_NESTED_CFG='#[cfg_attr(all(), cfg_attr(all(), doc = "```\nstd::process::Command::new(\"sh\").status().unwrap();\n```"))]
pub fn d() {}'
export DOC_IN_MACRO='macro_rules! m { () => { #[doc = "```\nstd::process::Command::new(\"sh\").status().unwrap();\n```"] pub fn d() {} }; }
m!();'
export DOC_INNER_CONCAT='#![doc = concat!("```\n", "std::process::Command::new(\"sh\").status().unwrap();", "\n```")]
pub fn x() {}'
export DOC_INNER_INCL='#![doc = include_str!("../README.md")]
pub fn x() {}'
export DOC_MACRO_ATTR='macro_rules! documented { ($d:expr) => { #[doc = $d] pub fn f() {} }; }'
export DOC_CLASS='/// ```{.example}
/// std::process::Command::new("sh").status().unwrap();
/// ```
pub fn d() {}'
export DOC_HEADING='/// # Example
///     std::process::Command::new("sh").status().unwrap();
pub fn d() {}'
export DOC_AFTERFENCE='/// ```text
/// output
/// ```
///     std::process::Command::new("sh").status().unwrap();
pub fn d() {}'
export DOC_NUMLIST='/// 1. ```
///    std::process::Command::new("sh").status().unwrap();
///    ```
pub fn d() {}'
# Crate docs (//!) that end inside an open fence, then the first item's own docs: rustdoc reads
# them as two texts (the crate block runs to its end); read as one, the item's fences pair up
# the wrong way and its code lands outside every block.
export DOC_TWO_ITEMS='//! ```
//! assert!(true);
/// ```
/// std::process::Command::new("sh").status().unwrap();
/// ```
pub fn d() {}'
# A prose quote opens a string for a whole-text lexer; the code then hides `use` behind `//` in a
# string literal. Only a lexing that starts at the code line sees it.
export DOC_BLIND='/// He said "hi
/// ```
/// let s = "//"; use std::process::Command as C;
/// C::new("sh").status().unwrap();
/// ```
/// bye"
pub fn d() {}'
# Not code: #[doc(include = ...)] was removed from rustdoc (1.95 warns "unknown doc attribute").
export DOC_INCLUDE_KEY='#[doc(include = "x.md")]
pub fn e() {}'
if [ "$REAL_TOKENS" = 1 ]; then
    EXPECT="include" case_run "r4-d(real): #![doc = include_str!(...)] in a new module file" 1 : 'printf "%s\n" "$DOC_INNER_INCL" >src/inner.rs'
    EXPECT="process" case_run "r4-d(real): control, a fence on its own line inside a list item" 1 : 'printf "%s\n" "$DOC_LIST_OWN" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a fence on the list marker line (- \`\`\`)" 1 : 'printf "%s\n" "$DOC_LIST" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a fenced doctest inside a block quote" 1 : 'printf "%s\n" "$DOC_QUOTE" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a /** */ doctest with leading stars (per-line lex blinded by a raw string)" 1 : 'printf "%s\n" "$DOC_STAR" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): control, a /** */ doctest with leading stars (per-line lex sees it)" 1 : 'printf "%s\n" "$DOC_STAR_PLAIN" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a fence on a numbered list marker line (1. \`\`\`)" 1 : 'printf "%s\n" "$DOC_NUMLIST" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a fence whose info string is only a {.class}" 1 : 'printf "%s\n" "$DOC_CLASS" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): an indented code block right after a heading" 1 : 'printf "%s\n" "$DOC_HEADING" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): an indented code block right after a closing fence" 1 : 'printf "%s\n" "$DOC_AFTERFENCE" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a fence opened in outer docs and closed in inner docs" 1 : 'printf "%s\n" "$DOC_MERGE" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a fence opened on mod m; and closed in m.rs" 1 : \
        'printf "/// \`\`\`\npub mod m;\n" >>src/lib.rs; printf "//! std::process::Command::new(\"sh\").status().unwrap();\n//! \`\`\`\npub fn x() {}\n" >src/m.rs'
    EXPECT="process" case_run "r4-d(real): #[r#doc = \"...\"] doctest" 1 : 'printf "%s\n" "$DOC_RAWID" >>src/lib.rs'
    EXPECT="include" case_run "r4-d(real): #[r#doc = concat!(...)] computed doc value" 1 : 'printf "%s\n" "$DOC_RAWID_CONCAT" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): #[r#cfg_attr(all(), doc = \"...\")] doctest" 1 : 'printf "%s\n" "$DOC_RAWID_CFG" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): nested cfg_attr doc" 1 : 'printf "%s\n" "$DOC_NESTED_CFG" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): a literal doc attribute inside a macro_rules body" 1 : 'printf "%s\n" "$DOC_IN_MACRO" >>src/lib.rs'
    EXPECT="include" case_run "r4-d(real): #![doc = concat!(...)] inner attribute" 1 : 'printf "%s\n" "$DOC_INNER_CONCAT" >src/inner.rs'
    EXPECT="include" case_run "r4-d(real): #[doc = \$d] in a macro definition" 1 : 'printf "%s\n" "$DOC_MACRO_ATTR" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): crate docs that end inside a fence, then an item doctest" 1 : 'printf "%s\n" "$DOC_TWO_ITEMS" >src/two.rs'
    case_run "r4-d(real): control, #[doc(include = ...)] runs nothing" 0 : 'printf "%s\n" "$DOC_INCLUDE_KEY" >>src/lib.rs'
    # R4 fix: doc text is code whatever its Markdown and whatever state prose leaves a lexer in.
    EXPECT="process" case_run "r4-d(real): a prose quote, then code that puts // inside a string" 1 : 'printf "%s\n" "$DOC_BLIND" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): prose that names Command::new outside any fence is a finding" 1 : 'printf "/// Runs std::process::Command::new(\"sh\") for you.\npub fn d() {}\n" >>src/lib.rs'
    EXPECT="process" case_run "r4-d(real): turning a text fence into a Rust fence re-judges the unchanged line" 1 \
        'printf "/// \`\`\`text\n/// std::process::Command::new(\"sh\").status().unwrap();\n/// \`\`\`\npub fn d() {}\n" >>src/lib.rs' \
        'sed -i.bak "s|^/// \`\`\`text\$|/// \`\`\`|" src/lib.rs; rm src/lib.rs.bak'
    EXPECT="include" case_run "r4-d(real): docs on mod m; that end on a path separator" 1 : 'printf "/// use libc::\npub mod m;\n" >>src/lib.rs; : >src/m.rs'
    EXPECT="process" case_run "r4-d(real): a module file whose docs continue a string the parent opened" 1 : \
        'printf "//! \"; use libc::fork as f; \"\n//! f();\npub fn x() {}\n" >src/m.rs; printf "pub mod m;\n" >>src/lib.rs'
    case_run "r4-d(real): control, plain module docs and a URL" 0 : 'printf "/// See https://example.com (or /sys/class/*).\npub mod m;\n" >>src/lib.rs; printf "//! The m module.\npub fn x() {}\n" >src/m.rs'
fi
