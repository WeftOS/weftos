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
branches are the repository default branch, `master`, `main` and `target-*`. "Newest protected
tip" means the version-sorted newest `origin/target-*`, else the default branch, `master`,
`main`, skipping the branch being pushed.

| Event | Trusted commit (gate copy and base) |
|---|---|
| `pull_request` | the PR base SHA |
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
| `new-dep` | a new entry in any Cargo.toml dependency table, in `[patch.*]` or in `[replace]`, or a changed source on an existing entry (`git`, `path`, `registry`, `registry-index`, `package`, `rev`, `tag`, `branch`). Quoted and dotted keys count. `workspace = true` entries for a dependency the base workspace already declares are not new |
| `build-script` | a new `build.rs` (also one created by a rename), or a `build` key in Cargo.toml in any form: bare, quoted, dotted (`package.build`) or inside an inline table (`package = { build = ... }`); `build = false` is fine. A quoted Cargo.toml key or table name with a backslash escape is reported here too, since it can spell `build` or `proc-macro` |
| `proc-macro` | `proc-macro` / `proc_macro` `= true` in any of the same key forms (`lib = { proc-macro = true }`, `lib.proc-macro`) |
| `net` | `std::net`, `tokio::net`, `async_std::net`, `mio::net` paths, `TcpStream`, `TcpListener`, `TcpSocket`, `UdpSocket`, `reqwest`, `ureq`, `hyper`, `isahc`, `surf`, `attohttpc`, `curl`, `socket2`, `tungstenite`, `quinn` paths or imports, `net` in a `std::{...}` import, `libc` socket calls. Address types (`SocketAddr`, `IpAddr` and friends) do no I/O and are ignored. Unix-domain sockets (`std::os::unix::net`) are local IPC, used by the daemon in about 75 files, and are not findings |
| `process` | `process::Command` in any form (including `std::{process::Command as K}` and multi-line `std::process::{` imports), the `std::process` module imported or aliased, `Command::new`, `tokio::process`, `CommandExt`, `libc` / `nix` exec, fork, spawn and `system`. Paths are resolved through the file's `use` aliases and glob imports, so `use std::process as p; p::Command` and `use std::process::*; use Command as C; C::args` count. `std::process::exit`, `abort`, `id`, `ExitCode`, `ExitStatus`, `Stdio`, `Output` and the `Child` types do not spawn and are ignored |
| `ffi` | the `extern` keyword in any form except `extern crate` (`extern "C"`, `extern "system"`, bare `extern {` / `extern fn`), `#[no_mangle]`, `export_name`, `link_section`, and `global_asm!` / `asm!` / `naked_asm!` (`global_asm!` needs no `unsafe`). An added declaration inside an existing `extern { }` block counts. `#[link(...)]` and `link_name` are not separate findings: they only act on an `extern` block, which is one |
| `include` | code pulled into the build from something that is not a `.rs` file: `include!` whose argument is not a literal `.rs` path, `#[path = "..."]` to a non-`.rs` file, and a new Cargo target `path` (`[lib]`, `[[bin]]`, `[[example]]`, `[[test]]`, `[[bench]]`) to a non-`.rs` file. `#[path]` counts inside `cfg_attr`, over several lines. `include_str!` and `include_bytes!` are not findings in code: they produce a `&str` / `&[u8]` value, and turning data into code needs `unsafe`, which is its own finding. Inside an attribute they are (`#![doc = include_str!("README.md")]` turns that file's code blocks into doctests that `cargo test` runs). `paste!` and `concat_idents!` are findings too: they can build any identifier or path from pieces |
| `ci-workflow` | any change under `.github/workflows/`, to `scripts/diff-gate.sh` or under `tools/diff-gate-tokens/` (the gate's own checker), including moving or deleting them |
| `toolchain-config` | any change to `.cargo/config` or `.cargo/config.toml` at any depth (rustc wrapper, linker, rustflags, registries) or to `rust-toolchain` / `rust-toolchain.toml` (toolchain `path`) |
| `special-file` | an added or changed symlink (mode 120000) or submodule / nested repository (mode 160000). The gate never follows a link to scan its target |
| `gitattributes-change` | an added, edited, moved or deleted `.gitattributes` at any depth. Owner-approvable by an approval row, like the content rules; `DIFF_GATE_OWNER_APPROVED` does not clear it |
| `rust-parse` | a changed `.rs` file that does not tokenize, or that syn cannot parse. rustc would reject the first; the second is reported so a file the checker cannot fully read never passes unread. The tokens it does read are still checked |
| `rust-tokens` | the token helper could not be built, run, or gave output the gate does not accept. Reported once per changed `.rs` file, as `helper unavailable: review-required (reason)`. Owner only: an approval row does not clear it; `DIFF_GATE_OWNER_APPROVED` does |
| `approvals-change` | an added, removed or edited row in `config/diff-gate-approvals.toml`, or any change to `config/diff-gate-owner-ledger.toml`. An added row whose hash is on the owner ledger at the base ref's tip is reported `approved` (see [Owner ledger](#owner-ledger)) |

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
| `scripts/diff-gate.sh` or `.github/workflows/` changed, moved or deleted | FAIL (`ci-workflow`) |

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

`DIFF_GATE_TEST_TMP` puts the fixture repos on another disk (the default is `target/`).

`scripts/tests/diff-gate-ci-test.sh` runs the workflow's gate step against fixture remotes: a
push that rewrites the gate to `exit 0` still fails (with a control showing that copy passes
itself), a second push on an unprotected branch cannot use a gate or ledger entry from the
first, new branches and new protected branches are judged against another protected tip, a
force-push is judged from the merge-base and fails closed when `before` is gone, and
`workflow_dispatch` and pull requests (including a base without the gate) use a trusted copy.
