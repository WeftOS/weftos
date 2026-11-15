# shellcheck shell=bash
# diff-gate-test-review3.sh: review round 3 fixtures (2026-11-15), sourced by diff-gate-test.sh
# (section 14). Uses that file's case_run / check_result / fixture_repo / appr helpers and
# counters, the review file's CR, section 9's SBASE / SENS / SUP / EVIL and section 13's
# wt_ignored / d1_row. KNOWN_GAP=<id> marks a case the gate misclassifies today; see
# reports/review-diff-gate-combined-r3-20261115.md.
# shellcheck disable=SC2016  # snippets are eval'd inside the fixture repo on purpose

echo "diff-gate review round 3 fixtures (2026-11-15)"
A=config/diff-gate-approvals.toml
S=config/diff-gate-sensitive.toml

# ── (a) Cargo.toml: keys pass 1 reads only on added lines ────────────
# build and proc-macro keys are checked per added line (pass 1). An edit that moves an
# unchanged key line into the table Cargo reads it from adds no such line.
export B_META='[package]
name = "fx"

[package.metadata.x]
build = "src/b.rs"

[dependencies]
existing = "1"'
KNOWN_GAP=R3-A1 EXPECT="build-script" case_run "r3-a: deleting a header moves an unchanged build key into [package]" 1 \
    'printf "%s\n" "$B_META" >Cargo.toml; echo "fn main() {}" >src/b.rs' \
    'grep -v "^\[package.metadata.x\]" Cargo.toml >c.tmp && mv c.tmp Cargo.toml'
KNOWN_GAP=R3-A1 EXPECT="build-script" case_run "r3-a: renaming [package.metadata] to [package] exposes an unchanged build key" 1 \
    'printf "[package.metadata]\nname = \"fx\"\nbuild = \"src/b.rs\"\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml; echo "fn main() {}" >src/b.rs' \
    'sed -i.bak "s/^\[package.metadata\]/[package]/" Cargo.toml; rm Cargo.toml.bak'
KNOWN_GAP=R3-A1 EXPECT="build-script" case_run "r3-a: un-wrapping a multi-line string exposes an unchanged build key" 1 \
    'printf "[package]\nname = \"fx\"\ndescription = \"\"\"\nbuild = \"src/b.rs\"\n\"\"\"\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml; echo "fn main() {}" >src/b.rs' \
    'sed -i.bak -e "s/^description = \"\"\"\$/description = \"x\"/" -e "s/^\"\"\"\$/# end/" Cargo.toml; rm Cargo.toml.bak'
KNOWN_GAP=R3-A1 EXPECT="proc-macro" case_run "r3-a: deleting a header moves an unchanged proc-macro key into [lib]" 1 \
    'printf "[package]\nname = \"fx\"\n\n[lib]\npath = \"src/lib.rs\"\n\n[package.metadata.lib]\nproc-macro = true\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml' \
    'grep -v "^\[package.metadata.lib\]" Cargo.toml >c.tmp && mv c.tmp Cargo.toml'
KNOWN_GAP=R3-A1 EXPECT="build-script" case_run "r3-a: deleting a header moves an unchanged escaped key into [package]" 1 \
    'printf "[package]\nname = \"fx\"\n\n[package.metadata.x]\n\"bui\\\\u006cd\" = \"src/b.rs\"\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml; echo "fn main() {}" >src/b.rs' \
    'grep -v "^\[package.metadata.x\]" Cargo.toml >c.tmp && mv c.tmp Cargo.toml'
# Removing `build = false` turns on an existing build.rs (Cargo's default). No line is added.
KNOWN_GAP=R3-A2 EXPECT="build-script" case_run "r3-a: deleting build = false activates an existing build.rs" 1 \
    'printf "[package]\nname = \"fx\"\nbuild = false\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml; echo "fn main() {}" >build.rs' \
    'grep -v "^build = false" Cargo.toml >c.tmp && mv c.tmp Cargo.toml'
KNOWN_GAP=R3-A2 EXPECT="build-script" case_run "r3-a: a manifest added next to an existing build.rs runs it" 1 \
    'mkdir -p crates/x/src && echo "fn main() {}" >crates/x/build.rs && echo "pub fn a() {}" >crates/x/src/lib.rs' \
    'printf "[package]\nname = \"x\"\nversion = \"0.1.0\"\n" >crates/x/Cargo.toml'
# lib.crate-type = ["proc-macro"] makes the library a proc-macro crate, like proc-macro = true.
KNOWN_GAP=R3-A3 EXPECT="proc-macro" case_run "r3-a: crate-type = [\"proc-macro\"]" 1 : \
    'printf "\n[lib]\ncrate-type = [\"proc-macro\"]\n" >>Cargo.toml'
case_run "r3-a: control, build = true is still a finding" 1 : \
    'sed -i.bak "s/^name = \"fx\"/name = \"fx\"\nbuild = true/" Cargo.toml; rm Cargo.toml.bak'

# ── (a) Cargo.toml: the TOML reader (pass 3) ─────────────────────────
export BOM=$'\357\273\277'
# A UTF-8 BOM before the first header: the reader does not see the header, so later keys land in
# the previous table (the top level). A new manifest is cargo-unparsed (fail closed), but once
# the BOM line is on the base, new entries under it are read as top-level keys.
KNOWN_GAP=R3-A4 case_run "r3-a: a BOM before [dependencies] on the base hides a new entry" 1 \
    'printf "%s[dependencies]\nexisting = \"1\"\n\n[package]\nname = \"fx\"\n" "$BOM" >Cargo.toml' \
    'printf "%s[dependencies]\nexisting = \"1\"\nevil = { git = \"https://evil\" }\n\n[package]\nname = \"fx\"\n" "$BOM" >Cargo.toml'
EXPECT="cargo-unparsed" case_run "r3-a: control, a new manifest with a BOM header is cargo-unparsed" 1 : \
    'printf "%s[dependencies]\nevil = { git = \"https://evil\" }\n" "$BOM" >Cargo.toml'
EXPECT="new-dep build-script" case_run "r3-a: escaped hyphen in a dependency name" 1 : 'printf "\"evil\\\\u002Dx\" = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r3-a: unicode dependency name" 1 : 'printf "\"\303\251vil\" = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r3-a: comments inside a multi-line inline table" 1 : \
    'sed -i.bak "s|^existing = \"1\"|existing = { # pinned\n    version = \"1\", # x\n    git = \"https://evil\", # y\n}|" Cargo.toml; rm Cargo.toml.bak'
EXPECT="new-dep" case_run "r3-a: nested inline tables under [target.'cfg(unix)']" 1 : \
    'printf "\n[target.\x27cfg(unix)\x27]\ndependencies = { evil = { git = \"https://evil\", branch = \"x\" } }\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r3-a: multi-line literal string git source with an escape-looking body" 1 : \
    'printf "[package]\nname = \"fx\"\n\n[dependencies]\nexisting = { version = \"1\", git = \x27\x27\x27https://evil\\\\x\x27\x27\x27 }\n" >Cargo.toml'
EXPECT="new-dep" case_run "r3-a: a second [dependencies] table with a new entry" 1 : \
    'printf "\n[features]\nx = []\n\n[dependencies]\nevil = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r3-a: [target.'cfg(..)'.build-dependencies]" 1 : \
    'printf "\n[target.\x27cfg(target_os = \"linux\")\x27.build-dependencies]\ncc = \"1\"\n" >>Cargo.toml'
EXPECT="new-dep" case_run "r3-a: top-level dotted workspace.dependencies key" 1 : \
    '{ printf "workspace.dependencies.evil = { git = \"https://evil\" }\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
EXPECT="new-dep" case_run "r3-a: [patch.crates-io.name] table with a path source" 1 : \
    'printf "\n[patch.crates-io.existing]\npath = \"../evil\"\n" >>Cargo.toml'
EXPECT="include" case_run "r3-a: [[bin]] path to a non-.rs file" 1 : 'printf "\n[[bin]]\nname = \"x\"\npath = \"src/x.txt\"\n" >>Cargo.toml'
EXPECT="include" case_run "r3-a: [[test]] path to a non-.rs file" 1 : 'printf "\n[[test]]\nname = \"x\"\npath = \"tests/x.txt\"\n" >>Cargo.toml'
EXPECT="include" case_run "r3-a: [[example]] path to a non-.rs file" 1 : 'printf "\n[[example]]\nname = \"x\"\npath = \"examples/x.md\"\n" >>Cargo.toml'
EXPECT="include" case_run "r3-a: bin as an array of inline tables with a non-.rs path" 1 : \
    '{ printf "bin = [ { name = \"x\", path = \"src/x.txt\" } ]\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
EXPECT="build-script" case_run "r3-a: build key inside a multi-line package inline table" 1 \
    'printf "package = {\n    name = \"fx\",\n    version = \"0.1.0\",\n}\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml' \
    'printf "package = {\n    name = \"fx\",\n    version = \"0.1.0\",\n    build = \"b.rs\",\n}\n\n[dependencies]\nexisting = \"1\"\n" >Cargo.toml'
EXPECT="proc-macro" case_run "r3-a: quoted proc_macro key in a lib inline table" 1 : \
    '{ printf "lib = { \"proc_macro\" = true }\n"; cat Cargo.toml; } >c.tmp && mv c.tmp Cargo.toml'
case_run "r3-a: control, links is not a rule" 0 : 'sed -i.bak "s/^name = \"fx\"/name = \"fx\"\nlinks = \"foo\"/" Cargo.toml; rm Cargo.toml.bak'
case_run "r3-a: control, dependency names under package.metadata stay data" 0 : \
    'printf "\n[package.metadata.deb]\ndependencies.evil = { git = \"https://evil\" }\n" >>Cargo.toml'
# .cargo/config.toml and rust-toolchain: any change is toolchain-config, at any depth.
EXPECT="toolchain-config" case_run "r3-a: .cargo/config.toml source replacement" 1 : \
    'mkdir -p .cargo && printf "[source.crates-io]\nreplace-with = \"evil\"\n\n[source.evil]\nregistry = \"https://evil\"\n" >.cargo/config.toml'
EXPECT="toolchain-config" case_run "r3-a: nested .cargo/config.toml [patch] and linker" 1 : \
    'mkdir -p crates/x/.cargo && printf "[patch.crates-io]\nexisting = { path = \"../evil\" }\n\n[target.x86_64-unknown-linux-gnu]\nlinker = \"/tmp/x\"\n" >crates/x/.cargo/config.toml'
EXPECT="toolchain-config" case_run "r3-a: .cargo/config without extension (rustflags)" 1 : \
    'mkdir -p .cargo && printf "[build]\nrustflags = [\"-Clink-arg=-Wl,-rpath,/tmp\"]\n" >.cargo/config'
EXPECT="toolchain-config" case_run "r3-a: upper-case .Cargo/Config.toml (case-insensitive disks)" 1 : \
    'mkdir -p .Cargo && printf "[build]\nrustc-wrapper = \"/tmp/x\"\n" >.Cargo/Config.toml'
EXPECT="toolchain-config" case_run "r3-a: legacy rust-toolchain file in a sub-crate" 1 : \
    'mkdir -p crates/x && echo "nightly-2026-01-01" >crates/x/rust-toolchain'
# Limit (spec: the gate never follows a link): an approved symlinked Cargo.toml reads its
# target, and edits to a target not named Cargo.toml are not read. Pinned at exit 0.
case_run "r3-a: limit, an edit to the target of a symlinked Cargo.toml is not read" 0 \
    'mkdir -p crates/x/src tmpl && echo "pub fn a() {}" >crates/x/src/lib.rs && printf "[package]\nname = \"x\"\n\n[dependencies]\n" >tmpl/x.toml && ln -s ../../tmpl/x.toml crates/x/Cargo.toml' \
    'printf "evil = { git = \"https://evil\" }\n" >>tmpl/x.toml'

# ── (b) sensitive crates: what counts as a reference to an exempt file ──
# KB: crate k with lib.rs declaring supervise; supervise_tests.rs exists (exempt by name).
KB="$SBASE"'; printf "pub mod supervise;\n" >crates/k/src/lib.rs; echo "fn t() {}" >crates/k/src/supervise_tests.rs'
EVIL_T='printf "%s\n" "$EVIL" >>crates/k/src/supervise_tests.rs'
KNOWN_GAP=R3-B1 EXPECT="sensitive-crate" case_run "r3-b: an include! over several lines (rustfmt's layout for a long path) is a reference" 1 \
    "$KB"'; printf "include!(\n    \"supervise_tests.rs\"\n);\n" >>crates/k/src/supervise.rs' "$EVIL_T"
KNOWN_GAP=R3-B1 EXPECT="sensitive-crate" case_run "r3-b: a mod declaration split after the mod keyword is a reference" 1 \
    "$KB"'; printf "pub(crate) mod\n    supervise_tests;\n" >>crates/k/src/lib.rs' "$EVIL_T"
KNOWN_GAP=R3-B1 EXPECT="sensitive-crate" case_run "r3-b: deleting #[cfg(test)] above a split mod declaration" 1 \
    "$KB"'; printf "#[cfg(test)]\npub(crate) mod\n    supervise_tests;\n" >>crates/k/src/lib.rs' \
    'grep -v "^#\[cfg(test)\]\$" crates/k/src/lib.rs >l.tmp && mv l.tmp crates/k/src/lib.rs; '"$EVIL_T"
KNOWN_GAP=R3-B2 EXPECT="sensitive-crate" case_run "r3-b: a declaring file with a NUL byte (git grep -I skips it)" 1 \
    "$KB"'; printf "// \000\n#[path = \"supervise_tests.rs\"]\nmod helpers;\n" >>crates/k/src/lib.rs' "$EVIL_T"
EXPECT="sensitive-crate" case_run "r3-b: control, the same declaration without the NUL byte" 1 \
    "$KB"'; printf "// x\n#[path = \"supervise_tests.rs\"]\nmod helpers;\n" >>crates/k/src/lib.rs' "$EVIL_T"
KNOWN_GAP=R3-B3 EXPECT="sensitive-crate" case_run "r3-b: a #[path] that differs in case (a case-insensitive disk builds it)" 1 \
    "$KB"'; printf "#[path = \"Supervise_Tests.rs\"]\nmod helpers;\n" >>crates/k/src/lib.rs' "$EVIL_T"
KNOWN_GAP=R3-B4 EXPECT="sensitive-crate" case_run "r3-b: a symlinked module file that points at an exempt file" 1 \
    "$KB"'; ln -s supervise_tests.rs crates/k/src/real.rs; printf "pub mod real;\n" >>crates/k/src/lib.rs' "$EVIL_T"
EXPECT="sensitive-crate" case_run "r3-b: a mod declaration in a multi-line macro_rules body" 1 \
    "$KB"'; printf "macro_rules! m {\n    () => {\n        mod supervise_tests;\n    };\n}\nm!();\n" >>crates/k/src/lib.rs' "$EVIL_T"
EXPECT="sensitive-crate" case_run "r3-b: a #[path] that climbs with .. from another directory" 1 \
    "$KB"'; mkdir -p crates/k/src/a; printf "#[path = \"../supervise_tests.rs\"]\npub mod x;\n" >crates/k/src/a/mod.rs; printf "pub mod a;\n" >>crates/k/src/lib.rs' "$EVIL_T"
EXPECT="sensitive-crate" case_run "r3-b: a crate tests/ file pulled into src by #[path]" 1 \
    "$SBASE"'; printf "#[path = \"../tests/common/mod.rs\"]\npub mod common;\n" >crates/k/src/lib.rs; mkdir -p crates/k/tests/common; echo "pub fn c() {}" >crates/k/tests/common/mod.rs' \
    'printf "%s\n" "$EVIL" >>crates/k/tests/common/mod.rs'
EXPECT="sensitive-crate" case_run "r3-b: include! through std:: of an exempt file" 1 \
    "$KB"'; printf "std::include!(\"supervise_tests.rs\");\n" >>crates/k/src/supervise.rs' "$EVIL_T"
EXPECT="sensitive-crate" case_run "r3-b: a non-test module renamed into tests/ keeps its declaration" 1 \
    "$SBASE"'; printf "pub mod util;\n" >crates/k/src/lib.rs; echo "pub fn u() {}" >crates/k/src/util.rs' \
    'mkdir -p crates/k/src/tests && git mv crates/k/src/util.rs crates/k/src/tests/util.rs && printf "#[path = \"tests/util.rs\"]\npub mod util;\n" >crates/k/src/lib.rs && printf "%s\n" "$EVIL" >>crates/k/src/tests/util.rs'
EXPECT="sensitive-crate" case_run "r3-b: cfg_attr(not(test), path) builds the exempt file in normal builds" 1 \
    "$KB"'; printf "#[cfg_attr(not(test), path = \"supervise_tests.rs\")]\nmod helpers;\n" >>crates/k/src/lib.rs' "$EVIL_T"
case_run "r3-b: control, an integration test under tests/ that nothing refers to stays exempt" 0 \
    "$SBASE"'; mkdir -p crates/k/tests; echo "fn t() {}" >crates/k/tests/it.rs' 'printf "%s\n" "$EVIL" >>crates/k/tests/it.rs'

# ── (c) trust inputs ─────────────────────────────────────────────────
case_run "r3-c: the sensitive list turned into a symlink fails closed" 2 \
    "$SBASE" 'rm '$S' && printf "%s\n" "$SENS" >config/other.toml && ln -s other.toml '$S
EXPECT="approvals-change" case_run "r3-c: emptying the sensitive list is owner-only" 1 "$SBASE" ': >'$S'; printf "%s\n" "$CALLER" >>crates/k/src/supervise.rs'
EXPECT="sensitive-crate" case_run "r3-c: emptying the list does not exempt the same diff's code" 1 "$SBASE" \
    ': >'$S'; printf "%s\n" "$CALLER" >>crates/k/src/supervise.rs' DIFF_GATE_OWNER_APPROVED=1
EXPECT="approvals-change" case_run "r3-c: deleting the ledger is owner-only" 1 \
    'printf "[[entry]]\ndecision = \"d\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n" "$(printf %064d 0)" >config/diff-gate-owner-ledger.toml' \
    'rm config/diff-gate-owner-ledger.toml'
# A HOME or XDG_CACHE_HOME inside the checkout puts the default cache there: refused.
d="$WORK/r3_c_home" && fixture_repo "$d" && mkdir -p tools/diff-gate-tokens && echo x >tools/diff-gate-tokens/README.md
git add -A && git commit -qm base && base="$(git rev-parse HEAD)" && echo "fn z() {}" >>src/lib.rs && git add -A && git commit -qm change
out="$(env -u DIFF_GATE_CACHE -u RUNNER_TEMP -u XDG_CACHE_HOME DIFF_GATE_BASE="$base" DIFF_GATE_TOKENS_BIN="" HOME="$d" PATH="/usr/bin:/bin" "$GATE" 2>&1)"; rc=$?
EXPECT="rust-tokens" GREP="inside the checkout" check_result "r3-c: a HOME inside the checkout is refused as the cache" 1 "$rc" "$out"
out="$(env -u DIFF_GATE_CACHE -u RUNNER_TEMP DIFF_GATE_BASE="$base" DIFF_GATE_TOKENS_BIN="" XDG_CACHE_HOME=target/xdg PATH="/usr/bin:/bin" "$GATE" 2>&1)"; rc=$?
EXPECT="rust-tokens" GREP="inside the checkout" check_result "r3-c: a relative XDG_CACHE_HOME is refused as the cache" 1 "$rc" "$out"
cd "$ROOT" || exit 2
# Limit: approvals are read from the head commit, so a row the owner removed on the base tip
# still approves findings on a branch forked before the removal; after the merge the code is
# in and the row is gone. Pinned at exit 0 (spec: rows come from --head).
d="$WORK/r3_c_revoked" && fixture_repo "$d" && appr unsafe 'src/**' "" >$A && git add -A && git commit -qm base && fork="$(git rev-parse HEAD)"
: >$A && git commit -qam "owner removes the row" && tip="$(git rev-parse HEAD)"
git checkout -q "$fork" && echo "fn f() { unsafe { g() } }" >>src/lib.rs && git commit -qam change
out="$(DIFF_GATE_BASE="$tip" "$GATE" 2>&1)"; rc=$?
GREP="approved unsafe" check_result "r3-c: limit, a row removed on the base tip still approves an older branch" 0 "$rc" "$out"
cd "$ROOT" || exit 2

# ── (d) approval rows ────────────────────────────────────────────────
# RV-D1 asks each pattern about single characters. A text-matched rule's finding always holds a
# space or a letter, so a pattern of one common character, or a class that leaves out one probe
# character, approves every finding of the rule in its path and is not named unscoped.
for p in '" "' '"[^a]"' '"[^<]"' '"e"'; do
    KNOWN_GAP=R3-D1 GREP="unscoped approval row" case_run "r3-d: new-dep row with pattern $p and no blob is unscoped" 0 \
        "d1_row \"\$(printf %s '$p')\" new-dep Cargo.toml >$A" 'printf "evil = { git = \"https://evil\" }\n" >>Cargo.toml'
done
KNOWN_GAP=R3-D1 GREP="unscoped approval row" case_run "r3-d: sensitive-crate row with pattern \" \" and no blob is unscoped" 0 \
    "$SBASE"'; d1_row "\" \"" sensitive-crate "crates/k/**" >'$A 'printf "%s\n" "$CALLER" >>crates/k/src/supervise.rs'
case_run "r3-d: control, a negated class approves no unsafe token (the ^ rule)" 1 \
    'd1_row "\"[^<]\"" >'$A 'echo "fn f() { unsafe { g() } }" >>src/lib.rs'
EXPECT="new-dep" case_run "r3-d: blob row for one manifest version does not approve another" 1 \
    'appr new-dep Cargo.toml "$(printf "[package]\nname = \"fx\"\n\n[dependencies]\nexisting = \"1\"\nok = \"1\"\n" | git hash-object --stdin)" >'$A \
    'printf "evil = \"1\"\n" >>Cargo.toml'
case_run "r3-d: blob row approves exactly its manifest version" 0 \
    'appr new-dep Cargo.toml "$(printf "[package]\nname = \"fx\"\n\n[dependencies]\nexisting = \"1\"\nok = \"1\"\n" | git hash-object --stdin)" >'$A \
    'printf "ok = \"1\"\n" >>Cargo.toml'
EXPECT="approvals-change" case_run "r3-d: ledger: an entry for a row does not approve it with a # moved into the reason" 1 \
    "printf '[[entry]]\ndecision = \"d\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n' \"\$(printf 'rule=unsafe\npath=src/**\npattern=*\nreason=a\napproved_by=owner\n' | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1)\" >config/diff-gate-owner-ledger.toml" \
    'printf "[[approval]]\nrule = \"unsafe\"\npath = \"src/**\"\npattern = \"*\"\nreason = \"a # b\"\napproved_by = \"owner\"\n" >'$A
case_run "r3-d: ledger: the same entry approves the row with a trailing comment" 0 \
    "printf '[[entry]]\ndecision = \"d\"\ndate = \"2026-11-15\"\nsha256 = \"%s\"\n' \"\$(printf 'rule=unsafe\npath=src/**\npattern=*\nreason=a\napproved_by=owner\n' | { sha256sum 2>/dev/null || shasum -a 256; } | cut -d' ' -f1)\" >config/diff-gate-owner-ledger.toml" \
    'printf "[[approval]]\nrule = \"unsafe\"\npath = \"src/**\"\npattern = \"*\"\nreason = \"a\" # b\napproved_by = \"owner\"\n" >'$A

# ── (e) robustness ───────────────────────────────────────────────────
EXPECT="unsafe" case_run "r3-e: a backslash in a .rs file name" 1 : 'echo "fn f() { unsafe { g() } }" >"src/a\\b.rs"'
EXPECT="unsafe" case_run "r3-e: glob characters in a .rs file name" 1 : 'echo "fn f() { unsafe { g() } }" >"src/[a]*?.rs"'
EXPECT="unsafe" case_run "r3-e: a .rs file name starting with a colon" 1 : 'echo "fn f() { unsafe { g() } }" >"src/:x.rs"'
EXPECT="unsafe" case_run "r3-e: a 200 KB line with unsafe at its end" 1 : \
    '{ printf "fn f() { "; yes "g();" | head -40000 | tr -d "\n"; printf " unsafe { g() } }\n"; } >src/long.rs'
EXPECT="unsafe" case_run "r3-e: unsafe after a 100 KB string on the same line" 1 : \
    '{ printf "fn f() { let _s = \""; yes a | head -100000 | tr -d "\n"; printf "\"; unsafe { g() } }\n"; } >src/longs.rs'
# No EXPECT: the 100 KB report line is longer than a pipe buffer, and check_result's
# `echo | grep -q` then fails on SIGPIPE under pipefail. The only change is the new entry.
case_run "r3-e: a 100 KB inline table with git at its end" 1 : \
    '{ printf "evil = { features = ["; yes "\"f\"," | head -20000 | tr -d "\n"; printf "], git = \"https://evil\" }\n"; } >>Cargo.toml'
EXPECT="unsafe" case_run "r3-e: invalid UTF-8 in a comment does not hide the next line" 1 : \
    'printf "// \377\376\nfn f() { unsafe { g() } }\n" >>src/lib.rs'
# A file name that is not valid UTF-8 (refused by APFS, accepted by ext4).
d="$WORK/r3_e_badname" && fixture_repo "$d" && git add -A && git commit -qm base && base="$(git rev-parse HEAD)"
if echo "fn f() { unsafe { g() } }" 2>/dev/null >"src/$(printf '\377')x.rs"; then
    git add -A && git commit -qm change
    out="$(DIFF_GATE_BASE="$base" "$GATE" 2>&1)"; rc=$?
    EXPECT="unsafe" check_result "r3-e: a .rs file name that is not valid UTF-8" 1 "$rc" "$out"
else
    echo "  skip  r3-e: a .rs file name that is not valid UTF-8 (this file system refuses it)"
fi
cd "$ROOT" || exit 2

# ── (f) Rust spellings ───────────────────────────────────────────────
# nix 0.30+ has nix::spawn::posix_spawn(p), safe functions behind the `process` feature. The
# spec lists "nix exec, fork, spawn and system"; only nix::unistd fork/exec are matched.
KNOWN_GAP=R3-F2 EXPECT="process" case_run "r3-f: nix::spawn::posix_spawnp" 1 : \
    'echo "fn f() { let _ = nix::spawn::posix_spawnp(&a, &b, &c, &d, &e); }" >>src/lib.rs'
EXPECT="process" case_run "r3-f: async_std::process::Command path" 1 : 'echo "fn f() { async_std::process::Command::new(\"sh\"); }" >>src/lib.rs'
EXPECT="net" case_run "r3-f: std::net::ToSocketAddrs (DNS lookup)" 1 : 'echo "use std::net::ToSocketAddrs;" >>src/lib.rs'
EXPECT="unsafe ffi" case_run "r3-f: 2024 unsafe extern block" 1 : 'echo "unsafe extern \"C\" { safe fn abs(x: i32) -> i32; }" >>src/lib.rs'
EXPECT="ffi" case_run "r3-f: link_section inside an unsafe attribute" 1 : 'echo "#[unsafe(link_section = \".init_array\")] static S: u8 = 0;" >>src/lib.rs'
EXPECT="ffi" case_run "r3-f: export_name inside cfg_attr" 1 : 'echo "#[cfg_attr(unix, export_name = \"q\")] pub fn q() {}" >>src/lib.rs'
EXPECT="include" case_run "r3-f: include! of a computed path" 1 : 'echo "include!(concat!(env!(\"OUT_DIR\"), \"/x.rs\"));" >>src/lib.rs'
export DOC_CONCAT='#[doc = concat!("```\n", "std::process::Command::new(\"sh\").status().unwrap();", "\n```")]
pub fn d() {}'
export DOC_CFG_ALL='#[cfg_attr(all(), doc = "```
std::process::Command::new(\"sh\").status().unwrap();
```")]
pub fn d() {}'
export DOC_CFG_DOC='#[cfg_attr(doc, doc = "```
std::process::Command::new(\"sh\").status().unwrap();
```")]
pub fn d() {}'
export DOC_RAW='#[doc = r#"
```
std::process::Command::new("sh").status().unwrap();
```
"#]
pub fn d() {}'
if [ "$REAL_TOKENS" = 1 ]; then
    # Doctests: rustdoc expands any attribute that sets doc, not only a literal #[doc = "..."].
    KNOWN_GAP=R3-F1 EXPECT="process" case_run "r3-f(real): doctest built with #[doc = concat!(...)]" 1 : 'printf "%s\n" "$DOC_CONCAT" >>src/lib.rs'
    KNOWN_GAP=R3-F1 EXPECT="process" case_run "r3-f(real): doctest in #[cfg_attr(all(), doc = \"...\")]" 1 : 'printf "%s\n" "$DOC_CFG_ALL" >>src/lib.rs'
    KNOWN_GAP=R3-F1 EXPECT="process" case_run "r3-f(real): doctest in #[cfg_attr(doc, doc = \"...\")]" 1 : 'printf "%s\n" "$DOC_CFG_DOC" >>src/lib.rs'
    EXPECT="process" case_run "r3-f(real): doctest in a raw-string doc attribute" 1 : 'printf "%s\n" "$DOC_RAW" >>src/lib.rs'
    EXPECT="process" case_run "r3-f(real): an existing async_std alias, a new call" 1 \
        'printf "use async_std::process::Command as C;\nfn g() {}\n" >src/a.rs' 'printf "use async_std::process::Command as C;\nfn g() {}\nfn f() { C::new(\"sh\"); }\n" >src/a.rs'
    EXPECT="process" case_run "r3-f(real): an existing smol glob import, a new Command call" 1 \
        'printf "use smol::process::*;\nfn g() {}\n" >src/a.rs' 'printf "use smol::process::*;\nfn g() {}\nfn f() { Command::new(\"sh\"); }\n" >src/a.rs'
    EXPECT="net" case_run "r3-f(real): extern crate std as core alias to net" 1 : \
        'printf "extern crate std as k;\nfn f() { let _ = k::net::TcpStream::connect(\"x:1\"); }\n" >src/k.rs'
fi
