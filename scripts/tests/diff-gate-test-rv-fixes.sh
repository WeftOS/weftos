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
