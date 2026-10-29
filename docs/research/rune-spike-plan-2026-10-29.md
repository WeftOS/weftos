# Rune spike on WeftOS, and the feedback it produces

- **Date:** 2026-10-29.
- **Owner direction:** "Let's do a spike on it, and get him some feedback". Him is rUv, the author of Rune.
- **Subject:** [ruvnet/rune](https://github.com/ruvnet/rune), "the learning toolchain for Rust", an architecture preview. v0.2.0 was released on 2026-10-26.
- **Inputs:** `docs/research/rune-evaluation-2026-10-29.md`, a source-level evaluation of what Rune implements today. The spike tests only the features that evaluation finds implemented. Features that are stubbed or only documented become feedback items, not tests.

## What the spike answers

1. **Does Rune build WeftOS?** It has to cope with a large workspace, feature flags, `build.rs`, the `native` and `browser` feature mutex (ADR-083), and the wasm32 and wasip2 targets.
2. **Is it faster or smarter than what we run now?** The baseline is `scripts/build.sh` with per-project sccache, plus agent-git-flow's affected-only checks.
3. **Do its learning features help an agent lane?** That covers RuVector memory, Agent Booster edits, bounded discovery and validated builds, measured on real WeftOS tasks.
4. **Is it safe to put in an agent's hands?** Network access, automatic edits, what it writes and where, and credentials.

## Rules

- **Isolation:**
  - The work happens in a throwaway worktree on the external drive, `/Volumes/ai-models/weftos-worktrees/rune-spike`, made from `target-0.8.5`.
  - `CARGO_TARGET_DIR` is separate, so no existing cache is poisoned.
  - Nothing in the spike changes `scripts/build.sh`, the gate or any shared config.
- **No secrets or client code:**
  - Rune runs only on the WeftOS public tree.
  - Network access is allowed for crates.io only, and any AI or discovery call is logged.
  - If a feature needs an API key, the spike records that and stops; it doesn't use one.
- **Rune's code:**
  - Pinned to the commit under test.
  - Install scripts are read before anything runs.
- **Long jobs** run detached with a log and a PID, watched in the foreground.
- **No posting:** nothing reaches GitHub without the owner approving the exact text (see "Feedback").

## Tasks and measurements

| # | Task | Rune | Baseline | Measure |
|---|---|---|---|---|
| T1 | Install and doctor on macOS arm64 | install steps as documented | n/a | time; failures and their fix |
| T2 | `check` of the workspace, cold | Rune check | `scripts/build.sh check`, cold | wall time, peak RSS, pass/fail per crate |
| T3 | The same, warm, after a one-line change in `clawft-kernel` | Rune | build.sh with sccache | wall time; what was rebuilt |
| T4 | Tests for one crate (`weftos-cogbus`) | Rune test | build.sh test, scoped | time; same pass/fail set? |
| T5 | wasm: `clawft-wasm` browser and wasip2 | Rune with target and features | `build.sh browser`, `build.sh wasi` | builds or not; feature mutex respected |
| T6 | "Affected" selection after the T3 change | Rune's selection, if it has one | agent-git-flow `affected` | which crates and tests each picks |
| T7 | Agent Booster edit: a mechanical change across ~20 files (for example renaming one internal fn) | Rune edit | Ruflo codemod or a hand-written sed | correct? compiles? time; diff noise |
| T8 | RuVector memory: run T3 twice; does the second run use what the first learned? | Rune | n/a | observable effect: logs, selection, time |
| T9 | Bounded discovery: ask for a fix to a seeded compile error | Rune | n/a | what it reads and sends, whether the fix is right, the bound respected |
| T10 | Validated build: produce an artifact, then validate it independently | Rune | our release checksums | what "validated" proves; does it fit owner-run signing? |

Each task records: the command, the exit status, the wall time, the log path, and anything changed outside the spike worktree, which should be nothing.

## Spike result

A short section appended to the evaluation doc:
- a table with T1 to T10 marked pass, fail or n/a, with numbers;
- a recommendation from the same set the evaluation uses: adopt now, trial behind a flag in one lane, pattern-only, or watch;
- if the recommendation is a trial, the first lane and its exit criteria.

## Feedback: what we send rUv

The feedback is concrete and reproducible, from a large real workspace, and every item carries the commit it was tested on. It goes in three forms:

1. **One "field report" issue on ruvnet/rune**, titled for example "Field report: Rune v0.2.x on a 60-crate workspace (WeftOS)". It contains:
   - the environment: OS, arch, Rust toolchain, Rune commit;
   - the T1 to T10 table with numbers against the Cargo and sccache baseline;
   - what worked well, specifically;
   - the top 3-5 things blocking adoption for a workspace like ours, ranked;
   - docs gaps hit while installing and using it.
2. **One issue per reproducible bug.** Each has the title, the steps to reproduce, the expected and actual results, the log excerpt, and the Rune commit. Only bugs with a clean repro become separate issues; anything else stays in the field report.
3. **Feature requests,** only where WeftOS has a real need. Each states the use case and what we do today instead. For example: honouring a project build wrapper like `scripts/build.sh`, sccache interop, and feature-mutex awareness.

Rules for the feedback:
- **Voice:** drafted in Mathew's voice with the voice-writer, for a maintainer audience: direct, specific, no praise padding.
- **Approval:** the owner approves the exact text of each post before it goes up. The posting itself follows the ruvpr flow: our account, our fork if code is involved, no PR to ruvnet unless the owner asks.
- **Security:** any security-relevant finding (unexpected network calls, writes outside the workspace, credential handling) goes privately, not in a public issue.
- **Leaks:** no client names, private paths, hosts or internal IPs. The same leak check we use for public pushes applies.
- **Copies:** the drafts live in `docs/research/rune-feedback/` until posted, then link to the posted issues.

## Done when

- T1 to T10 have run, or are marked n/a with a reason.
- The spike result is appended to the evaluation.
- The feedback drafts are written, leak-checked and handed to the owner for approval.
- The spike worktree and its target dir are removed. Logs are kept under `docs/research/rune-feedback/` if they're small, or summarised if not.
