# Shared filesystem status: ADR-126 and ADR-131 (2026-11-08)

Written by the weftos-fs-research session on the Mac for owner item 3. It lists each lane, what is
done with a pointer to the evidence, and which owner decision unblocks what. Sources are the repo
at `target-0.8.5` a89cfebcf, the Mac worktrees under `/Volumes/ai-models/weftos-worktrees`, the
mesh-mock worktree, and session memory as of 2026-11-08. Nothing on aepod-xpc or photo-gallery was
read from here. Timings taken today are "noisy, load 6-7" unless a row says otherwise.

## 1. What the ADRs are

| ADR | What it is | Status | Built where |
|---|---|---|---|
| 126 | Mesh object store: WeftOS-governed buckets on RustFS for nodes, projects, worktrees, releases. L1 tier (LAN staging and sharing) | Proposed since 2026-10-29 | S1 to S5 code on aepod branches, S2 fixes and a drive prototype on Mac branches. Nothing landed, nothing deployed |
| 128 | RVF layered trees, git kept as integrator. Gate G2 (the mount route) is answered by ADR-131 | Proposed | Spike crates in the mesh-mock repo, outside the workspace |
| 130 | GCS seed store: one elected writer (seed cone, SC), signed heads, PG as standby. L2 durable tier | Accepted 2026-11-01 | Live in GCP; source in the mesh-mock repo, local git only |
| 131 | Seed drive: a project is a materialised native tree plus a watcher, not FUSE. SC is the primary file holder | Proposed since 2026-11-01 | D1 Mac daemon spike on `feat/adr131-d1`; nothing deployed |

The rclone mount was rejected by the owner. The access path is materialise and watch, in two forms:
the ADR-131 drive over GCS (L2), and the Mac drive library over RustFS buckets (L1).

## 2. Lanes

| Lane | Owner and host | Branches and tips | Done (evidence) | In progress | Blocked on |
|---|---|---|---|---|---|
| weftos-fs-research | This Mac session; Mac worktrees | `wt/adr126-s2-mac` 220952fe9; `wt/adr126-s2-hooks` af688ca60; `wt/adr126-s2-meta` 8f4652c16; `wt/adr126-s3-drive` a757fe2cc; mesh-mock `feat/manifest-v2` d9cb33a | See rows below. S2 macOS evidence, 25 defect fixes, hook seam, metadata bench, drive library and verbs, mesh-mock manifest v2, range join, cache, drive index, adoption plan, repo plan | E-1 scoped view for Tapestry R9 on `wt/adr126-e1-scoped-view`; drive hardening tests on `wt/adr126-s3-harden`. Both branches sit at a757fe2cc with no new commits and clean trees. In progress, 2026-11-08 AM | aepod merging the Mac branches; PG credentials; owner decisions in section 4 |
| weftos-d1 | Mac worktree `adr131-d1` | `feat/adr131-d1` 363ebb3f7, plus uncommitted edits to `drive.rs`, `drive_tests.rs`, the ADR-131 text | D1 daemon spike; kill points 1 to 3 pass offline; kill point 4 passes against a loopback bucket; refresh redo hazard fixed; D2 design and overlay round trip pass (`docs/research/adr131-d2-linux-drive-design.md`; `/tmp/weftos-d1-status.md`) | Head-lineage and truncated-copy options, one-command harness `d1_run.py` | **No D1 or D2 timing exists.** Needs `gcloud auth login`, a quiet 30 min window, and the diff-gate decision on the crate's new deps |
| aepod weftos-ci | aepod-xpc sessions s1 to s5 | `feat/adr126-s1..s5` on `~/weftos-wt/s1..s5`, unpushed `integ/adr126-s1-s4`; S2 +4 and S3 +3 over origin | Code for all five slices (per next-steps doc section 1) | Not verified from this Mac | Landing order S1 first; F2, F9, F15 to F18 open; merging the Mac branches; Windows bucket load never run |
| photo-gallery (weftos-pg) | PG session as `mathew`; PG tailnet 100.90.170.87 | Checkout `/data1/home/mathew/weftos` at 527ea4db | Host survey; S0 proofs in ADR-126 "S0 results" | RustFS 1.0.4 live on `:9700`, with JuiceFS mounts, Redis `:9710`, meshd `:9720` still up | PG RustFS upgrade (card `3f30aaf9`, upstream fixes #9067 and #9073 merged upstream, not deployed); no PG credentials on the Mac; Linux bwrap side of E-1 (handoff pending); the owner's pre-PG asks in section 4 |

Merge order for aepod, as the owner set it: `wt/adr126-s2-mac`, then `wt/adr126-s2-hooks`, then
`wt/adr126-s3-drive`. `wt/adr126-s3-drive` already contains `wt/adr126-s2-mac`. `wt/adr126-s2-hooks`
has diverged from it (7 commits over be64b6014, touching `scripts/worktree-store.sh`, the ADR-126
text and the review), so expect a script and ADR text conflict check. `wt/adr126-s2-meta` is in
neither the lead's list nor any other branch; it needs a place in the order.

## 3. Slices

Label meanings: proven means run and recorded; built means code and in-process tests only.

| Slice | State | Where the code is | Tests | What proves it |
|---|---|---|---|---|
| S0 spike | proven on PG | TF `tf/rustfs-spike` 024ad78 | not applicable | ADR-126 "S0 results": audit gaps, policy refusal, revocation 0.22 s, STS 900 s, conditional put, import not in place |
| S1 store crate, grants, `weaver store`, cog `svc.store` | built; done-when proven on a throwaway RustFS | aepod `feat/adr126-s1` ccd11d361 | Count not verified from this Mac | ADR-126 "S1 status". Audit stream never produced a record from env config (F9 open) |
| S2 worktree stores, follower view | built; macOS evidence on one host; 25 fixes on Mac branch | aepod `feat/adr126-s2` be64b6014; Mac `wt/adr126-s2-mac` 220952fe9, `-hooks`, `-meta` | aepod: 56 integration tests plus `s2_done_when`. Mac: weftos-store rustfs 114, clawft-weave store 52, real-RustFS ignored 11 of 11 | `docs/research/adr126-s2-evidence-macos-2026-11-08.md` on `wt/adr126-s2-mac`; review `docs/research/adr126-s1-s2-adversarial-review-2026-11-08.md` on `wt/adr126-s2-hooks` |
| S3 edge creds | built | aepod `feat/adr126-s3` 0b67861d | 37 against a mock, one live loopback run | ADR-126 "S3 results". Not run on a real edge node |
| S4 release stores | built | aepod `feat/adr126-s4` f84191571 | `tests/release_store.rs` | Not wired into `release.sh` or CI; CI files are owner-approved |
| S5 client accounts | built | aepod `feat/adr126-s5` e11d07b6b | `client_store_tests.rs`, in progress when last read | Never run as a second RustFS on PG |
| Drive library (Mac) | prototype proven on loopback; verbs wired | `wt/adr126-s3-drive` a757fe2cc, `crates/weftos-store/src/drive/` | weftos-store rustfs 141, clawft-weave store 61 | `docs/research/adr126-s3-materialise-watch-2026-11-08.md`. 500-file materialise 435 ms, watch plus sync cycle 2.3 s of which 2.0 s is the quiet rule; noisy, load 7-9. RustFS 1.0.4 honours `If-Match` on PUT and DELETE; not run against a live daemon |
| D1 Mac drive daemon | functional only | `feat/adr131-d1` 363ebb3f7 | 72 targeted tests at last check | Kill-criterion timings not taken (warm `git status` at most 2x local, refresh at most 1 s, edit to seal at most 3 s) |
| D2 Linux drive | design plus overlay spike | same branch, `tools/seed-drive-d1/src/overlay.rs` | 12 overlay tests | Round trip upper dir to layer to bucket to second drive passes in both directions. CI-host reflink support unmeasured |
| E-1 scoped view | in progress, 2026-11-08 AM | `wt/adr126-e1-scoped-view`, no commits yet | none yet | Tapestry plan section 4 Phase 3 needs it. Mac side is the interface; Linux bwrap side is the PG or aepod handoff |
| Metadata cost | proven on loopback; PG extrapolated | `wt/adr126-s2-meta` 8f4652c16 | bench under `scripts/bench/` | `docs/research/adr126-s2-metadata-latency-2026-11-08.md`. One poll per worktree per 30 to 60 s is affordable. Mac to PG round trip 58 ms p50, 114 ms p99 |
| Mesh-mock manifest v2 | built, flags default off | `feat/manifest-v2` d9cb33a | 45 at the last count in the plan; not rerun | `docs/manifest-v2.md`, `docs/manifest-range-bench.txt`. Range join fetches 7.9% of v1 and is typed unverified until the head signs the chunk-table digest |
| Drive index | built, `MESH_DRIVE_INDEX` default off | same branch, `src/driveidx.rs` | in the 45 | Capture of 3 edits hashes 3 files instead of 5,873 (222 ms against 2,202 ms); partial refresh does 30 ops instead of 5,931. Full-tree refresh is not faster. Noisy, load 5-11, compare within a run |
| agent-git-flow hook seam and adoption plan | seam done; adoption planned, not adopted | skill repo branch `feat/hooks-seam` 7ed3bfa and f73d9b6; `wt/adr126-s2-hooks` | skill suite 178; shell assertions 61 | `docs/plans/agent-git-flow-adoption-2026-11-08.md`. One round trip: `wt.sh new` 20.8 s, `land.sh` 0.3 s, diff gate 15 s |
| Seed store (ADR-130) | proven and live as of 2026-11-01 | mesh-mock `main` 0a05fe7; TF `gcp/seed-store` | gates 1 to 9 recorded in ADR-130 | Live state not re-read today: gcloud login has expired on the Mac |

Open S1-layer findings for aepod: F2 (grant and revoke race), F9 (audit ingestion has no call
site), F15 (secret in `Debug`), F16 (no SigV4 known-vector test), F17 (bucket name omits mesh id),
F18 (`store.promote` accepts empty prefix). Source: ADR-126 "Still open" on `wt/adr126-s2-hooks`.

## 4. Owner decisions

| Decision | Asked in | Unblocks | Who acts once decided |
|---|---|---|---|
| Park the S2 union mount (aepod 61b6e705) | next-steps section 8 Q1 | Removes it from the S2 done-when; `worktree-union-mount.test.sh` leaves the gates | aepod s2 |
| Scoped, non-root PG test key and a named `s2-drill-*` prefix | next-steps section 3 and Q2 | Two-machine S2 drill and the first PG run of the drive. Alternative: aepod binds a throwaway RustFS to `100.114.32.6` for one window | Owner mints on PG; the Mac and aepod run the drill |
| Spike teardown on PG: JuiceFS mounts, Redis, meshd, three spike buckets | next-steps Q3 | Frees PG; ends the October spikes | PG session runs `teardown.sh --purge` |
| ADR-126 Q2: archived bucket retention | ADR-126 "Open questions" | S2 promotion and `leftovers` thresholds | aepod s2 |
| ADR-126 Q3: cloud agent sessions write or read worktree stores | same | The S2 grant model | aepod s1 and s2 |
| ADR-131 Q1: waker-minted create-only inbox URLs | ADR-131 "Open questions" | Per-chain SC deploy | Owner (Terraform); aepod code |
| ADR-131 Q2: client data in GCS, encrypted, with KMS | same | D4, any client seed | Owner (KMS enable); aepod |
| ADR-131 Q3: bucket per client or prefix | same | S11 test pair; client seed layout | Owner (Terraform) |
| ADR-131 Q4: first project after WeftOS and first client project | same | Cutover order. Owner has said Sansone first (card `4c4319e0`); not recorded as an answer to Q4 | PG session |
| ADR-131 Q5: sparse-by-path on macOS in v1 | same | D1 design. weftos-d1 notes record this as received 2026-11-07, but the ADR still lists it open | weftos-d1 |
| ADR-131 Q6: Windows now or later | same | ProjFS work; the `/mnt/d` cost run | aepod Windows side |
| ADR-131 Q7: client git history as encrypted bundle or PG only | same | Client import design | PG session |
| ADR-131 Q9: weekday SC pre-warm | same | Avoids the 72 s wake. weftos-d1 notes record it as "owner-side, noted" | Owner (Terraform) |
| Accept ADR-131 | seed-drive plan section 6.3 | Closes ADR-128 G2 | Owner |
| Sign the `WMR2` chunk-table digest in the head | next-steps Q6; `manifest-v2.md` last section | Range join can be verified and turned on | Owner decides; mesh-mock code and SC deploy follow |
| Mesh-mock remote `weave-logic-ai/weftos-seed-cone`, who pushes `main`, where the three path deps (`weftos-tree-spike`, `weftos-integrator-spike`, `weftos-pq-sign`) get versioned | next-steps Q7; mesh-mock `docs/repo-plan.md` | Reproducible SC build; CI-host copy replaced by a clone | Owner; aepod diffs the CI-host copy against 0a05fe7 |
| agent-git-flow in WeftOS: adopt, and accept push of `target-0.8.5` on every land, or ask for a no-push mode first | next-steps Q8; adoption plan | The store hooks run on every new and land | Owner; then a PR for `.agent-git.env` |
| Drive index as plain file or RVF T2 container | `manifest-v2.md` decision 3 | Index keyed updates | Settled: plain file suffices for now (owner, per session memory); revisit if per-entry updates are needed |
| Audit webhook | `adr126-s3-materialise-watch` next step 6 | Small-write rate; the webhook capped writes near 43 a second in the mesh mock | Owner; PG session |
| Lift the store-capable weaver install stop on Mac, aepod and PG | next-steps section 3 | Daemon store env on PG, S1 live use, S5 live | Owner; then TF writes `store.env` |
| PG RustFS upgrade to a build carrying #9067 and #9073 | card `3f30aaf9` | Audit coverage; parser fix against a real record | Owner applies Terraform; PG session prepares |
| diff-gate approvals for the D1 crate's new deps and 2 `unsafe` lines in `devtools.rs` | `/tmp/weftos-d1-status.md` | Merging `tools/seed-drive-d1`; otherwise it stays an unmerged spike | Owner only; agents never edit the approvals file |
| Land S1 on `target-0.8.5`, then S2 and S5 rebased, then S3 and S4 | next-steps section 1 | Everything that needs the store crate in the workspace | Owner approves; aepod lands |
| `gcloud auth login` on the Mac, plus a quiet 30 min window | seed-drive plan section 7 | D1 and D2 timings, a live read of the head | Owner; weftos-d1 |

## 5. Hand-offs

| From | To | What | Pointer |
|---|---|---|---|
| weftos-fs-research | aepod s2 | Merge `wt/adr126-s2-mac`, `-hooks`, `-s3-drive` in order; the S2 fix file list | next-steps row 3c; evidence doc section "Fixes after the adversarial review" |
| weftos-fs-research | aepod s1 | F2, F9, F15 to F18; note the new `put_if_match` and `delete_if_match` trait methods that touch S1 code | review doc; next-steps row 9a |
| weftos-fs-research | aepod | Diff the CI-host mesh-mock copy against 0a05fe7, then replace it with a clone | mesh-mock `docs/repo-plan.md` |
| weftos-fs-research | aepod s3 | A short-lived credential scoped to one bucket and prefix, including put and delete on `<prefix>.drive-tombstones/` | `adr126-s3-materialise-watch` next step 5 |
| weftos-fs-research | PG session and aepod | Linux bwrap side of E-1, once the Mac interface exists | Pending; E-1 has no commits |
| weftos-d1 | weftos-fs-research | Sealing capture ops into a layer on the SC path. `driveidx::capture` returns put and remove ops; sealing is left to the caller | mesh-mock `manifest-v2.md`, "Capture". Whether the D1 daemon now consumes it is not verified |
| aepod s1 | PG session | A captured real audit record from PG's `audit.jsonl`, read-only | next-steps section 3 |
| PG session | Owner | RustFS upgrade plan, teardown list, import dry run of a weftos tree copy with the tree-hash check | next-steps row 11 |
| Owner | PG session | The scoped key, as a 0600 file path with a sha256, never the value | memory: secret-material-by-file-path |

## 6. Standing constraints

- No push. No Terraform apply. No production mounts. No Sansone migration.
- No restart of the weaver, the gateway, MLX or PG services. No store-capable weaver on the Mac, aepod or PG.
- Targeted tests only, no full workspace suite. CI on aepod is the green of record.
- No PG credentials on the Mac. Secret material moves by file path only.
- Diff-gate approvals are owner-only. Agents do not edit `config/diff-gate-approvals.toml`.
- Stage by path in the shared `~/weftos` checkout; Mac commits go on `wt/` branches.
- The seed-cone source is edited only through a `git worktree` of the mesh-mock repo, because weftos-d1 builds depend on its main checkout by path.

## 7. Not verified from this Mac

- Everything on aepod-xpc: branch tips beyond what the next-steps doc and memory say, test counts for S1, S3, S4, S5, the CI-host mesh-mock copy.
- Everything on photo-gallery: RustFS version and state, JuiceFS mounts, whether the checkout is still at 527ea4db.
- The live SC and head. gcloud login has expired, so the last live read is 2026-11-01.
- Any D1 or D2 timing. None has been taken.
- Whether the D1 daemon consumes the mesh-mock capture ops for sealing.
- Linux, Windows and two-physical-machine behaviour of S2 and the drive.

## 8. Where sources disagree

- **Name clash.** ADR-126 S3 is edge credentials (aepod `feat/adr126-s3`). The Mac branch `wt/adr126-s3-drive` is a different thing, the materialise-and-watch prototype. Read branch names, not slice letters.
- **Stale tips in the next-steps doc.** It names `wt/adr126-s2-mac` effb58881 in row 3c and 9db544992 for the drive in section 5; the branches are now at 220952fe9 and a757fe2cc. Session memory gives mesh-mock `feat/manifest-v2` as 323af44; the worktree is at d9cb33a after merging `feat/metadata-index`.
- **D1 state.** The header of `/tmp/weftos-d1-status.md` says "Nothing committed"; the branch holds commits through 363ebb3f7 and the same file records them. The next-steps doc cites D1 at 8439f55f6, which is the first commit.
- **Bucket stand-in.** The D1 status file says RustFS cannot stand in for the GCS bucket, then later records a RustFS-backed facade (`gcs2s3.py`) that supplies the generation compare-and-set itself. Both hold: RustFS alone cannot, the facade can.
- **Owner answers recorded in notes but not in the ADR.** ADR-131 Q5 and Q9 read open in the ADR; weftos-d1 notes record the owner's 2026-11-07 position. Q8 is answered per the next-steps doc and memory but ADR-131 still lists it.
