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

**Integration branch (2026-11-08 PM):** `wt/adr126-mac-integration` d003ba3d1 (worktree `adr126-mac-integration`, 87 commits over `target-0.8.5`) = `wt/adr126-e1-view-verbs` plus `-s2-meta` (72f715126, clean), `-s2-hooks` (6226544ce, one conflict in `scripts/tests/worktree-store.test.sh`, all tests kept, two assertions adapted to the injective slug and the real `wt.sh list`), `-s1-names` (5cb177479, clean in git but it broke `store_rpc_tests.rs`, which expected `store.n-pg-1`; fixed to `node-pg-1`, a conflict git did not show). The e1 handoff doc is byte-identical to the `target-0.8.5` copy. `git merge-tree --write-tree target-0.8.5 wt/adr126-mac-integration` reports **no conflicts** (re-checked by the lead). In memory: weftos-store rustfs 189 passed, 22 skipped; clawft-weave store 76; shell suite 63 of 63; clippy clean on both crates; worktree diff gate OK with no approvals entry. Live on a throwaway loopback RustFS 1.0.4: all 22 weftos-store live tests pass (142.7 s, noisy, load 6 to 12) and both clawft-weave store live tests pass (daemon service 0.6 s, view verbs 6.2 s). Evidence: `docs/research/adr126-mac-integration-2026-11-08.md` on the branch; ADR-126 header there carries an Updated 2026-11-08 paragraph and a "Mac branches (unlanded)" list. **Touch point with S1 at merge (aepod s1, via the owner 17:48):** S1's `backend.rs:210` declares `fn put_if_match(&self, bucket: &str, key: &str, data: &[u8], etag: &str) -> Result<ObjectMeta, BackendError>;` as a REQUIRED trait method; the Mac drive branch (2c43a95c5..1cade20f8, inside this integration branch) adds the identical signature as a default method returning `Unsupported`. At merge keep one declaration, S1's required one; S1's three backends and three test impls already implement it, so check that the Mac `mem`, `fs` and `rustfs` impls collapse to one each. `delete_if_match` does not exist in S1, so the Mac default stays as it is. **Second touch point, `BackendError::Unsupported` (aepod s1 via the owner, 18:08; sites confirmed by the lead with `git grep` on the integration branch):** S1 declares `Unsupported(String)` (its `backend.rs:26`, not present in the Mac's older `aepod/feat/adr126-s1` ref 53d007a41, so taken from aepod's report); the Mac enum declares a bare `Unsupported` (`crates/weftos-store/src/backend.rs:22`). Same keep-S1 rule: at merge the Mac defaults become `Err(BackendError::Unsupported("...".into()))` and the Mac match arms become `Unsupported(_)`. Sites to change, all in `crates/weftos-store/src/`: `backend.rs:60,64` (the two defaults), `backend.rs:155,156,158,161,162` (the `Bare` test impl and its two asserts); `drive/gc.rs:90`; `drive/remote.rs:94`; `drive/sync_up.rs:132,174`; `drive/view/lease.rs:104,131,150`; the test backends `drive/tests.rs:58,65` and `drive/tests_gc.rs:60`. Not affected: `drive/view.rs:91` `ViewError::Unsupported(String)` and `drive/view/work.rs:361`, `drive/view/tests.rs:393` (a different enum that already carries a String), the comment at `drive/sync_up.rs:10`, and the clawft-weave hits (`handshake_rpc.rs:273` is `ProtoCheck`, `store_worktree_tests.rs:525` is a comment). Landing option for aepod (owner call, brief 17): land this one branch after S1 instead of the eight Mac branches one by one; if the owner rejects the names amendment, revert the `s1-names` merge and the `node-pg-1` test edit together, and re-run merge-tree at landing.

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

**Settled 2026-11-09 13:20 ET:** S1 to S5 are all on `origin/target-0.8.5` e58899bb0 (rows below updated; the stale-ref columns are kept for the record). The task brief expected these to be absent. They were partly present on 2026-11-08. A remote-tracking set
`aepod/*` was fetched read-only on 2026-11-08 (next-steps section 1; `git branch -r | grep aepod`).
The owner-named hashes are not present. Column "verified from this Mac: no" means the tests and
state on aepod were not seen from here.

| Slice | Owner-named tip | `git cat-file -t` | Local `aepod/*` ref now | Status page tip | Verified from this Mac |
|---|---|---|---|---|---|
| S1 `feat/adr126-s1` | 2a8a1327 (owner, S1-merge pile) | commit | `aepod/feat/adr126-s1` = 2a8a13272 | ccd11d361 (stale) | **landed**: merge 68e010acf, Mac slices, approvals d26c0cbec, FF tip 15dcf72c3 (2026-11-09 AM); on `origin/target-0.8.5` e58899bb0 |
| S2 `feat/adr126-s2` | not named | n/a | aepod tip d0db74233 (rebase plan) | be64b6014 (stale) | **landed**: `land/adr126-s2-on-s1land` cae1b0b25 (reconcile 93a3836b5) merged onto 15dcf72c3 as 7ba34b071; on `origin/target-0.8.5` e58899bb0 |
| S3 `feat/adr126-s3` | not named | n/a | aepod tip fc9c081f7 | 0b67861d (stale) | **landed through S5**: fc9c081f7 merged into S5's rebased tip (96c78e454), nothing of S3 left to land. Unlanded S3 options on aepod: `prep/adr126-s3-reqwest-client` c9b85e569 (needs a reqwest diff-gate approval), `opt/adr126-nodekeys-adapter` 646b57ff5 |
| S4 `feat/adr126-s4` | not named | n/a | aepod `rebase/adr126-s4-on-64c547f4a` = e58899bb0 | f84191571 (stale) | **landed**: rebased onto 64c547f4a as 35bcc00e8..3d4d52834, owner approvals e58899bb0 (clawft-weave -> weftos-release `store`; weftos-store entry reworded), pushed 13:00 ET; e58899bb0 is the settled tip |
| S5 `feat/adr126-s5` | e5d6b9ac | not a valid object | aepod `feat/adr126-s5-on-s2-15dcf72c3` = 64c547f4a | e11d07b6b (stale) | **landed**: `feat/adr126-s5-on-s2land` 72be2ecc4 carried onto the S2 land as 64c547f4a (contains d26c0cbec, 15dcf72c3; approvals 3ddb1537e, 72be2ecc4 by weftos-s5); on `origin/target-0.8.5` e58899bb0 |
| `integ/adr126-s1-s4` | 76a26504 and e0447e0e | both not valid objects | 28c2a65e1 (10 ahead) | 28c2a65e1 | no |
| `feat/computer-hub-service` (Tapestry lead backend:4; Mac local only, not pushed) | n/a (lead-reported tip cd1bb2122) | not checked from this lane | base 64c547f4a, with `feat/weft-computer-hub` merged in as 078ceb4e4 | n/a | **not landed, by design**: lands after S3, S4 and ADR-131, so now after `feat/adr131-d1` only, and rebases or merges onto e58899bb0 first. Tapestry D1 hub as a clawft-weave service behind the off-by-default `computer-hub` feature, plus the W3 computer workload kind and the GBF-003 ADR. Approvals file: 6 entries added, owner-approved citing D1 (16 new-dep and 8 net findings). `store_ingest.rs` and `weftos-store` untouched. Gate fails on `approvals-change` only (owner's `DIFF_GATE_OWNER_APPROVED` in CI). Lead's delta 2026-11-09 PM; not verified from this lane |
| `feat/weave-wire-crate` (Tapestry lead backend:4; Mac local only, not pushed) | n/a (lead-reported tip 2ab12e6cf) | not checked from this lane | base 64c547f4a | n/a | **not landed, by design**: same order as the row above. New `crates/weftos-weave-wire` (serde only), a dev-dependency of clawft-weave only. Approvals file: 1 entry added, owner-approved citing D3. `store_ingest.rs` and `weftos-store` untouched. Gate fails on `approvals-change` only. Lead's delta 2026-11-09 PM; not verified from this lane |

Owner rule as of 2026-11-09 (relayed by the Tapestry lead): lane builds run in containers on `weftos-ci`, not on the Mac, and the WeftOS gate with the release dry run runs on the CI runner. aepod-xpc was offline on the tailnet when the lead's delta arrived (2026-11-09 PM), so nothing above the 13:00 push was re-fetched.

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
| `feat/manifest-v2` | fe62f5b (d9cb33a plus bf7cf3a `rust-toolchain.toml` 1.95, 78458d3 `version =` next to each path dependency, fe62f5b repo-plan "Pinned inputs" table) | same, worktree `weftos-mesh-mock-wt-manifest` |

2026-11-08 PM advance: the toolchain is pinned (1.95, a guess matching the WeftOS workspace; the toolchain behind the live SC binary is unknown), the three path dependencies carry versions (`weftos-tree-spike` 0.0.0, `weftos-integrator-spike` 0.0.0, `weftos-pq-sign` 0.8.4; none is a git repo, so the repo plan records a SHA-256 over each directory as the only real pin), `scripts/reproducible-build-check.sh` built `gcsmesh` twice into different target dirs with one hash (763b3892...; Mac arm64, not the x86-64 Linux binary the SC runs), 45 tests pass, mirror matches local. The Terraform pin (`sc_gcsmesh_sha256`, found in the `tf-gcp-seed` worktree, not under `~/weftos/gcp`) differs as expected and is untouched. Still the owner's: the remote and push rights, where the dependency crates get versioned, whether to deploy manifest v2 (CI-host x86-64 build, `sc-v...` tag, new pin).

The main checkout shows one untracked path, `tools/seed-drive-d1/.claude-flow/`. The seed cone (SC)
binary is pinned in Terraform `gcp/seed-store/variables.tf` (`sc_gcsmesh_sha256`, next-steps 3e).
The status page records the gcsmesh pin as unchanged. Verdict: no SC rebuild unless the owner wants
manifest v2 live. Manifest v2 flags default off; signing the `WMR2` chunk-table digest is an open
owner call (status page section 4). The three path deps (`weftos-tree-spike`, `weftos-integrator-spike`,
`weftos-pq-sign`) are not git repos, so a build is not yet reproducible from a commit (next-steps 3e).

## 2. Merge order and git-visible conflicts

Intended order on `target-0.8.5`: aepod S1 first (owner approval), the Mac branches in order, then
`feat/adr131-d1`. aepod S1 to S5 could only be simulated as far as local refs go, and this section
does not include them.

**S1 landed locally, 2026-11-08 PM (owner: "start the S1 merge now").** Worktree `adr126-s1-land`, branch
`integ/adr126-s1-land` off `target-0.8.5` f6af34678: merge commit 68e010acf = `aepod/feat/adr126-s1`
2a8a13272 (41 commits; one textual conflict, ADR-126 open question 2, resolved by keeping the owner's
"archives do not expire" and S1's setting-not-a-number wording), then 905d327a9, a macOS fix in
`RustfsBackend::private_dir` (S1 read `/proc/self` for its own uid; macOS has no `/proc`, so two S1
unit tests failed here; now a probe file in the secrets dir gives the uid, std only, no unsafe, no new
dep). S1 was built on `feat/infer-role-remote-stall`, so 7c37f5bb2, cbeed708a (the owner's ADR-101
approvals) and 527ea4db0 ride along. Tests on the merged tip: `weftos-store --features rustfs` 148/148;
16/16 live against a throwaway loopback RustFS 1.0.4 (binary from the spike target dir, random creds,
console off, data in the scratchpad, removed after); `clawft-weave --filter store` 50/50; the three
weave live store tests pass (`store_rt`, `store_svc`, `store_ingest`, the last spawning its own RustFS);
scoped clippy clean. **Not fast-forwarded onto `target-0.8.5`:** `scripts/diff-gate.sh --base
target-0.8.5` on the land tip reports 10 approved and 10 failed, and the failed ten are S1's receiver and
its loopback test listeners, which only the owner can approve (section 3 row "S1 receiver approvals").
The Mac branches (`wt/adr126-*`) all share merge-base ccd11d361 with S1, so they merge onto the land
branch as ordinary three-way merges.

**Mac land order done on the land branch, 2026-11-08 late PM (owner 21:20: continue without a separate go).**
Tip `integ/adr126-s1-land` **580a8df88** (first-parent chain: 68e010acf S1, 905d327a9 macOS fix, 30ea93478
s2-mac, f2bae2592 s2-meta, 18059aebd s2-hooks, 8ebb36618 s3-drive, 61fb7d931 s3-harden2, 4e8a26668
e1-view-verbs carrying e1-scoped-view, then a merge of target's docs commit 9bf35b717 so target is an ancestor
again). `wt/adr126-s1-names` skipped as told. Per step: s2-mac had four textual conflicts (policy.rs tests,
sigv4.rs, service_tests.rs, ADR-126 Updated lines) and two semantic ones (S1's F18 test wrote `.wt/` through
the service, which S2 refuses, so it plants the overlay state at the backend; S2's Deny resource list is now
sorted so the same grants give one policy document, which S1's concurrent-grants test demands); s2-meta clean;
s2-hooks one conflict in `scripts/tests/worktree-store.test.sh`, resolved as on the integration branch, shell
63/63; s3-drive three conflicts (backend.rs, fs.rs, mem.rs) resolved with the keep-S1 rule, plus a touch point
the earlier notes missed: S1's `BackendError::Stale(String)` is the 412 error and the Mac's
`PreconditionFailed(String)` is gone (16 sites, all in `crates/weftos-store/src`: backend, fs, mem, rustfs,
drive remote/sync_up/gc/live_tests/view lease and the contract tests); the Mac's duplicate RustFS
`put_if_match` dropped, its `delete_if_match` quotes the etag like S1's; s3-harden2 clean (three variant sites
in gc fixed); e1-view-verbs one add/add on the weftos-pg handoff note (target copy kept), six lease sites fixed.
Tests on 580a8df88: `weftos-store --features rustfs` 293 passed, 34 ignored; `clawft-weave --filter store` 94;
live on a throwaway loopback RustFS 1.0.4: weftos-store 27/27 (S1, S2 drills, drive, gc, view), clawft-weave
4/4 store live (`store_rt`, `store_svc`, `store_ingest`, `store_view`; the fifth `--filter live` hit is the
Hermes :8090 postmortem test, not store); scoped clippy clean. `scripts/diff-gate.sh --base target-0.8.5`:
still exactly the 10 S1 receiver rows, nothing new from the Mac branches. Was not fast-forwarded until the
owner's approvals (section 3).

**Fast-forwarded 2026-11-09 AM.** Owner answered card t213s943 at 08:34 ET: approve the S1 diff-gate lines and
fast-forward locally, no push. The four S1 entries (the three from `docs/guides/rustfs-store-ingest-approvals.draft.toml`
plus `crates/weftos-store/src/rustfs_tests.rs` `std::net::TcpListener::bind("127.0.0.1:0")`) were appended to
`config/diff-gate-approvals.toml` as **d26c0cbec** on the land branch and `target-0.8.5` was fast-forwarded to it
from 49ffd6cf6 (clean; the 38 dirty/untracked paths in `~/weftos` untouched). `scripts/diff-gate.sh --base target-0.8.5`
on d26c0cbec before the FF: 19 approved, 0 S1 rows failed; the one remaining FAIL is the structural `approvals-change`
row on the approvals file itself, which only `DIFF_GATE_OWNER_APPROVED=1` or the CI label clears (not set by this
session). Tests on d26c0cbec: `weftos-store --features rustfs` 293 passed, 34 skipped; `clawft-weave --filter store`
94 passed. S5's two `weftos-release` dev-dependency approvals are being added by weftos-s5 on xpc, not here. Not pushed.
Next: aepod rebases S2 and S5 onto d26c0cbec (S5 re-applies an S1 commit); `feat/adr131-d1` lands last;
`wt/adr126-s1-names` still waits for the node-<id> / pinned-creator confirmation. Method: `git merge-tree --write-tree --name-only --messages`. After each step a
synthetic commit was built with `git commit-tree` (objects only, no refs), so each step is "accumulated
result so far plus the next branch". Where a step conflicted, the marker tree was kept and the run went on,
so later conflicts in the same file can be knock-on effects.

**Settled 2026-11-09 13:00 ET (origin).** `origin/target-0.8.5` = e58899bb0 with S1, S2, S5 (carrying S3) and S4 all landed, in that order: 15dcf72c3 (S1 + approvals) -> 7ba34b071 (S2 land `land/adr126-s2-on-15dcf72c3`) -> 64c547f4a (S5 `feat/adr126-s5-on-s2-15dcf72c3`, with `feat/adr126-s3` fc9c081f7 merged in at 96c78e454) -> 35bcc00e8..3d4d52834 (S4 rebased onto 64c547f4a) -> e58899bb0 (owner approvals for S4: `clawft-weave` -> `weftos-release` with `store`; the `weftos-store` entry reworded as not test-only). 17 approvals entries on the tip. Pushed by the aepod lane at 13:00 ET; the Mac checkout `~/weftos` was still at 64c547f4a when this was written and was not moved (backend:3's checkout, Mathew's uncommitted files). S3 had nothing left to land. Remaining: `feat/adr131-d1` (weftos-d1), `wt/adr126-s1-names` (owner confirmation), the Tapestry-side WeftOS branches `feat/computer-hub-service` and `feat/weave-wire-crate` (backend:3), and the CI gate `scripts/build.sh gate --with-release-dry-run` on `weftos-ci` (owner: CI is the record, not the Mac). Test slices rerun on e58899bb0: section 4.

**Moved 2026-11-09 PM (git).** `origin/target-0.8.5` = 88b2fb172: e58899bb0 plus a52368b39 and 88b2fb172 (OverlayGate `check_detailed` fix and its RPC regression test; Tapestry R9 bug 1). Store slices not rerun; the delta has no store code. Flagged to the owner by the Tapestry lead and checked here with git: at the tip `crates/clawft-weave/Cargo.toml` depends on `weftos-store` (features `rustfs`) and `weftos-release` (features `store`) unconditionally, not optional, so since S1 every weaver built from `target-0.8.5` is store-capable; there is no store-less weaver to build from the tip. This bears on the no-store-capable-weaver install stop (section 3), which now covers any build from the tip. **Owner decision 2026-11-09 19:44 ET: change the rule, not the code.** Store-capable weavers are OK to install. Context he was given: `origin/target-0.8.5` 88b2fb172 cannot build a weaver without the store code, because since S1 `clawft-weave` always pulls in `weftos-store` and the store-enabled `weftos-release`. He chose to lift the install rule rather than make store support optional; store support is not to be made optional. Recorded in section 3 and in ADR-126 "Phase status".

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
| `config/diff-gate-approvals.toml`, aepod S1 commit 4eda02ced | It adds 22 approvals lines (owner approvals for the RustFS transport, 2026-11-07). The Mac branches already contain it and add 22 more (44 lines over target in `wt/adr126-s2-mac`). If aepod S1 lands first, git sees the same commit and should not conflict. If the approvals are re-applied by a rebased S1 copy, expect a conflict. **Merged 2026-11-08 PM:** the land of 2a8a13272 took `config/diff-gate-approvals.toml` with no conflict (S1's 22 lines plus cbeed708a's 22 ADR-101 lines over target); the gate still fails on the file change itself, owner-only | `git show --stat 4eda02ced -- config`; `git diff --stat target-0.8.5...wt/adr126-s2-mac -- config`; next-steps section 1 |
| `drive/mod.rs` across the store branches | Resolved by hand inside `wt/adr126-s3-harden2` (Options and tests.rs) and inside `-e1-view-verbs` | status page section 3, rows "Drive branches merged" and "verbs" |
| ADR-126 text | Edited on `-s2-hooks` (section 4a and still-open list, fc7e0c0e5) and on `-s1-names` (section 3 bucket table, `node-<id>`). Git auto-merges, but a reader must check the two edits agree: grant signing wording, archive retention answer, bucket naming | `git merge-tree` step 3 and 6; status page section 9 |
| ADR-131 text | Edited on `feat/adr131-d1` (D1 status, D1-1, D1-2 decisions) and on `target-0.8.5` this morning (Q5, Q8, Q9 lines). Auto-merges (step 9). The two must agree on Q5 and Q8 answered, Q9 noted owner-side | status page section 8 |
| aepod S5 carries a copy of S1's first commit | f89d3608a re-applies a8f123e02, so S5 must be rebased on landed S1 | next-steps section 1 |
| `BackendError::Unsupported` shape | S1: `Unsupported(String)`; Mac: bare `Unsupported`. Keep S1's; change the Mac defaults to `Unsupported("...".into())` and the arms to `Unsupported(_)` at the 15 sites listed in section 1a (backend.rs defaults and Bare test, drive gc, remote, sync_up, view lease, drive test backends). `ViewError::Unsupported(String)` is a different enum and stays | owner relay of aepod s1, 2026-11-08 18:08; lead's `git grep` |
| `BackendError::Stale` vs `PreconditionFailed` | **Resolved on the land branch 2026-11-08 PM:** S1 names the 412 error `Stale(String)`, the Mac drive branches `PreconditionFailed(String)`. Keep S1's; the Mac variant is removed and its 16 sites use `Stale(_)` | land commit 8ebb36618 |
| `Backend::put_if_match` declared twice | S1 declares it as required (`backend.rs:210`); the Mac drive branch declares the same signature as a default returning `Unsupported` (1cade20f8). Identical signatures, so git may auto-merge into a duplicate or a conflict; keep S1's required declaration and let the Mac `mem`, `fs`, `rustfs` impls collapse to one each. `delete_if_match` is Mac-only and stays | owner relay of aepod s1, 2026-11-08 17:48 |

## 3. Open owner calls that block release

No approval entries were added by this lane.
Diff-gate approvals are owner-only (CLAUDE.md, status page section 6); agents never edit
`config/diff-gate-approvals.toml`.

| Call | Where asked | What it blocks |
|---|---|---|
| **S1 receiver approvals.** `scripts/diff-gate.sh --base target-0.8.5` on `integ/adr126-s1-land` 905d327a9: 10 failed, all S1's: `net` at `crates/clawft-weave/src/store_ingest.rs:27,98` (the loopback webhook listener), `store_ingest_tests.rs:5,71,166`, `crates/weftos-store/src/rustfs_tests.rs:980,1119` (canned loopback servers in unit tests); `process` at `store_ingest_tests.rs:162,188` (the live test spawns RustFS); plus `approvals-change` on the file itself. aepod drafted three entries in `docs/guides/rustfs-store-ingest-approvals.draft.toml` (store_ingest.rs `TcpListener`; store_ingest_tests `(TcpStream|TcpListener)`; store_ingest_tests `std::process::|Command::new`); the draft does not cover `rustfs_tests.rs:980,1119`, so a fourth entry (`crates/weftos-store/src/rustfs_tests.rs`, `TcpListener`, loopback-only canned server) is needed. Only the owner appends them. **Answered 2026-11-09 08:34 ET (card t213s943): approved; appended as d26c0cbec and `target-0.8.5` fast-forwarded to it (section 2)** | S1 merge pile step 3; `docs/guides/adr-126-s1-merge-readiness.md` on the land branch | Fast-forwarding `target-0.8.5` to the S1 land and every later land step |
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
| ~~Lift the store-capable weaver install stop (Mac, aepod, PG)~~ **Answered 2026-11-09 19:44 ET: lifted.** **Owner decision 2026-11-09 19:44 ET: change the rule, not the code.** Store-capable weavers are OK to install. Context he was given: `origin/target-0.8.5` 88b2fb172 cannot build a weaver without the store code, because since S1 `clawft-weave` always pulls in `weftos-store` and the store-enabled `weftos-release`. He chose to lift the install rule rather than make store support optional; store support is not to be made optional. Still the owner's: when and where an install happens (none done); no push, no tag | `instructions/20261109-1944-fs-research-store-weaver-rule.txt` (agent-notes); this page section 2 | Nothing on the store-capable count any more; live store use, S1 live use and S5 live can be scheduled |
| Land order and approval: S1 first, then S2 and S5 rebased, then S3 and S4 | next-steps section 1; status page section 4 | Everything needing the store crate in the workspace |
| Phase 3 decisions D1 to D6 unanswered (Tapestry) | Tapestry lead, message 2026-11-08 PM; memo `docs/tapestry/phase3-decisions.md` on `tapestry/p3-decision-memo` | Tapestry hub work; D5 and D6 exploratory commits must not land |
| Engine-shape option B not applied (Tapestry) | same message | Tapestry integration shape |
| R11 release steps: secrets, environment, push, tag (Tapestry) | same message | `tapestry/p2-r11-release` cb216887 stays HELD |
| Diff-gate approvals for new unsafe and process-spawn code (Tapestry): bwrap backend raw Landlock and seccomp syscalls; ProcessRunner's weaver spawn | same message | `tapestry/p3-scoped-view` (item 10) |
| Sandbox calls C3 (allow-list profile contents), C4 (helper binary for Landlock inside bwrap, `noexec` on `/work`), C1 (how code gets to PG; archive in `~/tapestry-pg-linux-sandbox-20261108/`) (Tapestry) | same message | Linux run on PG, item 9 |
| Our `{code, message}` error payload, owner nod only if wanted (Tapestry) | same message; see the structured-errors row above | Nothing |
| R8 `project_required` gap (Tapestry): a `--profile user` daemon refuses `chain.append`, so the signed session journal records nothing on a user-profile daemon; it works only on a profile-less daemon or a project kernel. Owner decides whether the release accepts that or the daemon must accept journal appends on the user profile | `docs/tapestry/p2-r8-r9-live-checks.md` 4adc6cee | The R8 signed-journal claim on user-profile daemons |
| Gate (c) strings scan on linux x86-64 (Tapestry): 7 hits, a **platform** effect (x86-64 instruction immediates carry the 4 bytes inline, plus 3 literals glued in the string pool; same source scans clean on macOS aarch64; the profile does not change it). Owner decides whether the linux scan covers data sections only and allowlists the 3 glued literals by source string, and whether the release workflow scans the linux artifact. Nothing changed yet (allowlist, script, workflow) | Tapestry lead delta 2026-11-09 ~22:50 ET; `reports/tapestry-release-readiness-2026-11-09.md` question 1 (agent-notes) | The linux x86-64 artifact passing gate (c) as written |
| Items 6 (`tapestry/p3-hub-client` 0bf96e33) and 7 (`tapestry/p3-computer-use` 4fa59e19) ride this release or wait (Tapestry) | same report, question 3; 4a item 7 row | Whether computer use ships in this release |
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
| `origin/target-0.8.5` e58899bb0 (S1 to S5 settled; fresh detached worktree, cold target dir) | `build.sh test weftos-store --features rustfs`: 343 passed, 47 skipped; `clawft-weave --filter store`: 204 passed, 1541 filtered, 1 leaky (`release_cmd::tests::store_names_parse_and_bad_ones_are_refused` left a child process behind; nextest counts it as a pass); `weftos-release --features store`: 69 passed, 13 skipped; `clawft-weave --filter release_cmd`: 7 passed. Baseline at 15dcf72c3 was 293/34 and 94; the S2, S4 and S5 tests account for the rise | not rerun (no RustFS started; the next live run belongs to the CI gate on `weftos-ci`) | not rerun | not rerun; the owner's 17 entries are the tip | 2026-11-09 13:20 pile |
| `integ/adr126-s1-land` = `target-0.8.5` d26c0cbec (approvals commit on ac2f3a2be) | `build.sh test weftos-store --features rustfs`: 293 passed, 34 skipped; `clawft-weave --filter store`: 94 passed | not rerun (no store code change since 580a8df88) | not rerun | `--base target-0.8.5` (before FF): 19 approved, 0 S1 rows failed, only the owner-only `approvals-change` row | 2026-11-09 AM |
| `integ/adr126-s1-land` 580a8df88 (S1 plus the seven Mac branches) | `build.sh test weftos-store --features rustfs`: 293 passed, 34 ignored; `clawft-weave --filter store`: 94 passed; shell `worktree-store.test.sh` 63/63 | yes: weftos-store live 27/27 and clawft-weave store live 4/4 on a throwaway loopback RustFS 1.0.4 | clean (scoped) | `--base target-0.8.5`: 10 approved, the same 10 S1 rows failed | 2026-11-08 late PM |
| `integ/adr126-s1-land` 905d327a9 (S1 land on target) | `build.sh test weftos-store --features rustfs`: 148 passed, 16 ignored; `clawft-weave --filter store`: 50 passed; `clawft-weave --ignored --filter live`: 3 store live pass, 1 fail = `conv_postmortem` Hermes :8090 (not S1) | yes: 16/16 weftos-store live on a throwaway loopback RustFS 1.0.4, plus the three weave live store tests | clean (scoped `clippy weftos-store --features rustfs`) | `--base target-0.8.5`: 10 approved, 10 failed (section 3) | 2026-11-08 PM |
| mesh-mock `feat/manifest-v2` | 45 tests at last count in the plan, not rerun | no | n/r | n/a (outside the workspace) | n/r |

Pointers: status page section 3, next-steps section 7, d1 notes, and, on the branches,
`docs/research/adr126-s2-evidence-macos-2026-11-08.md` (`wt/adr126-s2-mac`) and
`docs/research/adr126-s3-materialise-watch-2026-11-08.md` (`wt/adr126-s3-harden2`).
All live runs were noisy (load 3 to 13), so no timing from them counts as a design number.

### What the release still needs. All of this is **not run**.

| Item | State | Pointer |
|---|---|---|
| Full workspace suite, `scripts/build.sh gate` | **not run**, release only. Not to be run now | CLAUDE.md; status page section 6 |
| Live tests on the merged final tip | **done on the Mac integration tip** d003ba3d1, 2026-11-08 PM: 22 of 22 weftos-store live tests (S2 drills, done-when, follower, drive cycle, hardening, gc, view) and 2 of 2 clawft-weave store live tests pass on loopback RustFS 1.0.4, noisy, load 6 to 12. Still to repeat on the final tip after aepod lands S1 | status page section 3, "Drive branches merged" |
| aepod S1 done-when on a throwaway RustFS | **done by aepod s1, 2026-11-08 PM** (owner relay 17:48): on `feat/adr126-s1` tip 1153af0a against a throwaway loopback RustFS 1.0.4 container, the weftos-store live suite passed 16 of 16 in 27.05 s including `live_service_done_when` and the F16 checksum/CopyObject test; clawft-weave `live_rustfs_backs_the_daemon_service` and `live_a_cog_uses_svc_store_on_rustfs_and_a_disabled_grant_is_refused` pass. Not S1 failures: the straight-to-RustFS audit test needs a local rustfs binary (container run only) and the Hermes postmortem test needs port 8090. Note committed there as 9efb7d7f, not pushed. The S1-layer live tests also passed on the Mac integration tip (row above). Rerun on the final tip still waits on S1 landing; land order stays with the owner | next-steps section 1 |
| Two-machine S2 drill (aepod plus Mac on one RustFS) | **not run** | next-steps 7, step 6 |
| Windows native bucket load | **not run**, never tested | next-steps 7, step 7 |
| D1 and D2 kill-criterion timings | **not run**, none exist. `gcloud auth login` works since 13:39 (d1 notes); blocked on a quiet 30 minute window and a signing path for a `drive-test/` chain; `sig_verified=false` in the join report needs a look | d1 notes; status page section 3 D1 row |
| PG end to end for the scoped view (weftos-pg, Linux bwrap side, five e2e checks on `weftos-ci`) | **not started** | status page section 9 |
| `weaver store drive` and `view` against a live daemon | **run for real 2026-11-10 (Tapestry E3 / item 10, `weftos-ci`)**, per the Tapestry lead: a weaver from `target-0.8.5` 88b2fb172 (the landed `weaver store view` verbs), a throwaway daemon in a `--network none` container with a local-directory store (no PG, no RustFS), Tapestry's bwrap/Landlock launcher. The interface matches the verbs with no functional mismatch; bwrap needed no extra podman flags. **One WeftOS-side gap blocks E3 without a workaround (next row).** With a test-only `chmod +x` on `base/` after open the full cycle passes: read, write, create, delete under `/work`; publish 2 puts, 1 delete, 0 conflicts; close releases the lease; store objects correct, the delete tombstoned. Minor: path-guard refusals come back unclassified (classification goes by message text); `limits` and `bucket` are not sent unless `--spec` carries them. Still owed by E3: overlay mode, conflict/resolve, lease expiry, Landlock proxy networking, refresh with remote changes. Not an install; still no store-capable weaver installed anywhere. **Rule changed 2026-11-09 19:44 ET:** store-capable weavers are OK to install, so a live-daemon run no longer waits on the release for that reason. not verified from this Mac beyond the code check in the next row | agent-notes `scratch/p3-e3/results.md`; fork branch `tapestry/p3-item10-real-view` 2461d3ab (local) with `scripts/tapestry/e3-real-view.sh`; Tapestry lead delta 2026-11-10 |
| WeftOS: **exec bit in store views** (E3 blocker) | **open, verbs owner (ADR-126)**: the store keeps no file modes and the drive view writes every base file 0444 (git: `crates/weftos-store/src/drive/view/tests.rs` at 88b2fb172 asserts base files are 0444 and work files 0644), so nothing in a root image is executable and a sandboxed run fails with `bwrap: execvp /bin/sh: Permission denied`. Needs the exec bit preserved or declared. **Owner answer 2026-11-10 17:08 ET** (`instructions/20261110-1708-backend3-exec-bit-object-metadata.txt`, addressed to the Tapestry lead): record each file's mode in the object metadata, not a list in the view spec; store the mode on put, apply it when the view materialises base files (write bits stay off, exec bits preserved); update the 0444 view tests and add exec-bit round-trip tests; targeted per-crate tests only; then rerun E3 on `weftos-ci` with a weaver from that branch without the chmod workaround and record it in `scratch/p3-e3/results.md`; report any diff-gate approvals needed, do not touch the approvals file. Still in force: no push, tag, publish or release; hub branches unmerged (D1); do not merge hub-passkey; R8 waits for ADR-131. **Being implemented by the Tapestry lead** on WeftOS `feat/store-file-mode` off 88b2fb172, worktree `/Volumes/ai-models/weftos-worktrees/store-exec-bit`, local only (legacy objects with no mode stay 0444). This lane does not start a parallel branch; the result arrives as a delta for this row. Recorded in ADR-126 "Phase status" | same evidence; Tapestry lead deltas 2026-11-10; the 17:08 instruction file (agent-notes) |
| S3 on a real edge node; S4 wired into `release.sh`; S5 as a second RustFS on PG | **not run** | next-steps 7, steps 9, 10, 13 |
| Tapestry: release dry run on `p2-integration` 77819c61 (`weftos-ci`, linux-x86_64) | **run 2026-11-09 PM** (Tapestry lead): release-dist build, gates (a) and (b), actionlint, throwaway-key sign dry run and targeted tests pass; **gate (c) strings scan fails** on linux x86-64, 7 hits (x86 instruction bytes and literals glued in the string pool, per the lead); unstripped artifacts 1.27 GB each. Not verified from this Mac. **Release fixes 2026-11-09 ~22:50 ET** (Tapestry lead, per the owner's 19:44 answers): `tapestry/p2-integration` fast-forwarded 77819c61 -> **e7cb0760** (ca3d65a6 + e7cb0760; git: the fork tip is e7cb0760); **the release candidate is now e7cb0760**. Strip before signing (linux-x86_64 1,269,693,896 -> 165,138,272 bytes; debug info to a separate non-release artifact); a pinned MinGit 2.55.0.windows.5 `-mingit` payload job (sha256 checked, signed in the dry run, a tampered zip refused); 39 stale xAI-telemetry shell tests `#[ignore]`d with reasons; one stale upstream test (`unified_list` chat-mode, hard-off in the snapshot) fixed rather than ignored. CI on `weftos-ci`: `xai-grok-shell` 7099 of 7101, the 2 failures are race-prone shared-state tests that pass 3 of 3 rerun alone; actionlint 0 errors. **Gate (c): PLATFORM** (linux x86-64 instruction immediates and glued literals; the profile does not matter); allowlist, scan script and workflow unchanged, owner to decide (section 3). All local on the fork, nothing pushed; not verified from this Mac beyond the git tip | `reports/tapestry-release-readiness-2026-11-09.md` (agent-notes), exact publish commands and the "After the owner's 7:44 PM answers" table |
| Tapestry: 16 Linux sandbox tests on PG | **done** 2026-11-08 PM: items 9+10 pass on photo-gallery, 104/104, 0 skips, all 16 Linux-only tests ok, no EPERM (commit eee49baf on `tapestry/p3-scoped-view`; `docs/tapestry/p3-linux-sandbox-pg-results-2026-11-08.md`; not verified from this Mac) | Tapestry lead, message 2026-11-08 13:12 |
| Tapestry: loopback onboarding e2e | **done** 2026-11-08 PM, green in both probe roles (Tapestry lead delta; evidence `docs/tapestry/p3-workspace-loopback.md` on `tapestry/p3-item4-onboarding` and logs in `~/tapestry-p3-onboarding-logs-20261108/`; not verified from this Mac) | same |
| Tapestry: Phase 1 exits (a), (b), (c) on the final integration build | **done** 2026-11-08 PM, all pass on `tapestry 1.0.45 (ef376c8d0250)`, a gate-only build, not installed: (c) strings scan 0 unexplained (x.ai 356: 350 ACP-exempt, 6 allowlisted; grok.com 1 exempt; mixpanel, sentry, googleapis 0); (a) no-key default session rc 0 with and without weft on PATH, sockets only 127.0.0.1:18181; (b) session plus subagent rc 0, 22 connects (9 loopback 18181, 13 unix kernel.sock R6 roster probe under the stand-in HOME, no daemon), 0 DNS, 0 non-loopback. Doc `docs/tapestry/p3-phase1-gates-rerun.md` on `tapestry/p3-closeout-docs` c1f4cec3; not verified from this Mac | Tapestry lead, delta 2026-11-08 PM |
| Tapestry: Phase 1 exits (f), (g), (h) on the final integration build | **done** 2026-11-08, all pass on `tapestry 1.0.45 (ef376c8d0250)` (same gate-only binary, sha256 5a7a4ae2...): (f) 0 canaries from a stand-in HOME full of Claude, Codex and `~/.grok` material, positive control on the phase0 binary saw 6; (g) 7 chat completions all naming the one catalog model, 0 `grok-*`, sockets only 127.0.0.1:18999; (h) `weftos-hard` completed through the gateway role with sockets only 127.0.0.1:18181 and :18182, 0 non-loopback rows for Tapestry, unset key names the variable (502 retried 7 times in 120 s, not terminal; the Tapestry lead calls the minutes-long retry a known robustness gap). Caveats recorded there: (h) passed with prompt steering (Grok chains shell tails, run 1 ended on the headless permission refusal); macOS arm64 only. (g) static half `scripts/check-catalog-models.sh` rerun 2026-11-08 about 20:35 ET: `check-catalog-models: OK`, exit 0, on both `tapestry/p2-integration` ef376c8d and `tapestry/p2-escalate-forced-ask` 23470789 (Tapestry lead delta) | `docs/tapestry/p3-exits-fgh-and-status.md` on `tapestry/p3-closeout-docs` 31ba29ba; not verified from this Mac beyond reading the doc |
| Tapestry: Phase 1 exit (d), xAI marks grep gate | **closed by the owner 2026-11-08 14:31**: the marks gate stays archived (`.archive/marks-gate-draft/`, 5b0e8b69); the WeftOS rewrite (card e89d2ba6) comes later and does not keep Phase 1 open. Rename grep proofs are in `docs/tapestry/rename-evidence.md` | owner instruction 14:31 |
| Tapestry: Phase 1 exit (e), existing tests pass or are deliberately rewritten | per-crate slices only (lane E triage 35aaa0e9; table above); the full Tapestry suite is release-only | `docs/handoff-tapestry-phase2.md`; owner rule |
| Tapestry: R8 `tapestry sessions verify --require-signed` end to end, and R9 policy layer against a real daemon (not a fake socket); G3 cross-node rule order review | **done** for R8 and R9 live (see 4a; `docs/tapestry/p2-r8-r9-live-checks.md` on `tapestry/p3-closeout-docs`). Still not run: the G3 cross-node rule-order review. That is a WeftOS governance spike card (bb4dff9d), not Tapestry work | Tapestry lead delta 2026-11-08 PM |
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
| 2 | R9 policy layer against a live daemon; rule-set loading and G3 cross-node rule order review | **done** live: two bugs found 2026-11-08 PM (row below); both fixes confirmed live by 2026-11-09 ~22:50 ET (escalate fix on `p2-integration`; overlay-gate fix at WeftOS 88b2fb172, see the row below). G3 cross-node rule order still not reviewed | `p2-r8-r9-live-checks.md`; Tapestry lead delta 2026-11-09 ~22:50 ET |
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
| 3 | Item 7: computer-use tool pack behind the R9 gate (W3 computer workload kind, W4 which desktops) | **built, not merged** (2026-11-09 ~22:50 ET): `tapestry/p3-computer-use` 4fa59e19 (git: the branch exists on the fork; commit subject: D5 computer-use access, own is full, granted needs an allowlist plus a per-action ask, unreachable policy denies). CI: weft-policy 23 of 23, `xai-grok-shell` computer-use slice 36 of 36. Not merged into `p2-integration`; whether it rides this release is the owner's call (section 3). The lead's report notes a field-name gap (`app`/`display`) until the hub's desktop tools are defined | Tapestry lead delta 2026-11-09 ~22:50 ET; cards f578103a, 26d7dbf1 |
| 3 | Item 8: shared wire crate adoption (W2) | **deferred** by the decision memo (D3 open); wire-shape fixtures landed instead (9492ec881) | `p3-decision-memo` D3; card 017ffa84 |
| 3 | Exploratory D5/D6 hub work | **do not land**: `tapestry/p3-hub-passkey` 847521bf and its backup | Tapestry lead |
| 3 | Test-only loopback hub crate | **built**, 20/20; lands as `tapestry/p3-test-hub` 17c43e99, in no shipped binary | Tapestry lead |
| 3 | Exit E1 (session binds to a remote workspace through the WeftOS hub) | **not met**; waits on D1 (item 5) | `phase3-prep.md` section 1 |
| 3 | Exit E2 (mesh agents live in the roster) | **done** | same |
| 3 | Exit E3 (Linux sessions under bwrap/Landlock with the store view as filesystem) | **built, not landed**; items 9 and 10 above | same |
| 3 | aarch64 `table_matches_libc` on the OrbStack `weftos-arm64` VM (kernel 7.2, toolchain 1.95) | **done** 2026-11-08 PM (G7), per the Tapestry lead; not verified from this Mac | Tapestry lead delta 14:4x |
| 2 | R11 target checks on ef376c8d: aarch64-apple-darwin and x86_64-apple-darwin pass (real checks); x86_64 and aarch64 Linux pass (stand-in type-check); x86_64-pc-windows-msvc **fails on ef376c8d** (`weft-journal/src/rpc.rs:6` imports a Unix socket with no cfg gate); **fixed on `tapestry/p2-journal-windows` 27bc741c** (must-land row e) | **done** as a stand-in type-check; a real Windows host and linking are not run. Write-up 4b9f89d8 on `tapestry/p3-closeout-docs` | Tapestry lead delta 2026-11-08 PM |
| 2 | R8 live check against an isolated test weaver (built from cebdcc35a, debug, not store-capable, per the Tapestry lead) | **done, passes**: `--require-signed` reports 5 before a checkpoint, 0 after one, 3 LOCAL ONLY with the daemon down, 0 after a daemon restart; `chain.get_range` used on every verify; tampering gives DIGEST MISMATCH (exit 2); no inet sockets from the test daemon, Tapestry only 127.0.0.1:18181. **Gap, owner call:** a `--profile user` daemon refuses `chain.append` with `project_required`, so R8 journals nothing there; it works only against a profile-less daemon or a project kernel | `docs/tapestry/p2-r8-r9-live-checks.md`, 4adc6cee on `tapestry/p3-closeout-docs`; not verified from this Mac |
| 2 | R9 live check, same daemon | **run; two bugs found, fixes in progress**: (1) WeftOS: on a project kernel `governance.check` answers `no_governance_engine` because `OverlayGate` lacks `check_detailed`, a silent fail-open where an overlay deny is ignored; fix on WeftOS `fix/overlay-gate-check-detailed` 541a9386b, green (clawft-kernel 249/249), needs the G0 review chain before merge. (2) Tapestry: `--always-approve` overrides a governance escalate, so the forced ask is not forced headless; fix on `tapestry/p2-escalate-forced-ask` 23470789 (must-land row f). Both R9 live bugs now have fix branches: 541a9386b and 23470789. With the overlay patched: a deny shows "Denied by WeftOS governance: rule ...", escalate forces an ask, a local deny still beats a governance permit. **Confirmed live 2026-11-09 ~22:50 ET** (Tapestry lead): R9 against a weaver built from the current `origin/target-0.8.5` 88b2fb172 (store-capable, which the owner's 19:44 decision allows). **A test, not an install** (Tapestry lead, confirmed 2026-11-09 ~23:05 ET): a debug weaver from `scripts/build.sh native-debug` ran in a `--rm` podman container on `weftos-ci` with its own HOME and runtime dir and a loopback stub model; the store was never configured; nothing was installed on the Mac, xpc or PG; the owner's daemon and `~/.weftos` were not touched. Evidence: agent-notes `scratch/r9-current/`: the overlay gate enforces deny, escalate and permit; the fail-open is fixed. not verified from this Mac | same write-up; Tapestry lead delta 2026-11-09 ~22:50 ET |
| 3 | Repeatable loopback onboarding script `scripts/tapestry/loopback-onboarding.sh`: runs the test hub, server and probe in a throwaway home; requires PROBE_OK and a 401 for a wrong key; checks loopback-only sockets and that the key is not logged; tears down | **done** 2026-11-08 PM, PASS on two runs, macOS only; on `tapestry/p3-item4-onboarding` c0f8510a (not verified from this Mac) | Tapestry lead delta |
| 3 | Not established, next release unless the owner pulls them in: `landlock_optional` degrade on a kernel below ABI 4, pass-key `auth_provider_command` route, supervised-by-weaver path for the backup hub, xpc WSL and `weftos` service-account hosts | **not started** | `p3-closeout-handoff.md` "Not established" |
| 3 | Leftovers the owner may delete: `/Volumes/ai-models/weftos-worktrees/real-merge-20261108-target`, `/tmp/wlabx.k83`, `~/tapestry-p3-linux-test-20261108/` on photo-gallery | **owner action** | `p3-closeout-handoff.md` item 8 |

## Tapestry side

Source: Tapestry lead (backend:4), message 2026-11-08 PM. Everything here is **not verified from this
Mac** except the WeftOS commits marked "git". The fork repo is `/Volumes/ai-models/research/tapestry`,
nothing pushed. I did not read it.

**Delta 2026-11-09 PM (Tapestry lead backend:4; recorded as sent, not verified from this Mac except the lines marked git).** `tapestry/p2-integration` is now 77819c61, reached by fast-forwards: the R11 merge 0e94d34b, option B ae0eee22, the R9 escalate fix 6b08db86..976ec699, then a test-only kernel.sock isolation merge, 77819c61. Phase 2 close-out: exits (f), (g), (h) pass against a loopback stand-in only (the real gateway was not exercised). R8: signed journal verified, the `project_required` gap remains an owner call. R9: the escalate fix is confirmed live; the overlay-gate fix is upstream as a52368b39 plus its RPC regression test 88b2fb172 on `origin/target-0.8.5` (git: fetched 2026-11-09 PM, tip 88b2fb172); its gate source is byte-identical to the fix weaver that enforced deny, escalate and permit live, and the targeted tests at 88b2fb172 pass on `weftos-ci` (6/6 kernel `check_detailed`, 1/1 RPC regression). Report: `reports/tapestry-p2-closeout-2026-11-09.md` under the agent-notes directory. Release dry run on 77819c61 (`weftos-ci`, linux-x86_64): the release-dist build, gates (a) and (b), actionlint, the throwaway-key sign dry run and the targeted tests all pass; gate (c), the strings scan, **fails** on linux x86-64 with 7 hits, explained as x86 instruction bytes and literals glued in the string pool; the unstripped artifacts would be 1.27 GB each. Readiness note with the exact publish commands: `reports/tapestry-release-readiness-2026-11-09.md`, same directory. WeftOS side: `feat/computer-hub-service` cd1bb2122 and `feat/weave-wire-crate` 2ab12e6cf stay on 64c547f4a, unmerged; the combined `scripts/build.sh gate --with-release-dry-run` still waits on ADR-131.

**Delta 2026-11-09 ~22:50 ET (Tapestry lead, now backend:3; recorded as sent, not verified from this Mac except the lines marked git).** `tapestry/p2-integration` fast-forwarded 77819c61 -> e7cb0760 (release fixes ca3d65a6 and e7cb0760; git: the fork tip is e7cb0760 and both commits exist). **The release candidate is now e7cb0760.** Release fixes per the owner's 19:44 answers: strip before signing (linux-x86_64 1,269,693,896 -> 165,138,272 bytes); a pinned MinGit 2.55.0.windows.5 `-mingit` payload job (sha256 verified, signed in the dry run, tamper refused); 39 stale xAI-telemetry shell tests `#[ignore]`d with reasons; one stale upstream test (`unified_list` chat-mode, hard-off in the snapshot) fixed rather than ignored. CI on `weftos-ci`: `xai-grok-shell` 7099 of 7101; the 2 failures are race-prone shared-state tests that pass 3 of 3 rerun alone; actionlint 0 errors. Gate (c): a platform effect (linux x86-64 instruction immediates plus glued literals; the profile does not matter); allowlist and workflow unchanged, owner to decide (section 3). R9 live on current WeftOS 88b2fb172 (store-capable weaver, allowed since 19:44; a test daemon in a throwaway podman container on `weftos-ci`, not an install, store never configured, confirmed by the lead ~23:05 ET): the overlay gate enforces deny, escalate and permit; the fail-open is fixed. Item 7 (`tapestry/p3-computer-use` 4fa59e19; git: branch present): weft-policy 23 of 23, shell slice 36 of 36; not merged. Reports: `reports/tapestry-release-readiness-2026-11-09.md` and `reports/tapestry-p2-closeout-2026-11-09.md` under the agent-notes directory. All local; nothing pushed.

### Must land, in order

| # | Branch | Tip | What | Notes |
|---|---|---|---|---|
| 0 | `tapestry/p2-integration` | ef376c8d | Phase 1 and 2, plus owner-approved R6 live roster, verify-range, wire fixtures (2026-11-08) | Already integrated on the fork | **Now e7cb0760** (2026-11-09 ~22:50 ET; release fixes ca3d65a6 + e7cb0760 on 77819c61, which was reached by the fast-forwards R11 0e94d34b, option B ae0eee22, R9 escalate 6b08db86..976ec699, kernel.sock isolation test merge; git: fork tip e7cb0760). Release candidate; see the deltas above |
| a | `tapestry/p3-scoped-view` | ab572851 | Items 9 and 10: Linux sandbox (bwrap, allow-list seccomp, Landlock TCP, tmpfs `/tmp`, opt-in `landlock_optional`) plus the file half over our view verbs | Contains `tapestry/p3-linux-sandbox` 2d27f635. Needs the Linux run on PG first. Needs our `wt/adr126-e1-view-verbs` landed first |
| b | `tapestry/p3-item4-onboarding` | c0f8510a (c3e1be5b, e242fa6a, 01cfe6fa, c0f8510a) | Workspace server stops self-sandboxing; `--pass-key-file`; two stale upstream probe calls fixed plus the green loopback onboarding write-up (delta from the Tapestry lead, 2026-11-08 PM) | Do NOT land `tapestry/p3-hub-passkey` 847521bf (exploratory D5/D6) nor its backup `tapestry/p3-hub-passkey-full-backup` |
| c | `tapestry/p3-test-hub` | 17c43e99 | Test-only loopback hub crate | In no shipped binary |
| d | Docs: `p3-merge-rehearsal` e4146f9f, `p3-decision-memo` d4808a46, `p3-workspace-loopback` 23349d5d, `p3-prep-docs` cc216e50, `p3-closeout-docs` 4adc6cee (c1f4cec3 gates (a)(b)(c) rerun, b7b0b948 Phase 3 close-out handoff, 31ba29ba exits (f)(g)(h) rerun and R5/Q13/R16 status, 4b9f89d8 R11 target checks, 4adc6cee R8 and R9 live checks) | | Docs only | Tip read from the fork worktree 2026-11-08 14:40 |
| W | WeftOS `feat/infer-role-remote-stall` | 527ea4db0 | The Grok hosted-API inference role for `weftos-hard` (R18, Phase 1 exit h): per-role stall timeout, bind retry | Pushed to origin; the owner approved its merge on Friday 2026-11-06 (decision 8); **not an ancestor of `target-0.8.5`** (`git merge-base --is-ancestor`, 2026-11-08). Must land if the release ships `weftos-hard`; `docs/tapestry-design` 39b8260a6 and `feat/mesh-bots-phase-a` sit on top of it and do NOT land with it (Friday-leftovers report section 4 item 1) |
| e | `tapestry/p2-journal-windows` | 27bc741c (off ef376c8d, 4 files) | Windows build fix: the Unix-socket weave client is under `cfg(unix)`; a non-Unix stub returns `Unreachable` ("WeftOS daemon socket is Unix-only"). Callers: R9 maps it to `Verdict::Unavailable`, treated like Proceed by `hooks.rs:255`, so local policy still decides and nothing becomes permit-all; R6 roster fails soft; the R8 sink does not attach and R8 verify gives a local-only verdict | Green: `xai-grok-pager-bin` and `weft-journal --tests` type-check for x86_64-pc-windows-msvc exit 0; weft-journal, weft-roster, weft-policy tests exit 0 on the Mac. Not run: a real Windows host, linking. Tapestry lead delta 2026-11-08 PM, not verified from this Mac |
| f | `tapestry/p2-escalate-forced-ask` | 7aa95aa8 (23470789 plus one test commit; base ef376c8d, unmerged, not pushed; waits on the owner's merge call) | R9 fix: a governance escalate (forced ask) beats `--always-approve`. Headless rejects a request carrying `_meta.hookAsk` and the refusal names the rule; in the TUI under yolo a forced ask is queued for the human and the yolo drain leaves it queued; ordinary asks under always-approve unchanged; deny still wins | Targeted tests all exit 0: xai-grok-workspace 2, xai-grok-pager 43, weft-policy 9; MODIFICATIONS gate OK. Follow-ups done (Tapestry lead delta, late PM; write-up `p2-r8-r9-live-checks.md` section "Escalate fix 23470789 follow-ups" on `tapestry/p3-closeout-docs` a590173e): xai-grok-shell tests 288/288 with filter "permission hook approv" under `env -i`, empty HOME and `RUST_MIN_STACK=33554432`; the first run with the inherited environment gave 265 passed, 23 failed, all `HookDenied weftos-governance`, because those tests reached the owner's real `~/.weftos/run/kernel.sock` and R9 asked a live daemon. These shell tests were not hermetic: fixed as test code on `tapestry/p2-shell-tests-hermetic` cce0bdd4 (row g); until that lands the release suite must run them with an isolated HOME. New test `hook_ask::policy_deny_beats_yolo_and_forced_ask` (xai-grok-workspace) passes and fails with the deny-before-yolo branch disabled. Live headless rerun with a debug `tapestry` from 7aa95aa8 against the patched test weaver (not store-capable), both under `--always-approve`: (a) escalate refused, file not created, reason names rule `overlay.approval[0]`; (b) deny still denied, file not created; Tapestry used only 127.0.0.1:18181, daemon and child had no inet sockets. Still not established: the TUI forced ask under yolo beyond unit tests, the subagent path, a human answering the ask. Behaviour change to note in release notes: headless without `--always-approve` now rejects a forced ask with the reason instead of ending the turn as cancelled. Tapestry lead delta 2026-11-08 PM, not verified from this Mac |
| g | `tapestry/p2-shell-tests-hermetic` | cce0bdd4 (off `tapestry/p2-integration` ef376c8d, independent of row f; unmerged, not pushed) | Test code only: a pre-main ctor in `xai-grok-shell/src/test_support/mod.rs` sets `WEFTOS_RUNTIME_DIR` to an empty per-process tempdir, beside the existing `TAPESTRY_HOME` ctor, plus an isolation test and a `MODIFICATIONS.md` line. No dependency or product change | The 288 tests matched by "permission hook approv": 288/288 with the real HOME and the real socket present, 288/288 under `env -i` with an empty HOME, 265 pass and 23 fail with the ctor disabled (so it fixes the 23 from row f). `lsof -U` sampled every 0.5 s during the real-HOME run, 58 samples, none showed `kernel.sock` (a connection shorter than 0.5 s could be missed); the isolation test asserts the resolved path is in a tempdir, not `~/.weftos/run`, and that `weft_policy::check` against it returns `Unavailable`. Not audited for the same leak: the integration test binaries under `tests/` and other crates. Write-up `p2-r8-r9-live-checks.md` section "Hermetic xai-grok-shell permission tests" on `tapestry/p3-closeout-docs` 57808043. Applies cleanly on p2-integration; once it lands the release-suite note "run shell tests with an isolated HOME" in row f is void. Tapestry lead delta 2026-11-08 late PM, not verified from this Mac |
| HELD | `tapestry/p2-r11-release` | cb216887 | R11 self-update and release workflow | Do not merge |
| BACKUP | `feat/weft-computer-hub` | 99bad03c / b1b2726c0 | Approved backup plan: unmerged prep that stays unmerged while Phase 3 D1 is unanswered; work continues on it (owner clarification 2026-11-08 13:12, via the Tapestry lead) | Stays unmerged for this release unless the owner answers D1 in its favour |

### WeftOS-side commits, checked with git

`git merge-base --is-ancestor <commit> target-0.8.5` returns yes for all five. `target-0.8.5` is now
9164089b8.
**2026-11-09 PM (git):** `origin/target-0.8.5` is 88b2fb172 = e58899bb0 (ADR-126 S1 to S5 settled) plus a52368b39 (`OverlayGate::check_detailed`, the fix from row 541a9386b, landed upstream) and 88b2fb172 (its RPC-level regression test). No store code in that delta, so the section 4 store slices on e58899bb0 stand.

| Commit | Role (per the Tapestry lead) | On `target-0.8.5` |
|---|---|---|
| 031b107cb | W1 | yes |
| b00ac6f95 | W6 | yes |
| bc86bcd70 | K0 | yes |
| 9492ec881 | wire fixtures | yes |
| 98658b929 | merge tip | yes |
| 541a9386b | `fix/overlay-gate-check-detailed` (off `target-0.8.5`, 3 files): `OverlayGate::check_detailed` delegates to the inner `GovernanceGate`, which holds the merged parent and overlay rules, under the same lock `check` uses; `evaluate()` untouched. clawft-kernel slice 249/249 under nextest; 3 new tests (overlay deny with its rule id, overlay `require_approval` as escalate, parent WL-DENY still wins). Other wrappers (CapabilityGate, WorkloadGate, TileZeroGate) have no engine and no such gap. RPC-level project-kernel test added at tip 74462982c (local only, not merged, not pushed): `governance_check_rpc_on_a_project_kernel_enforces_overlay_rules` in `crates/clawft-weave/tests/governance_check_project_kernel.rs`, in-process, boots a real project-profile kernel and goes through `daemon::handle_connection` over a unix socket to the real `governance.check` handler, no daemon process. Checks: overlay deny `d1` is denied and the answer names `d1`; overlay `require_approval` returns `escalate_to_human`; an uncovered action is permitted; never `no_governance_engine`. With the fix 1 passed; without it (overlay_runtime.rs from d3945ceb2) FAILED with `no_governance_engine`. `diff-gate.sh --base d3945ceb2`: 0 failed. Still not covered: a spawned daemon process with socket auth, and an overlay reload while serving. Doc `docs/governance/overlay-gate-check-detailed-2026-11-08.md` | no, unmerged; needs the G0 review chain and owner sign-off before merge |
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
