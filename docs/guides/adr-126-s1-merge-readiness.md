# ADR-126 S1: merge readiness (rehearsal, 2026-11-08)

For folding into `docs/guides/adr-126-integration-status.md` (that file lives on `feat/adr126-s5`, so this is a
separate page on `feat/adr126-s1` until S5 picks it up). Rehearsal only: a throwaway worktree and branch off
`target-0.8.5` (`527ea4db`), both removed afterwards. No real merge, no push, approvals file untouched.

## Result

- **Conflicts onto `target-0.8.5`: none.** `target-0.8.5` is an ancestor of `feat/adr126-s1` (0 commits on target
  that S1 lacks, 25 on S1), so the merge is a fast-forward and the merged tree is identical to the S1 tree
  (checked). 69 files, +10 053 / -9 against target; outside `crates/`, `docs/` and `scripts/` it adds
  `contracts/cogs/vectors/store.v1/*` and touches `Cargo.toml`/`Cargo.lock` only for the `weftos-store` member
  and its dependency lines.
- **Against the other lanes** (`git merge-tree`, tip `4b85d3ee`): S2 clean, S4 clean, S5 clean (S5 already carries
  S1 through `19de450e`). One collision existed with S2/S5 (`crates/weftos-store/src/lib.rs`, S1's test-only
  `mod` lines next to their new `pub mod` lines); S1 moved its lines above the `pub mod` list in `4b85d3ee`, so it is gone.
  S3 collides with S1 in `Cargo.toml` (both add a workspace member) and in the ADR (both append sections at the
  same place); both are keep-both resolutions and `integ/adr126` already contains them, so landing S5 covers it.
  Do not land S1 and S3 as separate merges without resolving those two by hand.

## Diff gate (`scripts/diff-gate.sh --base target-0.8.5`)

69 files, 8 failed, 3 approved. The 8 are exactly the receiver and the approvals file, none from S1's later commits:

| Rule | Where |
|---|---|
| net | `store_ingest.rs:27, :98` (TcpListener); `store_ingest_tests.rs:5, :71, :166` |
| process | `store_ingest_tests.rs:162, :188` (the live test that spawns RustFS) |
| approvals-change | `config/diff-gate-approvals.toml` |

The 3 approved are S1's `reqwest` lines and the `weftos-store` path dependency of `clawft-weave`. Entries that clear
the 8 are drafted in `docs/guides/rustfs-store-ingest-approvals.draft.toml`; only the owner appends them.

## Scoped checks on the merged tree (through `scripts/build.sh`)

- `check` and `clippy` for `weftos-store`, `clawft-weave`, `clawft-kernel` (with `weftos-store/rustfs`): exit 0.
- `weftos-store` tests: 115 passed, 15 ignored (live RustFS).
- `clawft-weave` tests: 1297 passed, 2 failed, 5 ignored in the full run. The two
  (`infer_managed_tests::an_over_budget_or_excluded_start_is_unplaceable_chained_and_clears_when_memory_frees`,
  `infer_managed_tests::beyond_loopback_needs_a_token_and_a_chained_permit_with_network_lan`) pass alone on the
  merged tree and on `target-0.8.5`: load flakes (the second takes 35 s alone).
- `clawft-kernel` lib tests: 3747 passed, 6 failed in the full run. Five pass alone on both trees (load flakes).
  One, `workload_ctl::tests_licence::the_gate_hashes_the_binary_the_runtime_admits_and_refuses_another_variant_arch`,
  **fails on plain `target-0.8.5` too**: not S1's, and not caused by the merge.
- Not run: `scripts/build.sh gate` (workspace-wide), the live RustFS tests, `www`/wasm builds.

A note on method: a direct `cargo test` (not via `build.sh`) fails 156 weave tests with "a test resolved the runtime
root ... without a WEFTOS_RUNTIME_DIR override". That is the harness, not a regression; always go through `scripts/build.sh test`.

## What the real merge needs

1. The owner's approvals entries for `store_ingest*` (above); nothing else trips the gate.
2. Land via S5 (`feat/adr126-s5` carries S1, S2, S3, S4 with the conflicts above resolved), not S1 alone, if S3 is in the same release.
3. The usual `scripts/build.sh gate` on the merged tree, which no lane has run.
4. Owner decisions that ship off: `WEFTOS_STORE_SWEEPER_PUT`, `WEFTOS_STORE_CI_DIRECT_PUT`, the archive schedule
   (`WEFTOS_STORE_ARCHIVE_GC_EVERY_HOURS` and the `*_DAYS` numbers), all unset by default.
5. S3 carrying `seq` through its chain feed and installing the guard; S2's `WakeSink` with `registry_hints`.
