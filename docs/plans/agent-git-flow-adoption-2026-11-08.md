# Adopting agent-git-flow in WeftOS

Status: proposal, 2026-11-08. Nothing is adopted. The draft config is `.agent-git.env` at the
repo root of this branch (`wt/adr126-s2-hooks`); it does nothing until the owner merges it.
This plan follows the skill's `docs/ADOPTING.md`, sections "WeftOS example" and "Hooks"
(`~/.claude/skills/agent-git-flow/docs/ADOPTING.md`).

## What already exists

`config/agent-git.env` is already tracked (commit 19b8825f3). It sets the target pattern
`target-0.8.*`, the external worktree root, branch prefixes `wt/ feat/`, protected branches,
the Rust 1.95 pin, `AGF_MAIN_BRANCH=master` and `AGF_RELEASE_FF=1`. The skill's loader and
`scripts/diff-gate.sh` both read it. No root `.agent-git.env` exists in this branch or in
`~/weftos`. So the real change is small: add hooks, a fast check and a worktree cap.

The loader prefers a root `.agent-git.env` over `config/agent-git.env` and reads only one of
them. The draft therefore sources the config file first, then adds. The alternative is to put
the same lines straight into `config/agent-git.env` and skip the root file. That is cleaner and
needs no shadowing. Either works.

## What the skill replaces

| Today (by hand) | With the skill |
|---|---|
| `git worktree add` on the external disk, base picked by eye | `wt.sh new <name>`: fetches, cuts from the current target tip, refuses below 100 GB free or over the lane cap |
| Land merge written by hand, then pushed | `land.sh`: fetch, rebase, fast check, compare-and-swap push. Linear history, no merge commits, retries when another lane lands first |
| "Never commit to master" kept by memory | Lanes only land on the target. Master moves only through `release.sh promote` |
| "Stage by path, no `commit -a`" kept by memory | Not replaced. It is a rule for the shared main checkout and stays in CLAUDE.md |
| Store create and promote done by `worktree-store.sh new` and `land` wrappers | The same two steps run as hooks, so the wrappers are no longer in the path |
| Stale lanes noticed by accident | `wt.sh hygiene` reports stale, merged and dirty lanes and prunable branches |

## The config

Exactly what the draft contains beyond `config/agent-git.env`:

```sh
AGF_REMOTE=origin
AGF_WORKTREE_ROOT=/Volumes/ai-models/weftos-worktrees
AGF_MAX_WORKTREES=30
AGF_BRANCH_PREFIX="wt/ feat/"
AGF_FAST_CHECK='case "$AGF_AFFECTED" in DOCS*) exit 0;; *) scripts/build.sh diff-gate --base "$AGF_BASE";; esac'
AGF_HOOK_POST_NEW='scripts/worktree-store.sh store-create'
AGF_HOOK_POST_LAND='scripts/worktree-store.sh store-promote'
AGF_HOOKS_STRICT=0
```

- **Base branch.** The target stays a pattern. Today it resolves to `target-0.8.5` (checked by
  loading the draft). A pinned name would break at the next `release.sh roll`.
- **Two hooks, never with the wrappers.** `worktree-store.sh new` and `land` run `wt.sh` and
  `land.sh` themselves. Using them with the hooks would create and promote each store twice.
  Agents call `wt.sh` and `land.sh` directly. Do not set `WTS_SLUG` in the hook lines: it
  overrides the slug the script now derives from the branch.
- **Worktree cap.** 23 lanes exist today and the default cap is 8, so `wt.sh new` refuses
  ("23 lanes already (max 8)"). The draft sets 30. The better fix is `wt.sh hygiene` first.
- **PRE_LAND gate.** I did not use a hook for it. A hook only warns unless `AGF_HOOKS_STRICT=1`,
  and that flag is global: a missing store would then block landing too. The blocking place is
  `AGF_FAST_CHECK`, which fails a land with exit 4 and lands nothing.
  - *Diff gate* (draft choice): static, no compile. Measured 15 s wall in this worktree
    (0.4 s of CPU; the rest is git reading the external disk). The docs say about one second.
  - *`scripts/build.sh check`*: a real compile. Not measured. It conflicts with the owner rule
    of no full local gates, because CI is green-of-record.
  - `AGF_FULL_GATE` is left unset, so `verify.sh` is not used on the Mac.
- **Leak check.** `scripts/agents-leak-check.sh` exists. Add it to the fast check only if the
  owner wants every land to be leak-checked (see decision 2).

## What changes for agents and lanes

| Stop doing | Type instead |
|---|---|
| `git worktree add ...` | `wt.sh new <name>`, then `git log -1` must match the printed target tip |
| hand merge into the target, `git push` | `land.sh` from the lane (`--dry-run` first if unsure) |
| `worktree-store.sh new`, `worktree-store.sh land` | nothing: the hooks run |
| `git worktree remove`, `git branch -D` on lanes | `wt.sh clean`, `wt.sh branches` (dry run, then `--apply`) |

Agents keep committing by path inside their lane. `worktree-store.sh share`, `claim`, `follow`
and the rest are unchanged. Exit codes to know: 3 rebase conflict, 4 fast check failed, 5 lost
the race, 11 a hook failed (only under strict).

## Cost, measured once

One throwaway round trip in this worktree, local-only target and no fast check or hooks (a
scratch branch, removed afterwards; nothing pushed):

| Step | Wall time |
|---|---|
| `wt.sh new` (checkout onto the external disk) | 20.8 s |
| `land.sh` (rebase, local swap) | 0.3 s |

Not measured: hooks (they need a daemon with a store), a real push to origin, the cargo check.
`wt.sh new` printed three "no golden" nm-cache notices for the Node trees. The cache is optional.

## Rollout

1. Owner decides (below) and merges the draft. Until then agents keep the manual flow.
2. One lane first, a docs-only lane, on a Mac with the daemon up. Confirm: the store is created
   and promoted, the diff gate runs, and the push lands on the intended branch.
3. All Mac lanes, after `wt.sh hygiene` clears stale lanes so the cap can drop toward 8.
4. aepod later. It needs its own checkout, the same config, and the daemon reachable.
5. Tell lanes with one line in CLAUDE.md, as ADOPTING step 3 says. That line is not added here.

## Risks

- **Every land pushes to the public origin.** `land.sh` force-with-lease pushes `target-0.8.5`.
  Today work is pushed at checkpoints after a leak check, and Tapestry has a no-push rule.
  Local-only (`AGF_REMOTE=`) is not a safe workaround: it moves the local branch with
  `update-ref`, and `~/weftos` has that branch checked out with other sessions' uncommitted
  edits, so its index would go stale under them.
- **Hook needs a daemon with a store.** Default policy is warn and continue, exit 0. With no
  daemon or no `weaver` the store step is skipped with a warning, and a lane lands without a
  promoted store. Strict mode would make that fatal and is not recommended.
- **Lane cut before the script exists.** Hooks run from the lane. A lane based on a target tip
  without `scripts/worktree-store.sh` warns and skips the store. Land the S2 scripts first.
- **sccache and `target/` sharing.** `build.sh` does not use sccache, and no cache dir exists
  yet, so this plan does not enable it. Each lane has its own `target/` on the external disk,
  which was the earlier disk-fill cause. Cap by `wt.sh hygiene`, not by this change.
- **Release interplay.** `release.sh promote` opens a PR or fast-forwards master with explicit
  approval (`AGF_RELEASE_FF=1`). Lanes never touch master, so this does not collide with land.
  `release.sh roll` deletes the old target; the pattern follows it.
- **Shared checkout.** `wt.sh` run in `~/weftos` warns. Lanes never run there.

## Decisions for the owner

1. Adopt at all, and whether the config lives in a root `.agent-git.env` or in `config/agent-git.env`.
2. Is it acceptable that `land.sh` pushes the public target on every land, replacing the
   push-at-checkpoint rule? If not, lanes need a different landing step, for example a
   local-only target in a dedicated clone rather than `~/weftos`.
3. Fast check: diff gate only (draft), or also the compile check.
4. Worktree cap: 30 for now, or clean first and keep 8.
