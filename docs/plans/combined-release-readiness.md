# Combined RustFS + Tapestry release: readiness (2026-11-08)

Prepared by the docs lane for the owner pile of 2026-11-08 12:45. The owner chose one combined release,
later. This page lists what must land, in what order, what git says about conflicts, what the owner
still has to decide, and which tests have and have not been run. It releases nothing.

Updated 2026-11-08 14:31 (owner instruction): Tapestry Phase 1 is closed by the owner (rename steps 3 and 5 included; npm is `weftos-tapestry`, applied; the landed MDM change stands; the marks gate stays archived and its WeftOS rewrite comes later without keeping Phase 1 open). The Friday-leftovers deltas (`~/weftos-friday-plan-leftovers-20261108.md` section 4) are applied below, and section 4a lists Phase 2 and Phase 3 remaining work as line items with done / in progress / blocked-on-owner status.

Hard stops (section 5) apply. Every claim carries a pointer. Where a claim could not be checked,
it says "not verified from this Mac". Sources: `git` in `~/weftos` (git 2.54.0), the status page
`docs/plans/adr126-131-shared-filesystem-status-2026-11-08.md` (called "status page"), the next-steps
doc `docs/plans/mesh-store-next-steps-2026-11-08.md` (called "next-steps"), and
`/tmp/weftos-d1-status.md` (called "d1 notes", read only).

## 1. RustFS side: every branch that must land

Counts are `git rev-list --count target-0.8.5..<tip>` and `git diff --name-only target-0.8.5...<tip> | wc -l`.
Base for every Mac branch is `be2166eed` (`git merge-base target-0.8.5 <tip>`). The counts include
aepod commits the Mac branches were built on: every `wt/*` branch below contains
`aepod/feat/adr126-s2` be64b6014 and `aepod/feat/adr131-drive` 527ea4db (`git merge-base --is-ancestor`,
both yes). The "Mac-only" column is `git rev-list --count aepod/feat/adr126-s2..<tip>`.

### 1a. Mac branches, in the landing order fixed in status page section 2

| # | Branch | Tip now | Commits over target | Mac-only | Files |
|---|---|---|---|---|---|
| 1 | `wt/adr126-s2-mac` | 220952fe9 | 43 | 27 | 88 |
| 2 | `wt/adr126-s2-meta` | 8f4652c16 | 18 | 2 | 86 |
| 3 | `wt/adr126-s2-hooks` | af688ca60 | 23 | 7 | 85 |
| 4 | `wt/adr126-s3-drive` | a757fe2cc | 52 | 36 | 102 |
| 5 | `wt/adr126-s3-harden2` (contains `-s3-harden` 4082527e8 and `-s3-tombstone-gc` 565e0c8fc) | b018f71db | 60 | 44 | 110 |
| 6 | `wt/adr126-s1-names` (pending owner confirmation, section 3) | fafd0fa13 | 62 | 46 | 111 |
| 7 | `wt/adr126-e1-scoped-view` | 4455d1cdc | 56 | 40 | 112 |
| 8 | `wt/adr126-e1-view-verbs` (contains 7 and 5) | 068ea6723 | 70 | 54 | 123 |

Notes:
- Tips for rows 6 and 8 are newer than the status page (6736b94eb and 22431e4be). The status page
  section 8 already warns that its tips go stale; use the tips above (`git log --oneline -1 <branch>`).
- Row 2 is docs and bench scripts only (status page section 2).
- Worktrees are under `/Volumes/ai-models/weftos-worktrees/` (`git worktree list`). Not pushed: no `origin` copy.
- Row 8's tip adds `deny` to `view.json` (068ea6723), asked for by the Tapestry item 10 launcher.

### 1b. `feat/adr131-d1` (weftos-d1 lane), lands last

| Item | Value | Pointer |
|---|---|---|
| Tip | 25378b449, refresh_upper and recover_upper tests against loopback RustFS | `git log --oneline -1` |
| Base and size | `be2166eed`; 15 commits over target-0.8.5; 18 files, 7,750 insertions | `git rev-list --count`, `git diff --stat target-0.8.5...feat/adr131-d1 \| tail -3` |
| Files touched | 16 under `tools/seed-drive-d1/`, plus `docs/adr/adr-131-seed-drive.md` and `docs/research/adr131-d2-linux-drive-design.md` | `git diff --name-only` |
| Named commits | c413038a6 xattr crate for the overlay probe; e765ee8cb `SEEDDRIVE_REDO` default strict plus kill point 5; d2351c00b docs for the strict default. All three exist and are on the branch | `git cat-file -t`, `git log` |
| Also on the branch | fd5b4f9e6 (Cargo.lock records optional `xattr`, "not diff-gate approved") | `git log` |
| Untracked in worktree | `tools/seed-drive-d1/.claude-flow/` in `/Volumes/ai-models/weftos-worktrees/adr131-d1`. Must not be committed | `git status --short` in that worktree |
| Diff gate | Rejected for new deps (d1 notes, 2026-11-08 progress). Gate surface later shrunk: unsafe only in `src/devtools.rs` (2 lines), `xattr` optional Linux plus devtools. No approvals entry added | d1 notes, "Gate surface shrunk", "xattr decision", "1220 pile" |
| Merge-order option | `opt/adr131-d1-redo-strict` (66ce790e4) is redundant, left in place, not merged | d1 notes 1143 |

### 1c. aepod branches (aepod-xpc): what the local object database holds

The task brief expected these to be absent. They are partly present. A remote-tracking set
`aepod/*` was fetched read-only on 2026-11-08 (next-steps section 1; `git branch -r | grep aepod`).
The owner-named hashes are not present. Column "verified from this Mac: no" means the tests and
state on aepod were not seen from here.

| Slice | Owner-named tip | `git cat-file -t` | Local `aepod/*` ref now | Status page tip | Verified from this Mac |
|---|---|---|---|---|---|
| S1 `feat/adr126-s1` | 923d6b41 | not a valid object | `aepod/feat/adr126-s1` = 53d007a41 (8 ahead of target) | ccd11d361 (exists locally) | no |
| S2 `feat/adr126-s2` | not named | n/a | be64b6014 (16 ahead) | be64b6014 | no, tests are aepod's |
| S3 `feat/adr126-s3` | not named | n/a | 7d59f9618 (9 ahead) | 0b67861d (exists locally) | no |
| S4 `feat/adr126-s4` | not named | n/a | f84191571 (4 ahead) | f84191571 | no |
| S5 `feat/adr126-s5` | e5d6b9ac | not a valid object | e11d07b6b (7 ahead) | e11d07b6b | no |
| `integ/adr126-s1-s4` | 76a26504 and e0447e0e | both not valid objects | 28c2a65e1 (10 ahead) | 28c2a65e1 | no |

Which of 76a26504 and e0447e0e is newer: unknown from here, neither is in the object database.
The local refs for S1, S3 and the integration branch differ from both the status page and the owner's
hashes, so aepod has moved since the fetch. Re-fetch from aepod before the real merge.
Commit 4eda02ced (S1 approvals and project-store commit) exists locally and is contained in every
`wt/*` branch (`git branch --contains`) but not in `target-0.8.5`.

### 1d. photo-gallery (PG)

- `git branch -a --list 'pg/*'` is empty. No `pg/*` branch is visible from here.
- Status page section 2, row "photo-gallery": checkout `/data1/home/mathew/weftos` at 527ea4db
  (`git cat-file -t 527ea4db` is a commit, the same as `aepod/feat/adr131-drive`).
- RustFS 1.0.4 live on PG `:9700`; the upgrade to a build with upstream fixes #9067 and #9073 is
  pending (card `3f30aaf9`, not deployed). JuiceFS mounts, Redis `:9710`, meshd `:9720` still up.
- Not verified from this Mac: PG's actual state, version or checkout (status page section 7).

### 1e. mesh-mock (outside the workspace)

| Branch | Tip | Pointer |
|---|---|---|
| `main` | 0a05fe7 | `git -C /Volumes/ai-models/research/weftos-mesh-mock log --oneline -1 main` |
| `feat/manifest-v2` | d9cb33a (merge of `feat/metadata-index`) | same, worktree `weftos-mesh-mock-wt-manifest` |

The main checkout shows one untracked path, `tools/seed-drive-d1/.claude-flow/`. The seed cone (SC)
binary is pinned in Terraform `gcp/seed-store/variables.tf` (`sc_gcsmesh_sha256`, next-steps 3e).
The status page records the gcsmesh pin as unchanged. Verdict: no SC rebuild unless the owner wants
manifest v2 live. Manifest v2 flags default off; signing the `WMR2` chunk-table digest is an open
owner call (status page section 4). The three path deps (`weftos-tree-spike`, `weftos-integrator-spike`,
`weftos-pq-sign`) are not git repos, so a build is not yet reproducible from a commit (next-steps 3e).

## 2. Merge order and git-visible conflicts

Intended order on `target-0.8.5`: aepod S1 first (owner approval), the Mac branches in order, then
`feat/adr131-d1`. aepod S1 to S5 could only be simulated as far as local refs go, and this section
does not include them. Method: `git merge-tree --write-tree --name-only --messages`. After each step a
synthetic commit was built with `git commit-tree` (objects only, no refs), so each step is "accumulated
result so far plus the next branch". Where a step conflicted, the marker tree was kept and the run went on,
so later conflicts in the same file can be knock-on effects.

### 2a. Accumulated, starting from target-0.8.5 (git 2.54.0)

| Step | Merge | Conflicts | Files |
|---|---|---|---|
| 1 | target + `wt/adr126-s2-mac` | none | auto-merged: `Cargo.lock`, `capability.rs`, `daemon.rs`, `lib.rs`, ADR-126 text |
| 2 | + `wt/adr126-s2-meta` | none | n/a |
| 3 | + `wt/adr126-s2-hooks` | 1 | `scripts/tests/worktree-store.test.sh` (ADR-126 text and `scripts/worktree-store.sh` auto-merge) |
| 4 | + `wt/adr126-s3-drive` | none | `Cargo.lock`, `daemon.rs`, `lib.rs` auto-merge |
| 5 | + `wt/adr126-s3-harden2` | none | n/a |
| 6 | + `wt/adr126-s1-names` | none | ADR-126 text auto-merges |
| 7 | + `wt/adr126-e1-scoped-view` | 2 | `crates/weftos-store/src/drive/mod.rs`; add/add `docs/research/adr126-e1-scoped-view-handoff-weftos-pg-2026-11-08.md` |
| 8 | + `wt/adr126-e1-view-verbs` | 1 | `drive/mod.rs` (knock-on from step 7's marker tree) |
| 9 | + `feat/adr131-d1` | none | `docs/adr/adr-131-seed-drive.md` auto-merges |

### 2b. Specific pairs asked for

| Pair | Conflicts | Files |
|---|---|---|
| `wt/adr126-e1-view-verbs` + `feat/adr131-d1` | none | n/a |
| `wt/adr126-e1-view-verbs` + `wt/adr126-s1-names` | none | n/a |
| `wt/adr126-s3-drive` + `wt/adr126-s2-hooks` | 1 | `scripts/tests/worktree-store.test.sh` only. The status page predicted `scripts/worktree-store.sh` and ADR text too, and git merged those cleanly |
| `wt/adr126-e1-view-verbs` + `wt/adr126-s2-hooks` | 1 | `scripts/tests/worktree-store.test.sh` |
| `target-0.8.5` + `feat/adr131-d1` | none | n/a |

Reading: only three files are real textual conflicts. The `drive/mod.rs` conflict in step 7 is the one
the status page predicted (view module line and `SkipWhy::Denied`), and it does not occur when
`wt/adr126-e1-view-verbs` merges as a whole (its own history already resolved it, pair 1 of this table).
The handoff add/add in step 7 exists because that doc was copied to `target-0.8.5` and also lives on the
branch. Merging `wt/adr126-e1-view-verbs` directly instead of `-e1-scoped-view` first avoids both.
Textual merges that report clean do not prove the result builds. Not run: any build on a merged tip.

### 2c. Known conflicts git does not show

| Item | What it is | Pointer |
|---|---|---|
| `config/diff-gate-approvals.toml`, aepod S1 commit 4eda02ced | It adds 22 approvals lines (owner approvals for the RustFS transport, 2026-11-07). The Mac branches already contain it and add 22 more (44 lines over target in `wt/adr126-s2-mac`). If aepod S1 lands first, git sees the same commit and should not conflict. If the approvals are re-applied by a rebased S1 copy, expect a conflict. Not simulated, aepod S1 is a different tip locally | `git show --stat 4eda02ced -- config`; `git diff --stat target-0.8.5...wt/adr126-s2-mac -- config`; next-steps section 1 |
| `drive/mod.rs` across the store branches | Resolved by hand inside `wt/adr126-s3-harden2` (Options and tests.rs) and inside `-e1-view-verbs` | status page section 3, rows "Drive branches merged" and "verbs" |
| ADR-126 text | Edited on `-s2-hooks` (section 4a and still-open list, fc7e0c0e5) and on `-s1-names` (section 3 bucket table, `node-<id>`). Git auto-merges, but a reader must check the two edits agree: grant signing wording, archive retention answer, bucket naming | `git merge-tree` step 3 and 6; status page section 9 |
| ADR-131 text | Edited on `feat/adr131-d1` (D1 status, D1-1, D1-2 decisions) and on `target-0.8.5` this morning (Q5, Q8, Q9 lines). Auto-merges (step 9). The two must agree on Q5 and Q8 answered, Q9 noted owner-side | status page section 8 |
| aepod S5 carries a copy of S1's first commit | f89d3608a re-applies a8f123e02, so S5 must be rebased on landed S1 | next-steps section 1 |

## 3. Open owner calls that block release

No approval entries were added by this lane.
Diff-gate approvals are owner-only (CLAUDE.md, status page section 6); agents never edit
`config/diff-gate-approvals.toml`.

| Call | Where asked | What it blocks |
|---|---|---|
| Diff gate `xattr` (optional, Linux plus devtools). Owner chose the crate 2026-11-08, no approvals entry exists | d1 notes, 1138; status page section 4 last rows | Merging `tools/seed-drive-d1`; opaque-directory detection in the scoped view |
| Diff gate `libc` (optional, devtools only, 2 unsafe lines in `src/devtools.rs`) | d1 notes, 2026-11-08 gate surface | Merging `tools/seed-drive-d1` |
| Diff gate `notify` for D1. The Mac drive's `drive-notify` feature uses the existing workspace `notify` and the gate did not count it as new | next-steps 9a; d1 notes (new-dep list) | D1 crate only; the Mac drive library is not blocked by it |
| Diff gate `clap`, `serde`, `serde_json`, `ed25519-dalek` (D1 crate, new deps) | d1 notes, the 04:40 and gate-surface entries | Merging the D1 crate |
| `rand_core`, `reflink-copy` | d1 notes: dropped from the crate, no entry needed | Nothing now; confirm after any later re-add |
| Diff gate mesh-mock path dependency (cannot be dev-only; resolves only on this Mac) | d1 notes, gate-surface entry | Merging the D1 crate; reproducible build of it |
| Real-SIGKILL crash test in 7f4eac450 (process spawning; gate rejects it) | status page section 4 and 9; `wt/adr126-s3-harden2` doc | Restoring the real-kill form; in-process form is what ships otherwise |
| `gcloud auth login` on the Mac: resolved, not a call any more. The d1 notes record login working at 13:39 on 2026-11-08 and a read-only cold join (5,914 files, 107 MB, correctness only). What remains for the D1 real-chain refresh is a signing path for a `drive-test/` chain, and the join report's `sig_verified=false` needs a look | d1 notes 13:39; Friday-leftovers report section 4 item 5 | D1 real-chain refresh evidence |
| A quiet 30 minute window on the Mac | same | D1 and D2 timings (kill-criterion numbers); none exist |
| Bucket rename `n-<id>` to `node-<id>` and the pinned bucket creator on `wt/adr126-s1-names`: confirm, drop or adapt | status page section 4; `docs/research/adr126-s1-names-2026-11-08.md` | Row 6 of the merge order; aepod s1 merge |
| PG `n-<id>` bucket migration if the rename is kept | status page section 9 | A daemon on that code would orphan the old PG data |
| Structured `{code, message}` view errors | status page section 4. Tapestry lead declined it for now | Nothing (later; owner call) |
| PG RustFS upgrade to the audited build (#9067, #9073) | card `3f30aaf9`; status page section 4 | Audit coverage; the parser fix against a real record |
| Lift the store-capable weaver install stop (Mac, aepod, PG). A release implies lifting it on the release hosts | next-steps section 3; status page section 4 | Any live store use, S1 live use, S5 live |
| Land order and approval: S1 first, then S2 and S5 rebased, then S3 and S4 | next-steps section 1; status page section 4 | Everything needing the store crate in the workspace |
| Phase 3 decisions D1 to D6 unanswered (Tapestry) | Tapestry lead, message 2026-11-08 PM; memo `docs/tapestry/phase3-decisions.md` on `tapestry/p3-decision-memo` | Tapestry hub work; D5 and D6 exploratory commits must not land |
| Engine-shape option B not applied (Tapestry) | same message | Tapestry integration shape |
| R11 release steps: secrets, environment, push, tag (Tapestry) | same message | `tapestry/p2-r11-release` cb216887 stays HELD |
| Diff-gate approvals for new unsafe and process-spawn code (Tapestry): bwrap backend raw Landlock and seccomp syscalls; ProcessRunner's weaver spawn | same message | `tapestry/p3-scoped-view` (item 10) |
| Sandbox calls C3 (allow-list profile contents), C4 (helper binary for Landlock inside bwrap, `noexec` on `/work`), C1 (how code gets to PG; archive in `~/tapestry-pg-linux-sandbox-20261108/`) (Tapestry) | same message | Linux run on PG, item 9 |
| Our `{code, message}` error payload, owner nod only if wanted (Tapestry) | same message; see the structured-errors row above | Nothing |
| R8 `project_required` gap (Tapestry): a `--profile user` daemon refuses `chain.append`, so the signed session journal records nothing on a user-profile daemon; it works only on a profile-less daemon or a project kernel. Owner decides whether the release accepts that or the daemon must accept journal appends on the user profile | `docs/tapestry/p2-r8-r9-live-checks.md` 4adc6cee | The R8 signed-journal claim on user-profile daemons |
| G0 review chain for `fix/overlay-gate-check-detailed` (WeftOS): the fix closes a silent fail-open in `governance.check` on project kernels and must go through the G0 review and sign-off before merge | same write-up | R9 correctness on project kernels; must-land pending row |
| Who supplies a Tapestry session's `ViewSpec` (Tapestry) | same message; alignment note gap 5 | Item 10 end to end |
| Where the release suite runs: CI on aepod is not authoritative (ADR-127 Proposed; only the manual Incus/WSL prototype passed nine checks, CI disk unformatted), and the owner rule forbids full suites on the Mac. Owner picks: a one-time Mac exception, or the prototype host | ADR-127 uncommitted edit; `docs/deployment/ci-wsl-bootstrap-2026-10-30.md`; Friday-leftovers report section 4 item 7 | The full-suite release gate (section 4 last rows) |
| Per-release Tapestry gates as CI or hand-run: the egress recorder and runner live in `~/tapestry-trial` (`run-step-tapestry.py`, `egress/`), not in the fork `scripts/`; only `strings-scan.sh`, `check-modifications.sh`, `check-catalog-models.sh` are committed. Gates (a) to (h) were run by hand on the 1.0.45 gate-only build | `docs/tapestry/p3-exits-fgh-and-status.md` on `tapestry/p3-closeout-docs` 31ba29ba ("Where the egress runner lives") | Whether a release can ship on hand-run gates; no Tapestry CI workflow exists except the held R11 one |
| Rotate `GROK_API_KEY` (owner action, not a decision): the Phase 1 gates lane printed it into its own session transcript under `~/.claude/projects/`; not in any log or doc (checked by value match) | `docs/tapestry/p3-closeout-handoff.md` item 7 on `tapestry/p3-closeout-docs` | Any release that ships `weftos-hard` through the gateway role |
| Commit the owner's uncommitted `docs/adr/adr-127-weftos-orchestrated-ci.md` and `adr-129-upstream-rvf-and-real-ml-dsa.md` edits before the release (status text only; ADR-129 now reads Accepted and cites commits already on `target-0.8.5`) | Friday-leftovers report section 4 item 8 | Record consistency only; no code dependency |
| R5 in-tree WeftOS agent and skill bundle, Q13 one-time `~/.grok` copy, and the R16 Phase 1 extension points (participant identity on journal events, `SessionChannel` trait, embedded-surface transcript entry) are all **not done** on ef376c8d. Phase 1 is closed by the owner, so these ride as Phase 2 or Phase 3 backlog unless the owner says this release needs them | `docs/tapestry/p3-exits-fgh-and-status.md` 31ba29ba ("R5, Q13, R16 against this build") | Nothing blocks on them today; the owner decides if they ride this release |
| Others still open (not release gates alone): ADR-126 Q3; ADR-131 Q1 to 4, 6, 7, 9; `WMR2` digest signing; mesh-mock remote and path-dep versioning; tombstone retention R; spike teardown; snapshot cadence | status page sections 4 and 9 | See those sections |

## 4. Targeted test slices already run, and what the release still needs

Command form is the `scripts/build.sh` wrapper. Counts are from the sources; I did not rerun any of them.
"Diff-gate base" is the `--base` used where recorded; "n/r" means not recorded in the sources I read.

| Lane | Slice run (counts) | Live RustFS loopback | Clippy | Diff-gate base | Last run |
|---|---|---|---|---|---|
| `s2-mac` | `build.sh test weftos-store --features rustfs`: 114 passed; `build.sh test clawft-weave --filter store`: 52; real-RustFS ignored 11 of 11 | yes (RustFS 1.0.4, loopback) | clean (evidence doc, second pass) | n/r | 2026-11-08 |
| `s2-meta` | bench under `scripts/bench/`, no crate code | yes (loopback; PG round trip extrapolated) | n/a | n/r | 2026-11-08 |
| `s3-drive` | weftos-store rustfs 141 passed; clawft-weave store 61 | yes, 1 live | clean (132-test pass); later n/r | `diff-gate.sh --worktree` ok | 2026-11-08 |
| `s3-harden`, `-harden2`, `-tombstone-gc` | weftos-store rustfs 141 to 148 to 153 to 157; merged `s3-harden2` 162 passed, 21 ignored | per branch: 7 plus 9 live scenarios, 1 tombstone cycle; **not rerun on the merge** | clean on the merge | `--base 4082527e8`, 13 files, 0 approved | 2026-11-08 |
| `s1-names` | weftos-store rustfs 162 to 166, 21 ignored; clawft-weave untouched | no | clean | `--base b018f71db`, 9 files, 0 approved | 2026-11-08 |
| `e1-scoped-view` | weftos-store rustfs 141 to 164 (23 view unit tests); clawft-weave store 61 | yes, 1 live cycle | clean | `diff-gate --worktree` | 2026-11-08 |
| `e1-view-verbs` | weftos-store rustfs 185 passed on merge; clawft-weave store 61 to 76; 1 live handler-path cycle | yes, 1 | not recorded for the 068ea6723 tip | n/r | 2026-11-08 |
| `feat/adr131-d1` | 91 targeted tests (89 to 91 with the D2 tests); `bucket_kill4.py` kill points 4 and 5 and `refresh_upper`, `recover_upper` on a throwaway RustFS through the `gcs2s3.py` facade; 19 overlay tests on `weftos-arm64` with `--features devtools` | yes, via facade; functional only, no timings | n/r | the gate **rejects** the crate (section 3) | 2026-11-08 12:20 pile |
| mesh-mock `feat/manifest-v2` | 45 tests at last count in the plan, not rerun | no | n/r | n/a (outside the workspace) | n/r |

Pointers: status page section 3, next-steps section 7, d1 notes, and, on the branches,
`docs/research/adr126-s2-evidence-macos-2026-11-08.md` (`wt/adr126-s2-mac`) and
`docs/research/adr126-s3-materialise-watch-2026-11-08.md` (`wt/adr126-s3-harden2`).
All live runs were noisy (load 3 to 13), so no timing from them counts as a design number.

### What the release still needs. All of this is **not run**.

| Item | State | Pointer |
|---|---|---|
| Full workspace suite, `scripts/build.sh gate` | **not run**, release only. Not to be run now | CLAUDE.md; status page section 6 |
| Live tests on the merged final tip | **not run**. Mac live drive and view tests ran per branch, not on the merge | status page section 3, "Drive branches merged" |
| aepod S1 done-when on a throwaway RustFS | **not run** on a merged tip; aepod ran it on its branch, not seen from here | next-steps section 1 |
| Two-machine S2 drill (aepod plus Mac on one RustFS) | **not run** | next-steps 7, step 6 |
| Windows native bucket load | **not run**, never tested | next-steps 7, step 7 |
| D1 and D2 kill-criterion timings | **not run**, none exist. `gcloud auth login` works since 13:39 (d1 notes); blocked on a quiet 30 minute window and a signing path for a `drive-test/` chain; `sig_verified=false` in the join report needs a look | d1 notes; status page section 3 D1 row |
| PG end to end for the scoped view (weftos-pg, Linux bwrap side, five e2e checks on `weftos-ci`) | **not started** | status page section 9 |
| `weaver store drive` and `view` against a live daemon | **not run**. The no-install rule stands, so this happens after the release lifts the store-capable weaver stop on the release hosts, not before (the status page section 9 and this page now agree) | status page section 9; owner install stop |
| S3 on a real edge node; S4 wired into `release.sh`; S5 as a second RustFS on PG | **not run** | next-steps 7, steps 9, 10, 13 |
| Tapestry: 16 Linux sandbox tests on PG | **done** 2026-11-08 PM: items 9+10 pass on photo-gallery, 104/104, 0 skips, all 16 Linux-only tests ok, no EPERM (commit eee49baf on `tapestry/p3-scoped-view`; `docs/tapestry/p3-linux-sandbox-pg-results-2026-11-08.md`; not verified from this Mac) | Tapestry lead, message 2026-11-08 13:12 |
| Tapestry: loopback onboarding e2e | **done** 2026-11-08 PM, green in both probe roles (Tapestry lead delta; evidence `docs/tapestry/p3-workspace-loopback.md` on `tapestry/p3-item4-onboarding` and logs in `~/tapestry-p3-onboarding-logs-20261108/`; not verified from this Mac) | same |
| Tapestry: Phase 1 exits (a), (b), (c) on the final integration build | **done** 2026-11-08 PM, all pass on `tapestry 1.0.45 (ef376c8d0250)`, a gate-only build, not installed: (c) strings scan 0 unexplained (x.ai 356: 350 ACP-exempt, 6 allowlisted; grok.com 1 exempt; mixpanel, sentry, googleapis 0); (a) no-key default session rc 0 with and without weft on PATH, sockets only 127.0.0.1:18181; (b) session plus subagent rc 0, 22 connects (9 loopback 18181, 13 unix kernel.sock R6 roster probe under the stand-in HOME, no daemon), 0 DNS, 0 non-loopback. Doc `docs/tapestry/p3-phase1-gates-rerun.md` on `tapestry/p3-closeout-docs` c1f4cec3; not verified from this Mac | Tapestry lead, delta 2026-11-08 PM |
| Tapestry: Phase 1 exits (f), (g), (h) on the final integration build | **done** 2026-11-08, all pass on `tapestry 1.0.45 (ef376c8d0250)` (same gate-only binary, sha256 5a7a4ae2...): (f) 0 canaries from a stand-in HOME full of Claude, Codex and `~/.grok` material, positive control on the phase0 binary saw 6; (g) 7 chat completions all naming the one catalog model, 0 `grok-*`, sockets only 127.0.0.1:18999; (h) `weftos-hard` completed through the gateway role with sockets only 127.0.0.1:18181 and :18182, 0 non-loopback rows for Tapestry, unset key names the variable (502 retried 7 times in 120 s, not terminal; the Tapestry lead calls the minutes-long retry a known robustness gap). Caveats recorded there: (g) static half `check-catalog-models.sh` not rerun; (h) passed with prompt steering (Grok chains shell tails, run 1 ended on the headless permission refusal); macOS arm64 only | `docs/tapestry/p3-exits-fgh-and-status.md` on `tapestry/p3-closeout-docs` 31ba29ba; not verified from this Mac beyond reading the doc |
| Tapestry: Phase 1 exit (d), xAI marks grep gate | **closed by the owner 2026-11-08 14:31**: the marks gate stays archived (`.archive/marks-gate-draft/`, 5b0e8b69); the WeftOS rewrite (card e89d2ba6) comes later and does not keep Phase 1 open. Rename grep proofs are in `docs/tapestry/rename-evidence.md` | owner instruction 14:31 |
| Tapestry: Phase 1 exit (e), existing tests pass or are deliberately rewritten | per-crate slices only (lane E triage 35aaa0e9; table above); the full Tapestry suite is release-only | `docs/handoff-tapestry-phase2.md`; owner rule |
| Tapestry: R8 `tapestry sessions verify --require-signed` end to end, and R9 policy layer against a real daemon (not a fake socket); G3 cross-node rule order review | **not run**; type-checked and unit-tested only. Add to the release suite | `docs/tapestry/handoff-phase2.md` "Not established" |
| WeftOS: the two daemon-boot tests under nextest on the real branch | **not reproduced; timeout raised; confirm under the release suite.** On ee9d48efe `kernel_restart_e2e` and `legacy_adoption_boot` pass under nextest 5 of 5 and under cargo test; full clawft-weave nextest 1531/1532, the one failure `project_principal_population` also fails on the baseline; added CPU load did not make them fail. Likely cause: the spawned 169 MB debug weaver stalls in `_dyld_start` for 8 to 35 s before main (seen with `sample`) and both tests hard-coded a 60 s boot wait, exceeded under the rehearsal's 5-crate parallel run. Fix in tests only: `fix/daemon-boot-nextest` 93b52ca22 (BOOT_WAIT 300 s). Not established that the failure is gone under that load; rerun the 5-crate nextest slice at release | Tapestry lead, delta 2026-11-08 PM |
| Tapestry and WeftOS full suites at release | **not run**, release only | same |

## 4a. Tapestry phases: Phase 1 closed; Phase 2 and Phase 3 remaining work

Phase 1 (detach from xAI) is **closed by the owner on 2026-11-08 at 14:31**, including rename step 3 (npm `weftos-tapestry`, applied) and step 5 (the marks gate, archived; the WeftOS rewrite is later and does not keep Phase 1 open). The landed MDM change (`ai.weavelogic.weftos.tapestry`) stands and is not reverted. Exits (a), (b), (c), (f), (g), (h) pass on the final integration build ef376c8d (section 4); (d) is closed by the owner; (e) is per-crate slices now and the full suite at release. Three Phase 1 scope items are not done and are carried as backlog, not as Phase 1 blockers: R5 bundle, Q13 one-time copy, R16 extension points (section 3 row).

Status words: **done** (merged on `tapestry/p2-integration` ef376c8d or on `target-0.8.5`), **built** (on a branch, lands in the order above), **in progress**, **blocked on owner** (the call is named), **not started**, **deferred**.

| Phase | Item | Status | Pointer |
|---|---|---|---|
| 2 | R3 gateway default, R6 step 1 synthetic plugin, R7 built-in weftos MCP, R8 signed session journal, R9 policy layer (`governance.check`, `weft-policy`), R11 verify code | **done** on `p2-integration` ef376c8d | `docs/handoff-tapestry-phase2.md` |
| 2 | Phase 2 close-out fixes: test-home leak, flaky shell MCP test, Windows MinGit path, remote `sandbox_mode` layer removed, built-in weft without update check, release file names | **done** (merged) | `tapestry/p2-closeout` b16bf59a and siblings |
| 2 | R8 verify CLI end to end against a real daemon; coverage across a real daemon restart | **done** live 2026-11-08 PM (row below); `project_required` gap is an owner call | `p2-r8-r9-live-checks.md` |
| 2 | R9 policy layer against a live daemon; rule-set loading and G3 cross-node rule order review | **run** live 2026-11-08 PM, two bugs found (row below), fixes pending; G3 cross-node rule order still not reviewed | `p2-r8-r9-live-checks.md` |
| 2 | R11 signed-update release workflow: run once, Windows and arm builds, MinGit payload in CI | **blocked on owner**: R11 release steps (environment, two key secrets, push, tag, sign-job approval) and whether `tapestry/p2-r11-release` cb216887 merges. HELD | `docs/tapestry/r11-signed-updates.md`; section 3 |
| 2 | Engine shape option B (in-process default plus weaver-supervised leader) and review decisions 2 to 6 | **blocked on owner**: option B not applied | `docs/grok-build-fork/engine-shape-review.md`; `p2-engine-shape-memo` ef8b62cb |
| 2 | K0 always-Allow stub deprecation | **done** on `target-0.8.5` (bc86bcd70) | section "WeftOS-side commits" |
| 2 | Board cards for chain follow-ups, update-crate fixtures, Windows installer CI | **deferred**: owner said not to file cards yet | `handoff-phase2.md` |
| 3 | Items 1 to 3: R6 step 2 live roster (`DefinitionProvider`, directory feed W1 with etag, WeftOS provider and `AgentLocation::Weftos`) | **done** (`p3-r6-live-roster` merged into `p2-integration`; W1 031b107cb on `target-0.8.5`); roster liveness as poll with etag (D4 as built) | Tapestry lead 2026-11-08; `p3-decision-memo` |
| 3 | Item 11: session verify by range read (W6 `chain.get_range`) | **done** (`p3-verify-range` merged; b00ac6f95 on `target-0.8.5`) | same |
| 3 | Item 4: workspace server onboarding on one desktop against a loopback hub | **built**, green in both probe roles; lands as `tapestry/p3-item4-onboarding` 01cfe6fa | `docs/tapestry/p3-workspace-loopback.md` |
| 3 | Item 9: bwrap/Landlock backend, process, net and syscall half | **built**; Linux tests 104/104 on photo-gallery, 0 EPERM; lands in `tapestry/p3-scoped-view` ab572851 (tip eee49baf) | `docs/tapestry/p3-linux-sandbox-pg-results-2026-11-08.md` |
| 3 | Item 10: Linux filesystem half over the store scoped view (`weaver store view`) | **built** against recorded replies, **blocked on owner** for landing: diff-gate approvals (raw Landlock and seccomp syscalls, `pre_exec`, `ProcessRunner` weaver spawn), sandbox calls C1 (code to PG), C3 (allow-list profile), C4 (helper binary, `noexec` on `/work`), who supplies the `ViewSpec`; also needs `wt/adr126-e1-view-verbs` landed first. Never run against a real `weaver store view` daemon; overlay mode never run | `p3-closeout-handoff.md`; alignment note; section 3 |
| 3 | Item 5: hub server on `xai-computer-hub-core` with a WeftOS-minted credential (W5); exit E1 | **blocked on owner**: Phase 3 D1. Backup plan `feat/weft-computer-hub` (118831eef PROBE_OK) stays unmerged while D1 is open, work continues on it (owner 13:12) | `p3-decision-memo` d4808a46; section "Must land" BACKUP row |
| 3 | Item 6: hub client wiring (config-only hub URL, `host_kind` from the minter) | **not started**; depends on item 5 | `phase3-prep.md` section 2 |
| 3 | Item 7: computer-use tool pack behind the R9 gate (W3 computer workload kind, W4 which desktops) | **blocked on owner**: D2 (and D1); not started | same; cards f578103a, 26d7dbf1 |
| 3 | Item 8: shared wire crate adoption (W2) | **deferred** by the decision memo (D3 open); wire-shape fixtures landed instead (9492ec881) | `p3-decision-memo` D3; card 017ffa84 |
| 3 | Exploratory D5/D6 hub work | **do not land**: `tapestry/p3-hub-passkey` 847521bf and its backup | Tapestry lead |
| 3 | Test-only loopback hub crate | **built**, 20/20; lands as `tapestry/p3-test-hub` 17c43e99, in no shipped binary | Tapestry lead |
| 3 | Exit E1 (session binds to a remote workspace through the WeftOS hub) | **not met**; waits on D1 (item 5) | `phase3-prep.md` section 1 |
| 3 | Exit E2 (mesh agents live in the roster) | **done** | same |
| 3 | Exit E3 (Linux sessions under bwrap/Landlock with the store view as filesystem) | **built, not landed**; items 9 and 10 above | same |
| 3 | aarch64 `table_matches_libc` on the OrbStack `weftos-arm64` VM (kernel 7.2, toolchain 1.95) | **done** 2026-11-08 PM (G7), per the Tapestry lead; not verified from this Mac | Tapestry lead delta 14:4x |
| 2 | R11 target checks on ef376c8d: aarch64-apple-darwin and x86_64-apple-darwin pass (real checks); x86_64 and aarch64 Linux pass (stand-in type-check); x86_64-pc-windows-msvc **fails on ef376c8d** (`weft-journal/src/rpc.rs:6` imports a Unix socket with no cfg gate); **fixed on `tapestry/p2-journal-windows` 27bc741c** (must-land row e) | **done** as a stand-in type-check; a real Windows host and linking are not run. Write-up 4b9f89d8 on `tapestry/p3-closeout-docs` | Tapestry lead delta 2026-11-08 PM |
| 2 | R8 live check against an isolated test weaver (built from cebdcc35a, debug, not store-capable, per the Tapestry lead) | **done, passes**: `--require-signed` reports 5 before a checkpoint, 0 after one, 3 LOCAL ONLY with the daemon down, 0 after a daemon restart; `chain.get_range` used on every verify; tampering gives DIGEST MISMATCH (exit 2); no inet sockets from the test daemon, Tapestry only 127.0.0.1:18181. **Gap, owner call:** a `--profile user` daemon refuses `chain.append` with `project_required`, so R8 journals nothing there; it works only against a profile-less daemon or a project kernel | `docs/tapestry/p2-r8-r9-live-checks.md`, 4adc6cee on `tapestry/p3-closeout-docs`; not verified from this Mac |
| 2 | R9 live check, same daemon | **run; two bugs found, fixes in progress**: (1) WeftOS: on a project kernel `governance.check` answers `no_governance_engine` because `OverlayGate` lacks `check_detailed`, a silent fail-open where an overlay deny is ignored; fix on WeftOS `fix/overlay-gate-check-detailed` 541a9386b, green (clawft-kernel 249/249), needs the G0 review chain before merge. (2) Tapestry: `--always-approve` overrides a governance escalate, so the forced ask is not forced headless; fix on `tapestry/p2-escalate-forced-ask`. With the overlay patched: a deny shows "Denied by WeftOS governance: rule ...", escalate forces an ask, a local deny still beats a governance permit | same write-up; tips to follow from the Tapestry lead |
| 3 | Repeatable loopback onboarding script `scripts/tapestry/loopback-onboarding.sh`: runs the test hub, server and probe in a throwaway home; requires PROBE_OK and a 401 for a wrong key; checks loopback-only sockets and that the key is not logged; tears down | **done** 2026-11-08 PM, PASS on two runs, macOS only; on `tapestry/p3-item4-onboarding` c0f8510a (not verified from this Mac) | Tapestry lead delta |
| 3 | Not established, next release unless the owner pulls them in: `landlock_optional` degrade on a kernel below ABI 4, pass-key `auth_provider_command` route, supervised-by-weaver path for the backup hub, xpc WSL and `weftos` service-account hosts | **not started** | `p3-closeout-handoff.md` "Not established" |
| 3 | Leftovers the owner may delete: `/Volumes/ai-models/weftos-worktrees/real-merge-20261108-target`, `/tmp/wlabx.k83`, `~/tapestry-p3-linux-test-20261108/` on photo-gallery | **owner action** | `p3-closeout-handoff.md` item 8 |

## Tapestry side

Source: Tapestry lead (backend:4), message 2026-11-08 PM. Everything here is **not verified from this
Mac** except the WeftOS commits marked "git". The fork repo is `/Volumes/ai-models/research/tapestry`,
nothing pushed. I did not read it.

### Must land, in order

| # | Branch | Tip | What | Notes |
|---|---|---|---|---|
| 0 | `tapestry/p2-integration` | ef376c8d | Phase 1 and 2, plus owner-approved R6 live roster, verify-range, wire fixtures (2026-11-08) | Already integrated on the fork |
| a | `tapestry/p3-scoped-view` | ab572851 | Items 9 and 10: Linux sandbox (bwrap, allow-list seccomp, Landlock TCP, tmpfs `/tmp`, opt-in `landlock_optional`) plus the file half over our view verbs | Contains `tapestry/p3-linux-sandbox` 2d27f635. Needs the Linux run on PG first. Needs our `wt/adr126-e1-view-verbs` landed first |
| b | `tapestry/p3-item4-onboarding` | c0f8510a (c3e1be5b, e242fa6a, 01cfe6fa, c0f8510a) | Workspace server stops self-sandboxing; `--pass-key-file`; two stale upstream probe calls fixed plus the green loopback onboarding write-up (delta from the Tapestry lead, 2026-11-08 PM) | Do NOT land `tapestry/p3-hub-passkey` 847521bf (exploratory D5/D6) nor its backup `tapestry/p3-hub-passkey-full-backup` |
| c | `tapestry/p3-test-hub` | 17c43e99 | Test-only loopback hub crate | In no shipped binary |
| d | Docs: `p3-merge-rehearsal` e4146f9f, `p3-decision-memo` d4808a46, `p3-workspace-loopback` 23349d5d, `p3-prep-docs` cc216e50, `p3-closeout-docs` 4adc6cee (c1f4cec3 gates (a)(b)(c) rerun, b7b0b948 Phase 3 close-out handoff, 31ba29ba exits (f)(g)(h) rerun and R5/Q13/R16 status, 4b9f89d8 R11 target checks, 4adc6cee R8 and R9 live checks) | | Docs only | Tip read from the fork worktree 2026-11-08 14:40 |
| W | WeftOS `feat/infer-role-remote-stall` | 527ea4db0 | The Grok hosted-API inference role for `weftos-hard` (R18, Phase 1 exit h): per-role stall timeout, bind retry | Pushed to origin; the owner approved its merge on Friday 2026-11-06 (decision 8); **not an ancestor of `target-0.8.5`** (`git merge-base --is-ancestor`, 2026-11-08). Must land if the release ships `weftos-hard`; `docs/tapestry-design` 39b8260a6 and `feat/mesh-bots-phase-a` sit on top of it and do NOT land with it (Friday-leftovers report section 4 item 1) |
| e | `tapestry/p2-journal-windows` | 27bc741c (off ef376c8d, 4 files) | Windows build fix: the Unix-socket weave client is under `cfg(unix)`; a non-Unix stub returns `Unreachable` ("WeftOS daemon socket is Unix-only"). Callers: R9 maps it to `Verdict::Unavailable`, treated like Proceed by `hooks.rs:255`, so local policy still decides and nothing becomes permit-all; R6 roster fails soft; the R8 sink does not attach and R8 verify gives a local-only verdict | Green: `xai-grok-pager-bin` and `weft-journal --tests` type-check for x86_64-pc-windows-msvc exit 0; weft-journal, weft-roster, weft-policy tests exit 0 on the Mac. Not run: a real Windows host, linking. Tapestry lead delta 2026-11-08 PM, not verified from this Mac |
| f | `tapestry/p2-escalate-forced-ask` | pending (tip to follow) | R9 fix: `--always-approve` must not override a governance escalate; the forced ask stays forced headless | Found by the live R9 check; lands once green |
| HELD | `tapestry/p2-r11-release` | cb216887 | R11 self-update and release workflow | Do not merge |
| BACKUP | `feat/weft-computer-hub` | 99bad03c / b1b2726c0 | Approved backup plan: unmerged prep that stays unmerged while Phase 3 D1 is unanswered; work continues on it (owner clarification 2026-11-08 13:12, via the Tapestry lead) | Stays unmerged for this release unless the owner answers D1 in its favour |

### WeftOS-side commits, checked with git

`git merge-base --is-ancestor <commit> target-0.8.5` returns yes for all five. `target-0.8.5` is now
9164089b8.

| Commit | Role (per the Tapestry lead) | On `target-0.8.5` |
|---|---|---|
| 031b107cb | W1 | yes |
| b00ac6f95 | W6 | yes |
| bc86bcd70 | K0 | yes |
| 9492ec881 | wire fixtures | yes |
| 98658b929 | merge tip | yes |
| 541a9386b | `fix/overlay-gate-check-detailed` (off `target-0.8.5`, 3 files): `OverlayGate::check_detailed` delegates to the inner `GovernanceGate`, which holds the merged parent and overlay rules, under the same lock `check` uses; `evaluate()` untouched. clawft-kernel slice 249/249 under nextest; 3 new tests (overlay deny with its rule id, overlay `require_approval` as escalate, parent WL-DENY still wins). Other wrappers (CapabilityGate, WorkloadGate, TileZeroGate) have no engine and no such gap. Not done: an RPC-level project-kernel test. Doc `docs/governance/overlay-gate-check-detailed-2026-11-08.md` | no, unmerged; needs the G0 review chain and owner sign-off before merge |
| 93b52ca22 | `fix/daemon-boot-nextest` (tests only: `kernel_restart_e2e.rs` and `legacy_adoption_boot.rs` wait up to 300 s for the spawned weaver instead of 60 s; verified locally: 2 files, 17 insertions) | no, unmerged; owner merges |

### Tapestry test slices (all targeted, on the Mac)

| Lane | Result | Source |
|---|---|---|
| Sandbox `-p xai-grok-sandbox --lib command::backend` | 146/146; `cargo check --tests` for both Linux targets, type-check only | Tapestry lead |
| Pass key, computer use, hub backend, terminal selection | 7/7, 6/6, 4/4, 6/6 | same |
| Test hub | 20/20 | same |
| Integration: weft-roster, weft-journal, xai-grok-agent | 631/632, 1 load-sensitive flake | same |
| `xai-grok-shell` `weftos_roster` | 5/5 | same |
| WeftOS crates for the merges | 6222/6229, 5 pre-existing failures; the 2 daemon-boot nextest failures were not reproduced on ee9d48efe (see the row above) | same |

Needed before release is in section 4 (all **not run**). Owner calls are in section 3, tagged Tapestry.

### Conflicts with the ADR-126 branches: agreement check

The Tapestry lead's trial merges against `target-0.8.5` at 98658b929 and later report `s2-mac`, `s2-hooks`,
`s3-drive`, `s3-harden2` and `s1-names` clean, and `e1-scoped-view` and `e1-view-verbs` conflicting only in
the e1 handoff doc. I reran each against the current tip (`git merge-tree --write-tree --name-only`):
the same seven results. **Agreement.** This also refines my section 2a: the `drive/mod.rs` conflicts
there came from merging in sequence, and `e1-view-verbs` alone against `target-0.8.5` conflicts only in the
handoff doc, as does `e1-scoped-view`. Tapestry's fork shares no files with the store branches (the
Tapestry lead; not verified here). The coupling is item 10 calling `weaver store view`, so
`wt/adr126-e1-view-verbs` must land before item 10 is useful.

## 5. Hard stops for this release prep

- No push, no tag, no deploy, no weaver install, no production restart.
- No approvals entries. No merges into shared branches. Commits only by the lead, by path.
- Do not revert the landed Tapestry MDM change; the owner closed Phase 1 with it standing (2026-11-08 14:31). Unanswered owner calls (R11, engine-shape option B, Phase 3 D1 to D6, Phase A deploy) stay unanswered here.
- No Terraform apply, no PG credentials on the Mac, no edits in aepod or PG trees from here.
- The owner's release note (status page section 9): "the RustFS and Tapestry release happens all at
  once later". This page is preparation only.
- The merge simulation in section 2 wrote loose git objects (merge-tree trees and `commit-tree`
  synthetic commits) and no refs, no worktree files, no index changes.
