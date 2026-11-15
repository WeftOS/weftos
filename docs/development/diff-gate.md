# Pre-build diff gate

`scripts/diff-gate.sh` rejects risky changes before any crate compiles. It looks only at what a
diff adds (added lines and new files); existing code never fails it. It needs git, grep and awk,
plus a small Rust helper for `.rs` files (`tools/diff-gate-tokens`, built from the trusted base;
see [The token helper](#the-token-helper)).

The idea of a static, pre-build gate over the diff comes from
[ruvnet/rune](https://github.com/ruvnet/rune) (MIT). This is an independent implementation. See
[the Rune evaluation](../research/rune-evaluation-2026-10-29.md).

## Where it runs

- `scripts/build.sh diff-gate` runs it alone. Extra arguments pass through: `--json`,
  `--worktree`, `--base REF`.
- `scripts/build.sh gate` runs it as check 0, with `--worktree` so uncommitted work counts. A
  rejected diff stops the gate before any compile step.
- CI: `.github/workflows/diff-gate.yml` is a reusable workflow. `pr-gates.yml`, `wasm-build.yml`,
  `wasm-browser.yml`, `benchmarks.yml` and `mcp-server-smoke.yml` call it as their first job, and
  every job that builds, tests, lints or measures crates has `needs: diff-gate`. CI never runs
  the gate script from the commit under review; see [CI: which gate runs](#ci-which-gate-runs).

The base is `DIFF_GATE_BASE` if set, otherwise the newest `origin/<AGF_TARGET_PATTERN>` from
`config/agent-git.env` (today `origin/target-0.8.*`), otherwise `origin/master`. The merge-base
with HEAD is used. CI always sets it to the trusted commit described below.

## CI: which gate runs

The gate step in `diff-gate.yml` picks one trusted commit already on a protected branch, takes
`scripts/diff-gate.sh` from it into `$RUNNER_TEMP`, and passes the same commit as the base. The
gate reads the base-side approvals rows and the owner ledger from git at that base, and nothing
from the checkout once a base is set, so all three come from protected history. Protected
branches are the repository default branch, `master` and `target-*`. "Newest protected tip"
means the version-sorted newest `origin/target-*`, else the default branch, then `master`,
skipping the branch being pushed.

`main` is not protected by name. It does not exist in this repository, and a branch name that
anyone with push rights can create first must not become a trust root: before 2026-11-15 a
self-made `main` was judged once against another protected tip and from then on by its own
gate, and a PR into it trusted its tip. It counts as protected only if it becomes the
repository default branch.

| Event | Trusted commit (gate copy and base) |
|---|---|
| `pull_request` into a protected branch | the PR base SHA |
| `pull_request` into any other branch (or with no base branch name) | the newest protected tip. The base branch never passed review, so its gate, rows and ledger are not trusted; `wasm-browser.yml` runs on PRs into any branch |
| push to a protected branch | its previous tip (`before`) |
| push to a protected branch, `before` not an ancestor (force-push) | `before`; the gate diffs from the merge-base, which is protected history, so every rewritten commit is judged |
| force-push whose `before` is not in the clone | none: fail closed (exit 2). Land it by PR or owner review |
| new protected branch (`before` = `000…`) | the newest *other* protected tip; a new branch never trusts itself |
| push to any other branch, new or not | the newest protected tip |
| `workflow_dispatch` and other events | the newest protected tip |

If the trusted commit predates the gate (for example the first landing on `master`), the gate
copy comes from the newest other protected tip instead; the base does not change. If no
protected branch has the gate, the step fails closed.

Why an unprotected branch is not judged from `before`: its earlier pushes never passed review.
With `before` as the trust root, a first push could weaken the gate, or add a ledger entry or
an approvals row (red, but landed on the branch), and a second push would then be judged by
that copy and those rows. Judging the branch against the protected tip is what a PR run does,
and every push to it reports the branch's full set of findings until they are fixed.

The selection logic is inline in the workflow, not in a helper script: a helper would have to be
fetched from the trusted commit it is meant to choose, and the workflow file is trusted by
GitHub anyway (see Known limits). `scripts/tests/diff-gate-ci-test.sh` extracts that `run:`
block from the workflow and runs it unchanged against fixture pushes.

## Rules

| Rule | Fails when the diff adds |
|---|---|
| `unsafe` | the `unsafe` keyword in a `.rs` file (any case of the extension), outside comments, strings and char literals, including in a doctest; also an added line inside an existing `unsafe { }` block, `unsafe fn` or `unsafe impl` body. See [How .rs files are read](#how-rs-files-are-read) |
| `new-dep` | a new entry in any Cargo.toml dependency table, in `[patch.*]` or in `[replace]`, or a changed source on an existing entry (`git`, `path`, `registry`, `registry-index`, `package`, `rev`, `tag`, `branch`). Every spelling Cargo resolves counts the same: a table header, a dotted key (`dependencies.evil = ...`, `[target.'cfg(unix)']` then `dependencies.evil`, `[workspace]` then `dependencies.evil`, `[patch]` then `crates-io.evil`), an inline table (`dependencies = { evil = ... }`), quoted and escaped key segments, and any line ending. `workspace = true` entries for a dependency the base workspace already declares are not new. See [How Cargo.toml is read](#how-cargotoml-is-read) |
| `cargo-unparsed` | a Cargo.toml statement the gate's reader cannot read, or a key path that names a dependency table (a segment containing `dependencies`, or `patch`, `replace` or `target`) in a form it does not know, such as `[target.x.y.dependencies]` or `patch = "x"`. Fail closed: such a statement is never an OK. Statements the base already had are not reported again; an approval row clears it |
| `build-script` | a new `build.rs` (also one created by a rename), or a `build` key in Cargo.toml in any form: bare, quoted, dotted (`package.build`) or inside an inline table (`package = { build = ... }`); `build = false` is fine. A quoted Cargo.toml key or table name with a backslash escape is reported here too, since it can spell `build` or `proc-macro` |
| `proc-macro` | `proc-macro` / `proc_macro` `= true` in any of the same key forms (`lib = { proc-macro = true }`, `lib.proc-macro`) |
| `net` | `std::net`, `tokio::net`, `async_std::net`, `mio::net` paths, `TcpStream`, `TcpListener`, `TcpSocket`, `UdpSocket`, `reqwest`, `ureq`, `hyper`, `isahc`, `surf`, `attohttpc`, `curl`, `socket2`, `tungstenite`, `quinn` paths or imports, `net` in a `std::{...}` import, `libc` socket calls. Address types (`SocketAddr`, `IpAddr` and friends) do no I/O and are ignored. Unix-domain sockets (`std::os::unix::net`) are local IPC, used by the daemon in about 75 files, and are not findings |
| `process` | `process::Command` in any form (including `std::{process::Command as K}` and multi-line `std::process::{` imports), the `std::process` module imported or aliased, `Command::new`, `tokio::process`, `CommandExt`, `libc` / `nix` exec, fork, spawn and `system`. Paths are resolved through the file's `use` aliases and glob imports, so `use std::process as p; p::Command` and `use std::process::*; use Command as C; C::args` count. `std::process::exit`, `abort`, `id`, `ExitCode`, `ExitStatus`, `Stdio`, `Output` and the `Child` types do not spawn and are ignored |
| `ffi` | the `extern` keyword in any form except `extern crate` (`extern "C"`, `extern "system"`, bare `extern {` / `extern fn`), `#[no_mangle]`, `export_name`, `link_section`, and `global_asm!` / `asm!` / `naked_asm!` (`global_asm!` needs no `unsafe`). An added declaration inside an existing `extern { }` block counts. `#[link(...)]` and `link_name` are not separate findings: they only act on an `extern` block, which is one |
| `include` | code pulled into the build from something that is not a `.rs` file: `include!` whose argument is not a literal `.rs` path, `#[path = "..."]` to a non-`.rs` file, and a new Cargo target `path` (`[lib]`, `[[bin]]`, `[[example]]`, `[[test]]`, `[[bench]]`) to a non-`.rs` file. `#[path]` counts inside `cfg_attr`, over several lines. `include_str!` and `include_bytes!` are not findings in code: they produce a `&str` / `&[u8]` value, and turning data into code needs `unsafe`, which is its own finding. Inside an attribute they are (`#![doc = include_str!("README.md")]` turns that file's code blocks into doctests that `cargo test` runs). `paste!` and `concat_idents!` are findings too: they can build any identifier or path from pieces |
| `sensitive-crate` | any added non-blank `.rs` line in non-test code under a crate listed in `config/diff-gate-sensitive.toml` (a file exempt by name counts as test code only if nothing builds it outside a test cfg), a test cfg removed from a module declaration that stays, and in such a crate's Cargo.toml a new or changed target `path`. Interim posture (G1, option c); it runs alongside the token helper until the owner retires it; see [Sensitive crates](#sensitive-crates-interim) |
| `ci-workflow` | any change under `.github/workflows/`, to `scripts/diff-gate.sh` or under `tools/diff-gate-tokens/` (the gate's own checker), including moving or deleting them |
| `toolchain-config` | any change to `.cargo/config` or `.cargo/config.toml` at any depth (rustc wrapper, linker, rustflags, registries) or to `rust-toolchain` / `rust-toolchain.toml` (toolchain `path`) |
| `special-file` | an added or changed symlink (mode 120000) or submodule / nested repository (mode 160000). The gate never follows a link to scan its target |
| `gitattributes-change` | an added, edited, moved or deleted `.gitattributes` at any depth. Owner-approvable by an approval row, like the content rules; `DIFF_GATE_OWNER_APPROVED` does not clear it |
| `rust-parse` | a changed `.rs` file that does not tokenize, or that syn cannot parse. rustc would reject the first; the second is reported so a file the checker cannot fully read never passes unread. The tokens it does read are still checked |
| `rust-tokens` | the token helper could not be built, run, or gave output the gate does not accept. Reported once per changed `.rs` file, as `helper unavailable: review-required (reason)`. Owner only: an approval row does not clear it; `DIFF_GATE_OWNER_APPROVED` does |
| `approvals-change` | an added, removed or edited row in `config/diff-gate-approvals.toml`, or any change to `config/diff-gate-owner-ledger.toml` or `config/diff-gate-sensitive.toml`. An added row whose hash is on the owner ledger at the base ref's tip is reported `approved` (see [Owner ledger](#owner-ledger)) |

The file list comes from `git diff --raw -z --no-renames`, so a path is used byte for byte:
non-ASCII names, quotes and a ` b/` inside a path cannot hide or rename a file. Every
`diff --git` header in the patch must match that list, in order, or the gate stops with exit 2.
A path with a control character (tab, newline and so on) is refused with exit 2.

The patch is taken with `--text --no-textconv --no-ext-diff --no-renames`, explicit
`--src-prefix=a/ --dst-prefix=b/`, `--no-relative`, `--no-color`, `-U0 --inter-hunk-context=0`,
`--submodule=short` and `-c core.quotePath=false -c diff.noprefix=false -c
diff.mnemonicPrefix=false`, including the `--no-index` diff of untracked files in `--worktree`
mode. Explicit options beat config set in a file or through `GIT_CONFIG_COUNT`, so
`diff.noprefix`, `diff.srcPrefix`, `color.ui`, `diff.relative`, `diff.interHunkContext` and
`diff.external` cannot change what the gate reads; `GIT_DIFF_OPTS` and `GIT_EXTERNAL_DIFF` are
unset. Without `--text`, a `.gitattributes` line such as `*.rs -diff` or `*.rs binary` makes git
print "Binary files differ" with no hunks, and a `diff=<driver>` textconv could rewrite what the
gate sees; both are ignored, and any `.gitattributes` change is reported on its own.
`--no-renames` turns a rename or copy into a deletion plus a new file, so the new file is
scanned in full: a `.txt` renamed to `.rs` is new code, and a file renamed to `build.rs` is a
new build script. Moving an existing `.rs` file therefore reports its existing findings again.

The gate fails closed. A failed `git diff`, `git ls-files` or blob read, a parse stage that does
not finish, or a diff that does not match the file list exits 2.

### How .rs files are read

Two scanners read every changed `.rs` file, and their findings are merged (one finding per rule
and line, with the occurrences of both):

1. **The line lexer** in the script. Each `.rs` file with added lines is lexed in full from the
   head blob (nested block comments, line comments, strings with escapes, raw strings, char and
   byte literals versus lifetimes), and the added line is also lexed on its own; a rule fires if
   it matches either result. It errs towards false positives: a line inside a multi-line string
   is also read as code by the per-line lex.
2. **The token helper** (`tools/diff-gate-tokens`), which reads the file the way rustc does.
   See [The token helper](#the-token-helper).

Comment, string and char-literal contents are never code, so `"unsafe"` in a string or
`// unsafe` in a comment is not a finding. Doc comments are different: their Rust code blocks
are doctests, which `cargo test` compiles and runs, so the helper scans them.

### The token helper

`tools/diff-gate-tokens` is a small Rust binary (proc-macro2 and syn, no other dependencies). The
regex lexer kept missing valid Rust forms (`std :: process`, glob imports plus aliases,
multi-line brace imports, `use ::reqwest as r`, `#[path]` split over a multi-line `cfg_attr`,
doctests, `global_asm!`), so on 2026-11-15 the owner chose a real tokenizer over a third round of
regexes. For each changed `.rs` file (added, modified or retyped; deleted files are skipped) it:

- lexes the file with proc-macro2, so whitespace and comments between tokens do not matter,
  `r#process` is `process`, and doc comments arrive as `#[doc]` attributes;
- collects every `use` and `extern crate` binding in the file (any module or block, inside macro
  bodies too) and resolves each path through them, glob imports included. This over-approximates
  on purpose: a name that could mean `std::process::Command` anywhere in the file is treated as
  meaning it everywhere. A glob is only applied to type-like (upper-case) names and to the
  function names the table lists (`libc::fork`), or every identifier in a file with
  `use std::process::*;` would be a finding;
- checks the same rules as the lexer, plus the forms above, on the resolved paths, at any depth:
  macro bodies, attributes (`#[cfg_attr(..., path = ...)]` over several lines) and doctests
  (fenced blocks whose info string is Rust, indented blocks, hidden `# ` lines);
- decides what the diff touched. A construct is a finding when any of its lines is added: all of
  a `use` item, an attribute or a multi-line path, not only the line its leaf is on. Two more
  cases need the base file. An added line inside an existing `unsafe { }` or `extern { }` block
  is reported on that line. And an unchanged line whose meaning changed is reported: each
  occurrence is keyed by (rule, what it resolves to, line text), and a key that occurs more
  often in the head than in the base, beyond what added lines explain, reports its
  occurrences. That catches `use mock as p;` becoming `use std::process as p;` with the
  `p::Command` lines left alone;
- fails closed: a file that does not tokenize, or that syn cannot parse, is a `rust-parse`
  finding.

Over the whole tree (2,736 `.rs` files on 2026-11-15) it reports no parse failures.

**Trust and bootstrapping.** The helper is part of the gate, so a change must never pick its own
checker. The script builds it from `tools/diff-gate-tokens` at the base ref's tip (the trusted
commit in CI), never from the checkout:

1. `git rev-parse <base tip>:tools/diff-gate-tokens` gives the helper's tree hash.
2. The binary is cached at `<cache>/tokens-<tree hash>/diff-gate-tokens`, where `<cache>` is
   `$DIFF_GATE_CACHE`, else `$RUNNER_TEMP/weftos-diff-gate` (CI), else
   `${XDG_CACHE_HOME:-~/.cache}/weftos-diff-gate`. A cache inside the checkout is refused: a
   change could commit a binary at the predictable path.
3. On a miss, `git archive <tree>` is extracted into a fresh directory under the cache and built
   with `cargo build --release --locked` (its own `[workspace]` and `Cargo.lock`, exact version
   pins, so the root Cargo.toml and Cargo.lock cannot change it; the extract is outside the
   checkout, so the checkout's `.cargo/config.toml` and `rust-toolchain.toml` do not apply).
   The binary is moved into place atomically. A cold build takes about 15 seconds plus the crate
   download; CI needs no workflow change, since GitHub's Ubuntu runners have cargo.
4. The binary must answer `--protocol` with `diff-gate-tokens protocol 1`; its output must parse
   (known paths, numeric lines, well-formed spans). Anything else is `rust-tokens`.

Until the helper is on the base, every `.rs` change gets the owner-only `rust-tokens` finding:
the landing that adds it, and any branch judged against a base without it. That is the
bootstrap: the first landing is owner-approved anyway (it changes `scripts/diff-gate.sh`).
A change to `tools/diff-gate-tokens/` is a `ci-workflow` finding, like a change to the script.
Never cache `<cache>` with `actions/cache`: a cache saved by one run is a binary the next run
trusts.

`DIFF_GATE_TOKENS_BIN` points the script at a prebuilt helper, and `DIFF_GATE_TOKENS_REF` picks
another ref to build it from. Both are for local testing; CI sets neither, and the script says
so on stderr when the first is used.

### Sensitive crates (interim)

Owner decision 2026-11-15 (G1, option c). Review rounds 2 and 4 each found Rust forms the
token regexes miss (`std :: process`, glob imports and aliases, multi-line imports, attributes
split over lines, doctests, `global_asm!`), and the 7306a144 change added new callers of an
EXISTING `unsafe { libc::kill(...) }` line, which no content rule can see. Until a syn-based
token helper replaces the regexes (a separate lane), the gate also treats code in sensitive
crates as review-required:

- Every added non-blank `.rs` line under a `[[sensitive]]` glob in
  `config/diff-gate-sensitive.toml` is a `sensitive-crate` finding, comments and doc comments
  included (doctests compile under `cargo test`).
- In a sensitive crate's Cargo.toml, a new or changed target `path` (`[lib]`, `[[bin]]`, ...)
  is one too, so a target cannot be pointed at an exempt file.
- Exempt (test-only): files matching an `[[exempt]]` glob (today `**/tests/**`,
  `**/*_tests.rs`, `**/tests.rs`) that nothing builds outside a test cfg (see below), and lines inside an inline `mod name { ... }` whose
  attributes include `#[cfg(test)]` or `#[cfg(all(test, ...))]`. The region is found on the
  full-file lex: the cfg attribute, then only attributes, comments or blank lines, then
  `mod name {`; it ends at the matching `}`. Code after that brace on the same line is not
  exempt. The exemption counts only if the region closes and the file's braces balance (never
  negative, zero at the end), so a lexer confusion exempts nothing. `#[cfg(any(test, ...))]`,
  `#[cfg(test)]` on a `fn` or `use`, and `#![cfg(test)]` file attributes are not exempt.
- The other rules (`unsafe`, `net`, `process`, ...) still apply to exempt files.

The list is read at the tip of the base ref, like the owner ledger, and a change to it in the
diff is an `approvals-change` finding (owner override only); removing a crate in the same diff
as a change to it does not exempt that change. `DIFF_GATE_OWNER_APPROVED` does not clear
`sensitive-crate` findings: an approval row does. Use one row per reviewed file version:

```toml
[[approval]]
rule        = "sensitive-crate"
path        = "crates/clawft-kernel/src/workload_runtime/supervise.rs"
pattern     = "*"
blob        = "<git rev-parse <commit>:<path>>"
reason      = "card 7306a144: RSS watchdog, reviewed"
approved_by = "owner"
```

Name-based exemption is checked against the declarations (RV-F1, 2026-11-15). Before, it
assumed a `*_tests.rs`, `tests.rs` or `tests/` file was only compiled under a test cfg, so
deleting the `#[cfg(test)]` line above its `mod` declaration (a pure deletion, no added line)
moved new code in that file into the normal build with no finding. Now:

- **A file exempt by name is test-only only if nothing builds it outside a test cfg.** The gate
  looks in every file under the `[[sensitive]]` globs at the head (the working tree, untracked
  files included, with `--worktree`) for anything that can build it: a `mod <stem>;`
  declaration in the file's directory or above (the stem is the file name without `.rs`, or the
  directory name for `mod.rs`), and the file's name in a `#[path = "..."]` (also inside
  `cfg_attr`), an `include!(...)` or any `path = "..."` (file names compare without case;
  `dir/mod.rs` for a `mod.rs`). Comments and strings do not count. Each such reference must be
  test-only: its attribute chain carries `#[cfg(test)]` or `#[cfg(all(test, ...))]` (also
  written over several lines), it lies inside an inline test module, or the declaring file is
  itself an exempt file that passes this check. A reference after other code on the same line
  never counts as test-only. A file that is a Cargo target path in a sensitive Cargo.toml, or
  that lies under `src/bin/` (a binary target), is never test-only. An exempt-named file
  nothing refers to (an integration test under `<crate>/tests/`, or an orphan) stays exempt.
  When a file fails the check its added lines are `sensitive-crate` findings, and the gate says
  why on stderr: `... is not treated as test-only: declared or named without a test cfg at
  <file>:<line>`.
- **Taking a test cfg off a module declaration is a finding by itself.** In a sensitive file, a
  removed line holding a `cfg` / `cfg_attr` attribute that names `test` is a `sensitive-crate`
  finding when, on the base, the item it was attached to (after any other attributes, comments
  and blank lines) is a `mod` declaration, file or inline, and that declaration line stays.
  Changing the attribute (to `cfg(any(test, ...))`, `cfg(not(test))`, `cfg(unix)`, ...) removes
  the old line, so it counts too. Deleting a whole test module, or a test cfg on a `fn`, does
  not.

On 2026-11-15, with a comment appended to all 274 name-exempt `.rs` files in the sensitive
crates, every one passed the declaration check (no false positive). That diff took about 60
seconds against about 40 before, because every module parent that names one of the files is
read too; a normal landing touches a few such files.

Limits: a `#[cfg_attr(test, path = "x_tests.rs")]` declaration is not recognised as test-only
(the reference is in a `cfg_attr`, not under a test cfg), so new code in `x_tests.rs` is a
finding; a declaration a macro builds from pieces is not seen (`paste!` and `concat_idents!`
are findings of their own). A sensitive crate that builds a file outside the sensitive globs
(`#[path = "../../other/x.rs"]`) is judged by that file's own crate.

False-positive cost: this flags code, not risk. From 2026-11-01 to 2026-11-15, 280 commits on
`target-0.8.5` added 89,136 `.rs` lines; 57,699 were in these crates, and 37,061 of those in
194 non-exempt files (an upper bound: inline `#[cfg(test)]` blocks are exempt as well). So
nearly every landing that touches a sensitive crate needs owner rows, about one blob row per
touched file per landing, and the report lists every added line. That is the price the owner
accepted for this interim; it ends when the syn helper takes over the content rules.

The token helper and this rule landed together (merge of the G2-G4 and G1-syn lanes,
2026-11-15), so both run: a line in a sensitive crate can carry a `sensitive-crate` finding and,
separately, a content finding (`unsafe`, `process`, ...) from the lexer or the helper. Each is
cleared on its own. If the helper is unavailable, the file's `rust-tokens` finding is still
owner only, whatever `sensitive-crate` rows exist. Retiring this rule is a separate owner
decision; the helper's landing does not retire it.

A finding exits 1 with a per-finding report. `--json` prints the findings as JSON. In CI the
findings table is also written to `$GITHUB_STEP_SUMMARY`.

## Approvals

An approved finding is reported as `approved` and does not fail. Approvals live in
`config/diff-gate-approvals.toml`; the file header documents the format. The gate reads the
approvals from the head commit (`--head`, default `HEAD`), the same blob the `approvals-change`
check compares, and from the working tree only in `--worktree` mode. An approvals file in a
checkout that differs from `--head`, or an uncommitted one in commit mode, approves nothing. Each `[[approval]]`
needs `rule`, `path` (glob), `pattern` (substring or regex of the added line, or `"*"` for the
whole file), `reason` and `approved_by`.

Unknown keys in an `[[approval]]` (a misspelt `blob`, say) are a setup error (exit 2), so a
typo can never silently widen a row.

### Scoping a row to one file version (`blob`)

A row may carry `blob = "<git blob id>"`, 40 hex digits (64 in a SHA-256 repository). It then
approves a finding only when the file's post-change content has exactly that id; any later
edit of the file is a new blob and needs a new row. Compute the id from the content you have
reviewed:

```
git rev-parse <commit>:<path>          # a committed version
git hash-object --no-filters <path>    # the working-tree bytes
```

For a symlink the id is that of its target string (`printf %s <target> | git hash-object
--stdin`); for a deletion it is 40 zeros. In `--worktree` mode the gate hashes the bytes on
disk, without clean filters or eol conversion, since that is what a local build reads; CI
compares committed blob ids.

`blob` is what makes path-rule rows narrow. `ci-workflow`, `build-script` (a new `build.rs`),
`toolchain-config`, `special-file` and `gitattributes-change` findings carry fixed text such as
`file changed`, so a row for one of them without `blob` approves every future change to the
matching path, whatever its `pattern`: the `ci-workflow pr-gates.yml "*"` row approved
appending `run: curl ... | sh` to that workflow. Such rows, and `"*"` rows of any rule without
`blob`, still work for backward compatibility, but every run prints a
`WARNING: unscoped approval row` line for each (stderr and the CI step summary), and a finding
they approve is reported with `[UNSCOPED row: no blob]`. `blob` works on any rule, so it can also
pin a content row to one reviewed version of its file.

`blob` is part of the row's ledger hash (see [Owner ledger](#owner-ledger)), so a ledgered row
cannot be re-pointed at another file version.

### How Cargo.toml is read

Pass 3 reads each changed Cargo.toml in full, on the base and on the head, with a small TOML
reader (`cargo_entries` in the script; `scripts/diff-gate.sh --cargo-entries FILE` prints what
it makes of a file). It is a tokenizer, not one regex per line:

- **Line endings.** CRLF is a line ending; so is a lone CR (not a TOML newline, so Cargo rejects
  such a file, but splitting there can only show more keys). Before 2026-11-15 a CRLF manifest
  hid its tables: the header kept its CR and `[dependencies]\r` never matched (RV-B1). Pass 1's
  `build` / `proc-macro` / escaped-key checks on added lines split on CR the same way.
- **Keys.** Table and array-of-tables headers and dotted keys are split into segments: bare
  (`A-Za-z0-9_-`), basic strings (`"..."`, escapes decoded: `\u0065` is `e`) and literal
  strings (`'...'`), with spaces around the dots. Each key is resolved to its full path (header,
  then the dotted key, then the keys of any inline table it holds) before it is classified.
- **Values.** Basic, literal and both multi-line string forms, inline tables and arrays (both
  also over several lines, with comments), and bare scalars.
- **Classification.** A path is a dependency entry when it starts with `dependencies`,
  `dev-dependencies`, `build-dependencies` (or the `_` spellings), `target.<T>.` one of those,
  `workspace.dependencies`, `patch.<registry>` or `replace`; the next segment is the entry name.
  So `[dependencies] evil = {...}`, `dependencies.evil = ...`, `dependencies = { evil = ... }`
  and `[target.'cfg(unix)'] dependencies.evil = ...` are one and the same entry (RV-B2). A
  `path` under `lib`, `bin`, `example`, `test` or `bench` is a target path.
  `package.metadata`, `workspace.metadata` and `[features]` are skipped: Cargo resolves nothing
  there.
- **Fail closed.** A statement that does not parse, every key under a header that does not
  parse, and any other key path with a segment containing `dependencies`, or equal to `patch`,
  `replace` or `target`, is `cargo-unparsed`. The tables on the way to a dependency table
  (`[target]`, `[target.<T>]`, `[patch]`, `[workspace]`) are not. Unparsed statements are keyed
  by their table and text, so one the base already had is not reported again, while a new key
  under a header the base already could not read is.

On 2026-11-15 the reader gives the same entry keys as the old one on all 99 tracked Cargo.toml
files and finds no unparsed statement in any of them.

### Cargo.toml findings

For `new-dep`, `build-script` and `proc-macro` findings on a Cargo.toml line, and Cargo target
`include` findings, `pattern` is matched against the entry line itself with any trailing TOML
comment removed (a `#` inside a quoted string stays), not against the report wording
(`new entry in [dependencies]: ...`). So `^` anchors at the start of the entry, and a comment
cannot carry an approved name: before 2026-11-15 the `rvf-(crypto|wire)` row approved
`evil = { git = "..." } # rvf-crypto =`. A commented-out key (`# build = "b.rs"`) is not a
finding. A pattern still sees the whole entry, including its quoted values; anchor it on the
name (`^rvf-crypto[[:space:]]*=`) so it cannot match inside a `git` URL.

Approvals for `unsafe`, `net`, `process`, `ffi` and `include` are per occurrence. Each
occurrence on the line (a keyword, or one path expression such as
`std::net::TcpListener::bind`) is approved only by an approval match that overlaps it, so a row
written for one statement does not approve a second `unsafe` block or a second call added on
the same line. A pattern of `"*"` still approves every occurrence in the file. Other rules
compare the pattern with the reported text, as before.

## Owner only

Changing the approvals rows is itself a finding (`approvals-change`). Comment and blank-line
edits, and reordering rows, carry no authority and are not findings. A row change passes only
when the added row is on the owner ledger (below), or when the owner sets `DIFF_GATE_OWNER_APPROVED=1` locally, or in CI sets the repository variable
`DIFF_GATE_OWNER_APPROVED=1` or the PR label `diff-gate-approved`. Both need maintainer rights.

`config/diff-gate-sensitive.toml` is owner-only in the same way: any change to it is an
`approvals-change` finding that only the override clears, and the gate reads it at the base tip.

**Agents and automated lanes must never add, edit or remove approvals, and must never set
`DIFF_GATE_OWNER_APPROVED`, the variable or the label.** If the gate rejects a diff, change the
diff, or stop and ask the owner. Approvals apply to later diffs too, so keep `path` and
`pattern` narrow.

## Owner ledger

`config/diff-gate-owner-ledger.toml` records approval rows the owner has already decided on, so
landing those rows does not need a fresh `DIFF_GATE_OWNER_APPROVED` override each time. Each
`[[entry]]` has a `decision` id (the decision file or card), a `date` (`YYYY-MM-DD`) and the
`sha256` of the row's canonical text; `note` is optional. The canonical text is built from the
parsed values, so quoting style and key order do not change it:

```
rule=<rule>\npath=<path>\npattern=<pattern>\nreason=<reason>\napproved_by=<approved_by>\n
```

followed by `blob=<blob>\n` when the row has a `blob`. Rows without one hash exactly as before
blobs existed, so existing ledger entries stay valid.

`scripts/diff-gate.sh --ledger-hash [FILE]` prints the hash, rule, path and pattern of every row
in FILE (default: the approvals file).

How the check decides, comparing the approvals rows on the base with the head as multisets:

| Change | Result |
|---|---|
| added row whose sha256 is in the ledger **at the tip of the base ref** | `approved`, detail names the decision id |
| added row not in that ledger | FAIL |
| removed row, or an edited row (an edit is a removal plus an addition) | FAIL |
| any change to the ledger file (add, edit, delete) | FAIL |
| comment or blank-line edits, reordered rows | not a finding |
| `scripts/diff-gate.sh`, `.github/workflows/` or `tools/diff-gate-tokens/` changed, moved or deleted | FAIL (`ci-workflow`) |

`DIFF_GATE_OWNER_APPROVED=1` still approves every `approvals-change` finding. It does not
approve `ci-workflow`.

Which ledger approves: the one at the tip of the base ref (`--base` / `DIFF_GATE_BASE`, in CI
the PR base), not the one at the merge-base. Reading it at the merge-base meant an entry the
owner revoked on the target branch still approved a row on any branch forked before the revoke.
The tip alone is used rather than "both merge-base and tip" because the tip is already the
stricter trust root: every entry at the merge-base reached the tip through the same protected
branch, so the only thing the intersection adds is refusing entries the owner landed after the
fork, which forces a pointless rebase. Revocation takes effect at once; a new entry also takes
effect at once. Whether the ledger file itself changed is still judged on the diff (merge-base
against head), like every other finding.

Why an agent cannot self-approve: the ledger is read from the base ref, never from the diff
under review. An entry added in a diff is itself an
`approvals-change` finding, and it cannot clear a row added in the same diff. The only way an
entry starts counting is to land on the target branch, which already needed the owner override.
In CI the gate script also comes from the trusted base, on pushes as well as PRs, so a diff
cannot weaken the ledger logic either.
The ledger logic lives inside `scripts/diff-gate.sh` for that reason: a separate helper file
would be read from the PR head.

Owner workflow: when approving rows, add their hashes to the ledger (with the override) in the
same landing as the decision, or seed them ahead of the rows. To revoke a decision, remove the
ledger entries as well as the rows; a row whose hash stays on the ledger can be re-added later.

## Known limits

- Workflow files: GitHub runs `.github/workflows/*.yml` from the commit under review, on pushes
  and PRs alike, so a diff that edits `diff-gate.yml` (or drops the `needs: diff-gate` from a
  caller) changes the step that picks the trusted gate. The gate itself reports any such change
  as `ci-workflow`, but only when the edited workflow still runs it. Since 2026-11-15 the gate
  script, ledger and approvals come from protected history on every event; the workflow file is
  the remaining trust in the commit under review. Branch protection on the protected branches
  (PRs required, `diff-gate` a required check, CODEOWNERS review for `.github/`) covers it.
- The trusted commit is only as good as branch protection on the default branch and `target-*`,
  including who may create a `target-*` branch: the newest `origin/target-*` is a trust root.
- `blob` is read only by gates from 2026-11-15 on. An older gate copy ignores the key and treats
  the row as unscoped. CI runs another branch's gate only when the trusted base has none (see
  [CI: which gate runs](#ci-which-gate-runs)), so this matters only until every protected
  branch carries the new gate.
- `--worktree` mode runs locally against the local git config. `--text --no-textconv
  --no-ext-diff` neutralise attribute and diff-driver tricks, but a local clean `filter=` driver
  still rewrites working-tree content before the diff. That only fools the local run; CI diffs
  committed blobs, which filters do not touch.
- The trust root is the base ref. A locally rewritten `origin/<target>` ref moves it on that
  machine only.
- The content rules are a token list (see [How .rs files are read](#how-rs-files-are-read)).
  New ways to reach the network or spawn a process through a crate the list does not name are
  only caught as the `new-dep` that brings the crate in.
- Resolution is per file. A re-export in another file (`pub use std::process::Command as Cmd;`
  in `util.rs`, then `use crate::util::Cmd; Cmd::args` elsewhere) is only caught by name
  (`Command::new`, `CommandExt`, the socket types), not by resolution. The re-export itself was a
  finding when it landed.
- A glob import is applied only to upper-case names and listed functions: with
  `use reqwest::*;` already on the base, an added `get(url)` is not a finding.
- Code a macro produces from pieces (a proc-macro that turns a string into a path) is not seen;
  `paste!` and `concat_idents!` are findings, and a new proc-macro crate is a `new-dep` or
  `proc-macro` finding.
- A Markdown file pulled in with `#![doc = include_str!(...)]` becomes doctests, but later edits
  to that `.md` file are not scanned. Adding the include is an `include` finding; none exist
  today.
- Over-approximation means false positives: a type mention such as `fn f(c: &mut Command)` in a
  file that imports `std::process::Command` is a `process` finding.
- Only `.rs` files, Cargo.toml, the paths above and the CI files are inspected. A change to a
  script the build or CI runs (for example `scripts/build.sh`) is not a finding.
- `scripts/diff-gate.sh` is one file on purpose, although it is past the 500-line guideline:
  on pull requests CI runs the base copy of this one file, and a helper it sourced would be read
  from the PR head.

For the gate to block a merge, the `diff-gate` job must be a required status check in branch
protection. A skipped dependent job counts as passing, so requiring only the downstream jobs is
not enough.

## Tests

`scripts/tests/diff-gate-test.sh` builds throwaway repos under `target/` and covers every rule:
added fails, already-on-base passes, approved passes, approvals-change with and without the owner
flag, clean diff passes, and the owner ledger: base-ledger rows pass, unlisted, removed and edited
rows fail, ledger edits fail without the override, gate-script edits and moves fail, and the
override passes the `approvals-change` cases. Review hardening cases: `.gitattributes` `-diff`,
`binary` and textconv drivers do not hide `unsafe`/`process` (commit and `--worktree`, tracked
and untracked), `.gitattributes` changes fail unless an approval row covers them, a ledger entry
revoked on the base tip stops approving, and approvals in a checkout other than `--head` do not
approve. Content-layer cases (2026-11-15): lines starting with `*` or `/**/`, `'\"'` and
`r"\"` quotes, code after a multi-line string, lifetimes, aliased `Command`, `tokio::net`,
`extern "system"` and bare `extern`; one approval per occurrence (the candle row); a path
containing ` b/`; `diff.noprefix`, `diff.mnemonicPrefix`, `diff.srcPrefix`, `color.ui`,
`diff.interHunkContext` and `core.quotePath`; an unreadable blob exits 2; non-ASCII, quoted
and control-character paths (commit and `--worktree`); `include!`, `#[path]` and Cargo target
paths; symlinks; renames to `.rs` and `build.rs`; upper-case `.RS`; `.cargo/config*` and
`rust-toolchain*`; `[patch]`, `[replace]`, quoted keys, `registry`, `package`, inline and
dotted `build` / `proc-macro` keys.

Cases from 2026-11-15 (G1-G4 lane): Cargo patterns against comment-stripped entry lines
(`# rvf-crypto =` after an evil `git` source, a `#` inside a quoted URL, `^` anchors, the
report wording, build-key comments); `blob` rows for `ci-workflow`, `build-script`,
`toolchain-config`, `gitattributes-change` and `special-file` (exact content approved, any
other content fails, commit and `--worktree`), the unscoped-row warning, malformed blobs and
unknown keys (exit 2), and `blob` in the ledger hash; `sensitive-crate`: the 7306a144 new
caller of an existing `unsafe { libc::kill }`, doc comments, blank lines, `tests/`,
`*_tests.rs`, inline `#[cfg(test)]` and `#[cfg(all(test, ...))]` modules, code after or on the
closing line of a test module, a `{` in a string or char, an unclosed module, `cfg(any(...))`
and `cfg(test)` on a `fn`, a Cargo target path into `tests/`, blob rows, the list read at the
base tip, and list changes being owner-only.

Token helper cases: a missing helper, the wrong protocol, a failing helper, malformed output
and a finding for a path outside the diff all give the owner-only `rust-tokens` finding; an
approval row does not clear it, the owner flag does; a docs-only diff needs no helper; a base
without `tools/diff-gate-tokens` and a cache inside the checkout are refused; a change under
`tools/diff-gate-tokens/` is `ci-workflow`; lexer and helper findings on one line merge into one.

The suite needs only bash and git: every fixture runs with a stub helper
(`scripts/tests/diff-gate-tokens-stub.sh`) that finds nothing unless told to. Set
`DIFF_GATE_TEST_TOKENS_BIN` to a built helper to run every fixture against the real one, plus
the `tokens(real)` cases: the G1 escapes listed above, a line added inside an `unsafe` block, an
import edit that changes an unchanged line, a file that does not parse, and strings and
comments that still pass.

`tools/diff-gate-tokens/tests/escapes.rs` (`cargo test --locked` in that directory) ports every
Rust-rule fixture and every escape from the review rounds, and checks that strings, comments,
`text` code blocks, `std::process::exit` and an aliased `clap::Command` are not findings.

Review-fix cases (2026-11-15, `scripts/tests/diff-gate-test-rv-fixes.sh`, section 13): CRLF,
mixed and lone-CR manifests (a dependency after a lone CR, a `build` key after one, a CRLF
source change, a pure LF-to-CRLF conversion passing); dependency entries through `[workspace]`
inline tables, dotted `replace`, a whole `target` inline table, `[target]` with a dotted cfg
key, dotted `git` under `[patch.crates-io]`, a `\u0065` escape spelling `dependencies`,
`dev_dependencies`, a two-line inline table and an empty `[dependencies.x]` table; controls that
a form change with the same source, `package.metadata` and `[features]` pass; `cargo-unparsed`
for an unknown dependency key path, a scalar where a table belongs, an unreadable header and a
new key under a header the base already could not read; and `--cargo-entries` output for one
manifest that spells every form. RV-F1: a test cfg deleted from a file module declaration and
from an inline test module with no line added, `cfg(test)` changed to `cfg(not(test))`,
`cfg(all(test, unix))` changed to `cfg(unix)` with an attribute between; controls that deleting
a whole test module, a test cfg on a `fn`, or a cfg outside the sensitive crates is not a
finding; an exempt file also reached by a non-test `#[path]`, by a declaration after other code
on its line, by `include!`, as a Cargo `[[bin]]` target or under `src/bin/`; a `tests/`
module declared by an exempt file that is not itself a test; controls for a test-declared
parent, a multi-line `cfg(all(test, ...))`, a declaration inside an inline test module and a
mention in a comment or string; and an untracked non-test declaration in `--worktree` mode.

`DIFF_GATE_TEST_TMP` puts the fixture repos on another disk (the default is `target/`).
`DIFF_GATE_TEST_FILTER=<ERE>` runs only the `case_run` fixtures whose name matches.

`scripts/tests/diff-gate-ci-test.sh` runs the workflow's gate step against fixture remotes: a
push that rewrites the gate to `exit 0` still fails (with a control showing that copy passes
itself), a second push on an unprotected branch cannot use a gate or ledger entry from the
first, new branches and new protected branches are judged against another protected tip, a
force-push is judged from the merge-base and fails closed when `before` is gone, and
`workflow_dispatch` and pull requests (including a base without the gate) use a trusted copy.
Since 2026-11-15 it also covers a PR into an unprotected branch carrying a weakened gate (and a
PR with no base branch name), a self-made `main` (a later push and a PR into it), and `main`
as the default branch staying protected.
