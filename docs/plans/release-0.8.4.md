# v0.8.4 stable release plan

**Date**: 2026-10-27
**Owner**: Fable (release lead). Lead session "main" relays the owner's approvals.
**Base**: `origin/target-0.8.4` @ `911ef1158` (v0.8.4-rc.15 + 153 commits).
**Previous stable**: v0.8.3 (`fafd6168f`, on `master`). crates.io is at 0.8.3 for all 40 crates.
**Scope**: `docs/plans/release-0.8.4-cut.md` (owner-approved 2026-10-23). This file is the
execution plan for that scope; it does not reopen it.

Tiers. Every step names who does it and how it is verified:

- **local model**: the owner's small models on a member node over the tailnet: Qwen3.5-4B
  (`:8083`, up to 4 parallel requests) for inlined edits and tables, Qwen3.5-0.8B (`:8082`, up
  to 8) for classify and fan-out, each prompt plus output under 8k tokens; Qwen3-Coder-Next
  (`:8081`) for larger drafts when it is up. Driven with the inputs inlined and the exact output
  wanted. Narrow, checkable jobs only. A failed job goes to **haiku**, then to Fable. The tier
  that did each job is recorded in the final report.
- **codemod**: deterministic `sed`/`cargo` edits, no LLM.
- **haiku**: a Claude Code subagent on `haiku` for read-only checks and escalations.
- **Fable**: every merge, gate, tag, approval, publish and push. Nothing irreversible is
  delegated.

Owner gates (wait for "go" from "main"): **G1** tag push (which also starts the crates.io
publish and the signed release), **G2** signing-environment approval in CI, **G3** master
fast-forward. The crates publish is not a separate step: `publish-crates.yml` runs on the
stable tag, so G1 is the crates gate. The dry-run evidence goes to "main" before G1.

## 0. Preconditions (checked 2026-10-27)

| Item | State |
|---|---|
| Release worktree | `/Volumes/ai-models/weftos-worktrees/release-0.8.4`, branch `wt/release-0.8.4` |
| CI secrets | repo: `CRATES_IO_TOKEN`, `TAP_APP_ID`, `TAP_APP_PRIVATE_KEY`; env `weftos-cogs`: `WEAVELOGIC_RELEASE_KEY` (set 2026-10-27) |
| Toolchain | Rust 1.95 with `aarch64-apple-darwin`, `wasm32-wasip2`, `wasm32-unknown-unknown`, `x86_64-pc-windows-gnu` (no gate skips); cargo-dist 0.31.0 matches `[workspace.metadata.dist]` |
| Disk | internal 767 GB free; external 1.8 TB free |
| Local model lane | `:8082` and `:8083` answer a one-line smoke test in ~0.5 s; `:8081` not yet up (owner restart pending) |
| 0.8.4 blockers | all on `target-0.8.4` since rc.15: `1a919ae6` bounded stop (`89fedd4a1`), G0 `85f29f99` (`fe59c46ab`), Teams JWT `4366dc71` (`dd884c0cb`), KB/SBOM `0eff3101` (`af17a6053`), Docker `dec62889` (`43c5b0d89`, `cb4ce070b`) |
| rc.15 workflows | Release, SBOM, KB, Browser WASM all green; Publish Crates skipped (rc); 89 assets |

## 1. Remaining merges, in order

Each lane lands only when its agent reports clippy and tests green on a rebase onto the current
`target-0.8.4` tip. Fable merges with `--no-ff` from the release worktree, runs
`scripts/build.sh check` after each, and pushes `target-0.8.4` at the checkpoint.

| # | Branch | Agent | Why this position | Verify |
|---|---|---|---|---|
| 1 | `wt/kernel-stop-hang` | kernel-stop-hang | 0.8.4 blocker (`1a919ae6`): `weaver kernel stop` still hangs; SIGTERM e2e tests running, work uncommitted | agent's test log; `scripts/build.sh test clawft-weave` after merge |
| 2 | `wt/cogbus-b6` | cogbus-b6 | B6b–B6e + `[mesh_limits]`; it is rebasing now and touches cog install/place paths | agent's clippy+test evidence; `check` after merge |
| 3 | `wt/cogs-phase-b` | cogs-phase-b | Removes 11 Cogs crates (76k lines) and pins `cog-sources`/`cog-repo`/`cog-market` to weftos-cogs v0.1.3 by rev. Goes after B6 so the deletion rebases over B6 rather than B6 over the deletion | agent's checks; `check` + `clippy` after merge; `scripts/publish-crates.sh --dry-run` still lists 40 crates |
| 4 | `wt/infer-manifest` (`317ad883c`, 2026-10-03, 1 commit) | Fable | listed in the cut plan; only clawft-weave node facts. Lands only if it rebases clean and `test clawft-weave` passes; otherwise it moves to 0.8.5 | rebase, `check`, `test clawft-weave` |

Not landing:

- `wt/catalog-cog-awareness` (`f5c9e5ab5`): edits `weftos-cog-manager` and `weftos-cog-market`,
  which Phase B deletes. It belongs in `WeftOS/weftos-cogs`; note it on the cogs intake, do not
  merge it here.
- `wt/dependabot` `d98302f96` (imap 3.0.0-alpha.15): the owner has not decided whether to ship an
  alpha crate. Stays out unless "main" says otherwise. (`11c4a7366` already landed.)
- Everything under "Not in 0.8.4" in the cut plan.

Phase B and crates.io: the three crates that take the git-pinned cog deps (`clawft-weave`,
`weft-licence`, `weftos-cos`) are all `publish = false`, and none of the deleted crates was
`publish = true` or dist-able. The publish list stays at 40 and the release asset set is
unchanged (6 dist apps: `clawft-cli`, `clawft-weave`, `clawft-gui-egui`, `clawft-wasm-host`,
`clawft-wasm`, `weftos`).

## 2. Version bump (codemod, verified by Fable)

Recipe from the rc.15 bump commit `9d67f899c`. No `cargo workspaces` locally, so the edit is
`sed` over an explicit file list, then lockfile regeneration.

| File | Change | Check |
|---|---|---|
| `Cargo.toml` | `0.8.4-rc.15` → `0.8.4` (workspace `version` + 39 `[workspace.dependencies]` version pins) | `grep -c 0.8.4-rc.15 Cargo.toml` = 0 |
| `crates/exo-dag/Cargo.toml`, `crates/clawft-kernel/Cargo.toml`, `crates/clawft-edge-pad/Cargo.toml` | the literal rc.15 pins | same grep |
| `agents/catalog.json` | 31 `"version"` fields | `scripts/build.sh gate` check 18 (catalog up to date) |
| `crates/weftos-leaf-types/Cargo.toml` | `0.1.0` → `0.1.1` (API grew since the 0.1.0 publish: `discovery`, `frame_reader`); dependents `clawft-bvh`, `clawft-kernel` pin `0.1.1` | `cargo package --no-verify -p weftos-leaf-types` |
| `Cargo.lock`, `crates/clawft-core/fuzz/Cargo.lock` | `cargo update --workspace --offline` in both roots | `git diff --stat` touches only workspace member entries |
| `CHANGELOG.md` | §3 | review |
| `docs/src/content/docs/weftos/vision/releases.mdx` | `scripts/build.sh releases-mdx` | generated, diff reviewed |
| `docs/plans/release-0.8.4-cut.md` | status table → landed, with commit ids | local model drafts, Fable checks each id against `git log` |

npm: no package carries the product version (`clawft-ui` 0.0.0, `clawft-docs` 1.0.0,
`vscode-weft-panel` 0.0.1), so nothing to bump there. Repo-wide check: `grep -rn '0\.8\.4-rc'`
outside `CHANGELOG.md`, `releases.mdx` and historical docs returns nothing.

`eml-core` and `clawft-treecalc` stay at 0.1.0: only their `repository` URL changed since the
0.1.0 publish; the publish script skips them.

## 3. CHANGELOG

Input: the uncommitted rc.16 draft in `/Volumes/ai-models/weftos-worktrees/release-rc16`
(CHANGELOG `[Unreleased]`, 37 lines) plus the 114 non-merge commits since rc.15 (saved in the
scratchpad as `log-since-rc15.txt`), plus whatever lanes 1–4 add.

- **local model**: condense the draft + log into a `## [0.8.4] - 2026-10-27` section in the
  file's Keep-a-Changelog style (Added / Changed / Fixed / Security), rc.1–rc.15 summarised in one
  paragraph that points at the rc entries below it. Output is the section text only.
- **Fable** verifies: every bullet maps to a commit in the log; no commit of user-visible scope is
  missing (the cogbus B1–B6 series, Phase B crate removal, cog declarations, NPU service, CoS
  cards, phone app, Ventuno/Pluto plans, dependency advisories, org move); no client names, home
  paths or private hosts. Then: `[Unreleased]` reset to the staging note, compare links updated,
  `scripts/build.sh releases-mdx`.

## 4. Full gate and release dry-run (Fable)

On the combined tip after the bump commit, detached with a log under the scratchpad:

```
scripts/build.sh gate --with-release-dry-run    # 24 checks; rc.15 took ~80 min, load is high today
```

Must be 24/24 with no skips (targets are installed). Check 19 is the agents leak check; check 18
proves `agents/catalog.json` matches the bump. The dry-run proves the four host-triple archives
package with `LICENSE`/`README.md`.

Tests never touch the real `~/.weftos`: after the run, `ls -la ~/.weftos/run` is compared with
the pre-run listing (memory `tests-leak-real-weftos-home`).

## 5. Leak check before any push (Fable, haiku for the sweep)

The repo is public. Before pushing `target-0.8.4` or the tag:

1. `scripts/agents-leak-check.sh` (agents/, with the private terms file at
   `~/.config/weftos/leak-terms.txt`).
2. **haiku** sweeps `git diff v0.8.4-rc.15..HEAD --stat` plus the full text of every new or
   changed `docs/` and `CHANGELOG.md` file for: `/Users/`, `/home/`, `100.` tailnet addresses,
   `192.168.`, `10.` private ranges, tokens/keys and the private leak-terms list (client and
   site names), and reports file:line hits only.
3. Fable reads the hits and fixes or drops them. Node hostnames that already appear in history
   are not new exposure but must not appear in new files either.

## 6. crates.io dry-run (Fable) — evidence for G1

```
scripts/publish-crates.sh --dry-run                     # the 40-crate order and versions
for c in <the 40>; do cargo package --no-verify -p $c; done   # manifest/packaging errors only
```

`cargo publish --dry-run` cannot verify a 0.8.4 crate whose 0.8.4 dependencies are not on
crates.io yet, so packaging is the local proof; the v0.8.3 publish needed manifest fixes, and this
catches that class. Expected: 40 crates, 37 at `0.8.4`, `weftos-leaf-types 0.1.1`, `eml-core`
and `clawft-treecalc` skipped at 0.1.0. The list, versions and packaging result go to "main".

The real publish is CI: `publish-crates.yml` runs `scripts/publish-crates.sh` on the stable tag
with `CRATES_IO_TOKEN`; it skips versions already published and sleeps on 429. 37 are existing
crate names, so the new-crate rate limit (burst 5, then one per 10 min) should not apply the way
it did for 0.8.3, but allow up to an hour.

## 7. Tag and push (Fable; **G1**)

1. Push `target-0.8.4` with the bump commit (checkpoint push, leak check done).
2. `git tag -a v0.8.4 -m "Release v0.8.4"` on that commit (the one with the dated changelog).
3. Send "main": tip sha, gate result, dry-run output, crate list. Wait for "go".
4. `git push origin v0.8.4`. This starts: Release (cargo-dist), Release SBOM, Release KB,
   Browser WASM, Publish Crates; Release (Docker) follows the Release run.

## 8. What each workflow must produce (Fable verifies)

| Workflow | Produces | Check |
|---|---|---|
| `release.yml` plan/build | 6 dist apps × targets (x86_64/aarch64 × gnu/musl/darwin, windows-gnu), wasip2, installers, checksums | `plan` job is not empty (WEFT-593 guard); every `build-local-artifacts` leg green |
| `release.yml` host (env `weftos-cogs`) | `weftos-release.json` + `.sig` signed with the ed25519 release key; GitHub release, not prerelease, "Latest" | **G2**: a required reviewer approves the deployment; the owner approves stable (rcs were pre-approved, stable is not). Then `gh release view v0.8.4` shows ~89 assets incl. the signature |
| `publish-homebrew-formula` (first run with the App token `TAP_APP_ID`/`TAP_APP_PRIVATE_KEY`) | commit in `weave-logic-ai/homebrew-tap` for 0.8.4 | tap repo has the commit; `brew update && brew info weft` resolves 0.8.4 |
| `release-docker.yml` | multi-arch image `ghcr.io/weftos/weftos:0.8.4` and `:latest` (first push to the WeftOS org) | build legs green, post-publish smoke (`scripts/release/docker-smoke.sh`) green — it was red from 09-28 until `dec62889`; rc.16 was never cut, so 0.8.4 is the first tag to exercise it |
| `release-kb.yml`, `release-sbom.yml` | KB `.rvf` and SBOM assets on the release | both green (they now wait on the Release run, `0eff3101`) |
| `publish-crates.yml` | 40 crates at 0.8.4 / 0.1.1 on crates.io | run green; `curl crates.io/api/v1/crates/<name>/0.8.4` = 200 for each; `cargo install weftos --version 0.8.4 --dry-run`-style resolve of `clawft-kernel = "0.8.4"` in a scratch crate builds |
| `wasm-browser.yml` | browser WASM asset | green |

Also: `weaver update --check` on a node running rc.15 sees 0.8.4, verifies the signature and
does not call it a downgrade.

If any job fails: fix forward on `target-0.8.4`, never retag `v0.8.4` (a crate version that has
been published cannot be replaced; a second attempt is v0.8.5 or a re-run of the failed workflow
by dispatch, which the publish script supports).

## 9. Fast-forward master (Fable; **G3**)

After §8 is green and "main" confirms:

```
AGF_TARGET=_release ~/.claude/skills/agent-git-flow/scripts/release.sh status
~/.claude/skills/agent-git-flow/scripts/release.sh promote v0.8.4 --ff --approved "<the owner's words, relayed by main>"
```

The script refuses unless `master` is an ancestor of the tag (it is: master = v0.8.3) and pushes
with a lease on master's tip. This closes `975e420f` (Next.js/esbuild alerts) and most open
Dependabot alerts. No commit is ever made on master.

## 10. Post-release cleanup (Fable, no gate)

- Tag `f19dd0996` as `crates-0.8.3-published` (owner 2026-10-18), then delete
  `release/0.8.3-crates` on origin and the `crates-083` worktree.
- Board: `done` on `f896c4f7` (crates.io) with the run id and crate count; `done` on
  `975e420f` after the fast-forward; note the release on `1a919ae6`, `85f29f99`, `4366dc71`,
  `dec62889`, `0eff3101`.
- Memory notes: `weftos-release-state`, `crates-io-publishing`, `main-gets-every-release`.
- Remove merged lane worktrees only after their agents are idle (memory
  `dont-remove-agent-worktrees-early`).

## 11. Rollout handover to "main"

Fable does not run Terraform. The hand-over is a filled-in checklist, not a sketch:

1. **Artifacts**: download `weftos-release.json` and the Linux archives from the tag into a
   scratch dir, verify the `.sig` with the release pubkey PEM
   (`WEFTOS_RELEASE_PUBKEY_HEX` → DER prefix `302a300506032b6570032100`), record sha256s.
2. **Console tgz**: `scripts/build.sh manager-web` from the `v0.8.4` checkout →
   `~/weavelogic.terraform/artifacts/console-0.8.4.tgz`.
3. **Pins**: in `~/weavelogic.terraform` (worktree `tf-sansone-hub`, branch
   `tf/sansone-dev-hub`): 34 `rc.15` occurrences across `environments/main.tf` and
   `inventory/hosts.yaml` → `0.8.4`; `pg_weftos_release_json_sha256` and each archive sha;
   strip the held weftos-cogs entries for the apply and restore after.
   The local model produces the sed list from the grep; Fable checks it against the files.
4. **Plan/apply** with the 4 release targets (`pg_weftos_release`, `pg_shared_weftos_bin`,
   `pg_weftos_console`, `pg_weftos_gateway`), **members first** (the member nodes, and the
   Mac by hand from verified archives), then the primary node, then its project daemon
   (restarted as the project user with `systemctl --user restart weftos.service`).
5. **Watchdog units**: the pin swap does not regenerate systemd units. After the apply,
   regenerate by hand with `weaver service unit --kind systemd --out ... --force` for the
   primary node and the project daemon (with that project's `HOME`), `daemon-reload`,
   restart, confirm `WatchdogUSec=1min`. The exact hosts, users and paths are in the private
   Terraform inventory and the lead's memory notes, not here.
6. **Verify**: `weaver --version` on each node = 0.8.4, `weaver kernel liveness` healthy,
   dashboard heartbeats, mesh peers unchanged, `weaver update --check` reports current.
7. **Ventuno rehearsal** re-run (main's).

## 12. Local model lane: what it does and how it is checked

| Job | Tier | Input inlined | Check |
|---|---|---|---|
| J1 CHANGELOG 0.8.4 section | local model | rc.16 draft + 114-line log + lane summaries | Fable: bullet ↔ commit map, style, leak terms |
| J2 cut-plan status table | local model | the table + landing commit ids | Fable: each id in `git log` |
| J3 Terraform sed list | local model | grep of the 34 pin lines | Fable: applies in a dry run, diff reviewed |
| J4 crate list table for the report | local model | publish dry-run output + crates.io checks | Fable: counts and versions |
| J5 leak sweep of the diff | haiku (read-only, needs tool use) | file list | Fable reads hits |
| Version bump | codemod | file list | greps + gate check 18 |

Access: the Agent tool cannot run a subagent on a local model, and Ruflo's `agent_execute`
on the local provider fails before any model call (`policy-ledger-anchor-missing`, raised with
the `llm` agent), so jobs go through the servers' OpenAI-compatible chat API directly (a
30-line script, temperature 0.2, thinking off, output to a file that Fable diffs).
If a job fails twice on the local model it goes to haiku; three failures of a class means the
class is not workable locally and the report says so with the outputs.
