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
  every job that builds, tests, lints or measures crates has `needs: diff-gate`. On pull requests
  CI runs the gate script as it exists on the base branch, so a PR cannot weaken the check that
  judges it.

The base is `DIFF_GATE_BASE` if set, otherwise the newest `origin/<AGF_TARGET_PATTERN>` from
`config/agent-git.env` (today `origin/target-0.8.*`), otherwise `origin/master`. The merge-base
with HEAD is used. CI passes the PR base or the push's `before` SHA.

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
| `ci-workflow` | any change under `.github/workflows/` or to `scripts/diff-gate.sh` |
| `approvals-change` | any change to `config/diff-gate-approvals.toml` |

A finding exits 1 with a per-finding report. `--json` prints the findings as JSON. In CI the
findings table is also written to `$GITHUB_STEP_SUMMARY`.

## Approvals

An approved finding is reported as `approved` and does not fail. Approvals live in
`config/diff-gate-approvals.toml`; the file header documents the format. Each `[[approval]]`
needs `rule`, `path` (glob), `pattern` (substring or regex of the added line, or `"*"` for the
whole file), `reason` and `approved_by`.

## Owner only

Changing the approvals file is itself a finding (`approvals-change`). It passes only when the
owner sets `DIFF_GATE_OWNER_APPROVED=1` locally, or in CI sets the repository variable
`DIFF_GATE_OWNER_APPROVED=1` or the PR label `diff-gate-approved`. Both need maintainer rights.

**Agents and automated lanes must never add, edit or remove approvals, and must never set
`DIFF_GATE_OWNER_APPROVED`, the variable or the label.** If the gate rejects a diff, change the
diff, or stop and ask the owner. Approvals apply to later diffs too, so keep `path` and
`pattern` narrow.

For the gate to block a merge, the `diff-gate` job must be a required status check in branch
protection. A skipped dependent job counts as passing, so requiring only the downstream jobs is
not enough.

## Tests

`scripts/tests/diff-gate-test.sh` builds throwaway repos under `target/` and covers every rule:
added fails, already-on-base passes, approved passes, approvals-change with and without the owner
flag, clean diff passes.
