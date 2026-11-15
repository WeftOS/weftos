# Pre-build diff gate

`scripts/diff-gate.sh` rejects risky changes before any crate compiles. It looks only at what a
diff adds (added lines and new files); existing code never fails it. It needs only git, grep and
awk and runs in about a second.

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
| `unsafe` | `unsafe` block, fn, impl or trait in a `.rs` file (comments and strings ignored) |
| `new-dep` | a dependency entry in any Cargo.toml dependency table, or a changed `git =` / `path =` source. `workspace = true` entries for a dependency the base workspace already declares are not new |
| `build-script` | a `build.rs` file or a `build =` key in Cargo.toml |
| `proc-macro` | `proc-macro = true` |
| `net` | `std::net` sockets, `TcpStream`, `TcpListener`, `UdpSocket`, `reqwest::`, `ureq::`, `hyper::Client`. Address types (`SocketAddr`, `IpAddr` and friends) do no I/O and are ignored |
| `process` | `std::process::Command`, `Command::new`, `tokio::process`, `libc::exec*` / `libc::fork` |
| `ffi` | `extern "C"` blocks or `#[no_mangle]` |
| `ci-workflow` | any change under `.github/workflows/` or to `scripts/diff-gate.sh`, including moving or deleting them |
| `gitattributes-change` | an added, edited, moved or deleted `.gitattributes` at any depth. Owner-approvable by an approval row, like the content rules; `DIFF_GATE_OWNER_APPROVED` does not clear it |
| `approvals-change` | an added, removed or edited row in `config/diff-gate-approvals.toml`, or any change to `config/diff-gate-owner-ledger.toml`. An added row whose hash is on the owner ledger at the base ref's tip is reported `approved` (see [Owner ledger](#owner-ledger)) |

The diff is taken with `--text --no-textconv --no-ext-diff`, including the `--no-index` diff of
untracked files in `--worktree` mode. Without `--text`, a `.gitattributes` line such as
`*.rs -diff` or `*.rs binary` makes git print "Binary files differ" with no hunks, and every
content rule silently passes; a `diff=<driver>` textconv could rewrite what the gate sees. Both
are now ignored, and any `.gitattributes` change is reported on its own.

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
approve.

`scripts/tests/diff-gate-ci-test.sh` runs the workflow's gate step against fixture remotes: a
push that rewrites the gate to `exit 0` still fails (with a control showing that copy passes
itself), a second push on an unprotected branch cannot use a gate or ledger entry from the
first, new branches and new protected branches are judged against another protected tip, a
force-push is judged from the merge-base and fails closed when `before` is gone, and
`workflow_dispatch` and pull requests (including a base without the gate) use a trusted copy.
