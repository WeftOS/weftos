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
| weftos-fs-research | This Mac session; Mac worktrees | `wt/adr126-s2-mac` 220952fe9; `wt/adr126-s2-hooks` af688ca60; `wt/adr126-s2-meta` 8f4652c16; `wt/adr126-s3-drive` a757fe2cc; mesh-mock `feat/manifest-v2` d9cb33a | See rows below. S2 macOS evidence, 25 defect fixes, hook seam, metadata bench, drive library and verbs, mesh-mock manifest v2, range join, cache, drive index, adoption plan, repo plan | E-1 scoped view for Tapestry R9 done on `wt/adr126-e1-scoped-view` 4455d1cdc and its `weaver store view` verbs on `wt/adr126-e1-view-verbs` 22431e4be (see the E-1 rows). Drive hardening done on `wt/adr126-s3-harden` 4082527e8, hardening 2 and tombstone collection done and merged on `wt/adr126-s3-harden2` b018f71db (see the three drive rows) | aepod merging the Mac branches; PG credentials; owner decisions in section 4 |
| weftos-d1 | Mac worktree `adr131-d1` | `feat/adr131-d1` 363ebb3f7, plus uncommitted edits to `drive.rs`, `drive_tests.rs`, the ADR-131 text | D1 daemon spike; kill points 1 to 3 pass offline; kill point 4 passes against a loopback bucket; refresh redo hazard fixed (SEEDDRIVE_REDO default strict, owner, 2026-11-08); D2 design and overlay round trip pass (`docs/research/adr131-d2-linux-drive-design.md`; `/tmp/weftos-d1-status.md`) | Head-lineage and truncated-copy options, one-command harness `d1_run.py` | **No D1 or D2 timing exists.** Needs `gcloud auth login`, a quiet 30 min window, and the diff-gate decision on the crate's new deps |
| aepod weftos-ci | aepod-xpc sessions s1 to s5 | `feat/adr126-s1..s5` on `~/weftos-wt/s1..s5`, unpushed `integ/adr126-s1-s4`; S2 +4 and S3 +3 over origin | Code for all five slices (per next-steps doc section 1) | Not verified from this Mac | Landing order S1 first; F2, F9, F15 to F18 open; merging the Mac branches; Windows bucket load never run |
| photo-gallery (weftos-pg) | PG session as `mathew`; PG tailnet 100.90.170.87 | Checkout `/data1/home/mathew/weftos` at 527ea4db | Host survey; S0 proofs in ADR-126 "S0 results" | RustFS 1.0.4 live on `:9700`, with JuiceFS mounts, Redis `:9710`, meshd `:9720` still up | PG RustFS upgrade (card `3f30aaf9`, upstream fixes #9067 and #9073 merged upstream, not deployed); no PG credentials on the Mac; Linux bwrap side of E-1 (handoff pending); the owner's pre-PG asks in section 4 |

Merge order for aepod: `wt/adr126-s2-mac`, then `wt/adr126-s2-meta`, then `wt/adr126-s2-hooks`, then
`wt/adr126-s3-drive`, then `wt/adr126-s3-harden2`, then `wt/adr126-s1-names` (pending owner confirmation, see section 4; S1 code: `name.rs`, `service.rs`; aepod s1 reviews), then `wt/adr126-e1-scoped-view`, then
`wt/adr126-e1-view-verbs`. `wt/adr126-s3-drive` already contains `wt/adr126-s2-mac`.
`wt/adr126-s2-meta` (tip 8f4652c16, 2 commits over be64b6014, the same base as the first-pass
`wt/adr126-s2-mac`) touches only `scripts/bench/adr126_s2_meta_gen.py`,
`scripts/bench/adr126_s2_meta_s3.py` and `docs/research/adr126-s2-metadata-latency-2026-11-08.md`
(563 insertions, no crate code), so it conflicts with nothing and can merge any time after S2 lands.
`wt/adr126-s2-hooks` has diverged from the base (7 commits over be64b6014, touching
`scripts/worktree-store.sh`, the ADR-126 text and the review), so expect a script and ADR text
conflict check. `wt/adr126-s3-harden2` contains `-harden` and `-tombstone-gc`.
`wt/adr126-e1-view-verbs` is today's in-progress branch: a merge of `-e1-scoped-view` and
`-s3-harden2`, plus the `weaver store view` verbs when done.

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
| Drive hardening (Mac) | done 2026-11-08 | `wt/adr126-s3-harden` 4082527e8 (4 commits over a757fe2cc) | weftos-store rustfs 141 to 148 passed, 13 to 20 ignored (7 live scenarios on loopback RustFS 1.0.4, all pass) | "Hardening 2026-11-08" in `docs/research/adr126-s3-materialise-watch-2026-11-08.md` on that branch. Racing remote write (three orders): stale put refused with 412, becomes a conflict, remote bytes never overwritten. Tombstone delete, recreate, delete-again cycle works with `If-None-Match` and `If-Match`. Killed watch loop: restart uploads nothing twice, index matches the store. One bug fixed (cbabe7727: a pull over a locally modified file adopts equal bytes instead of a false conflict). Crash is simulated in-process at the tip because the diff gate rejects new process spawning; the real SIGKILL form is in 7f4eac450 and needs an owner gate approval. Not done: stale `.drive.tmp.*` cleanup, fsync before the index rename, tombstone collection, two real machines. Live scenarios 1 to 2.4 s each, noisy, load 6-7 |
| Drive hardening 2 (Mac) | done 2026-11-08 | `wt/adr126-s3-harden2` cf7495591 over 4082527e8; `drive/tmp.rs`, `tests_durable.rs`, `index.rs`, `materialise.rs`, `cas.rs` | weftos-store rustfs 148 to 153 passed, 20 ignored; 9 live drive tests pass on loopback | "Hardening 2" in `docs/research/adr126-s3-materialise-watch-2026-11-08.md` on that branch. Stale `.drive.tmp.*` removed at open (`materialise`, `sync_down`) when older than `Options.stale_tmp_age` (10 min default, an owner guess), live temps registered process-wide and never removed, symlinks never followed, count in `DownReport::stale_tmp_removed`. `Index::save` fsyncs the temp, renames, fsyncs the directory. In-process crash tests: death before the rename keeps the old index and the aged temp is removed at the next open; death after the rename loads the new index. Not covered: a real power cut (fsync order read from the code, not proven), a death inside `place_file`, `sync_up` and `watch` do not clean |
| Tombstone collection (Mac) | design plus default-off prototype, 2026-11-08 | `wt/adr126-s3-tombstone-gc` 565e0c8fc over 4082527e8; `drive/gc.rs`, `tests_gc.rs`, hook in `sync_up.rs` | weftos-store rustfs 148 to 157 passed, 20 to 21 ignored; live cycle passes on loopback | `docs/research/adr126-s3-tombstone-collection-2026-11-08.md` on that branch. `Options.collect_tombstones` default off (pinned by a counting backend); age from the marker body `ts_ms`; `delete_if_match` on the listed etag, never a blind delete; cap 500 markers per pass; a collection error never fails the sync. Retention assumption: a marker older than R is collected; `PLACEHOLDER_RETENTION` 7 days, the owner picks R. Device offline longer than R: unchanged behaviour (absence from the listing is the delete signal), only the deleter's name is lost in a later conflict record. Not solved: no device registry, no clock authority, one extra tombstone DELETE per create, two real machines |
| Drive branches merged (Mac) | merged and gated 2026-11-08 | `wt/adr126-s3-harden2` b018f71db = hardening 2 plus tombstone collection (conflicts in `drive/mod.rs` `Options` and `tests.rs` resolved by hand) | weftos-store rustfs on the merge: 162 passed, 21 ignored (the one leaky test is the pre-existing `audit::tests::a_denial_line_captured_from_rustfs_1_0_4_is_read`, not drive code); clippy clean; `diff-gate.sh --worktree --base 4082527e8` OK, 13 files, 0 approved. Live tests not rerun on the merge | aepod merges in the order of section 2: `wt/adr126-s2-mac`, `wt/adr126-s2-meta` (docs and bench scripts only), `wt/adr126-s2-hooks`, `wt/adr126-s3-drive`, then `wt/adr126-s3-harden2` (which contains `-harden` and `-tombstone-gc`), then `wt/adr126-e1-scoped-view` (from a757fe2cc; expect a `drive/mod.rs` conflict with the view module line and `SkipWhy::Denied`), then `wt/adr126-e1-view-verbs` |
| `weaver store view` verbs (Mac) | done 2026-11-08, tests only, no weaver installed or run | `wt/adr126-e1-view-verbs` 22431e4be = merge of `-e1-scoped-view` and `-s3-harden2` (bc87b13ed, fallout fix 8f47ad834) plus `crates/clawft-weave/src/store_view.rs`, `store_view_tests.rs`, `commands/store_view_args.rs` | weftos-store rustfs 185 passed on the merge; clawft-weave store 61 to 76; one live handler-path cycle on loopback RustFS | Spec section 6.3 on that branch: `open|status|refresh|publish|resolve|renew|close`, `--spec <file>` sent as a param (the daemon opens no caller-named path) or flags, `--read-only` drops the work tree so no lease and `publish` refuses, records with counts for open, publish, resolve, close; `status`, `refresh`, `renew` record nothing. Live, noisy, load 6-7: open of 60 files 3.5 s, refresh 159 ms, publish 416 ms, close 812 ms. Not done: deriving the spec from the session's grant (operator supplies it today); whether `renew` should be recorded is an owner call. 2026-11-08 PM, for the Tapestry item 10 lane: `view.json` now carries `deny` (the applied list, additive, schema 1; 068ea6723); answers to its six contract questions were sent to the Tapestry session. Open owner call it raised: RPC refusals are a single error string (the `ViewError` text), not a `{code, message}` object; adding a stable `code` would change the drive verbs' error shape too |
| Bucket rename `n-<id>` to `node-<id>` and one pinned bucket creator per service (Mac, S1 code for aepod s1). **Pending owner confirmation**: the owner's decision covers RustFS user names and user creation, not buckets | built 2026-11-08 | `wt/adr126-s1-names` 6736b94eb over b018f71db; `name.rs`, `service.rs`, `lib.rs`, `service_tests.rs` | weftos-store rustfs 162 to 166, 21 ignored; clippy and `diff-gate --worktree --base b018f71db` clean (9 files, 0 approved); clawft-weave untouched | `docs/research/adr126-s1-names-2026-11-08.md` on that branch. Node bucket `n-<id>` becomes `node-<id>` (id cap 58 so the bucket stays at 63; `p-`, `w-`, `r-` untouched; ADR-126 table row updated). "One creator per user" read as one daemon identity per service instance because the crate has no user notion: the first `create_store` pins `node:<node id>` in the persisted state and tags the bucket `creator`; a create under another node id returns `StoreError::CreatorMismatch` before any bucket is made; `ensure_store` on an existing store is not refused. Migration not done: PG's existing `n-<id>` bucket is not renamed; a daemon on this code would create an empty `node-<id>` and orphan the old data, so a one-off copy or rename comes first |
| Mac integration branch | built, tested, live-proven 2026-11-08 PM | `wt/adr126-mac-integration` d003ba3d1 = `-e1-view-verbs` + `-s2-meta` + `-s2-hooks` + `-s1-names`, conflicts resolved, e1 handoff doc aligned to `target-0.8.5`; merge-tree against `target-0.8.5` clean | weftos-store rustfs 189 passed, 22 live passed on loopback RustFS; clawft-weave store 76 plus 2 live; shell 63 of 63; clippy and gate clean | `docs/research/adr126-mac-integration-2026-11-08.md` on the branch. ADR-126 header updated there (Mac branches unlanded list). Landing option for aepod after S1: this one branch instead of eight; names amendment still pending owner confirmation (revert path documented) |
| D1 Mac drive daemon | functional only | `feat/adr131-d1` 363ebb3f7 | 72 targeted tests at last check | Kill-criterion timings not taken (warm `git status` at most 2x local, refresh at most 1 s, edit to seal at most 3 s) |
| D2 Linux drive | design plus overlay spike | same branch, `tools/seed-drive-d1/src/overlay.rs` | 12 overlay tests | Round trip upper dir to layer to bucket to second drive passes in both directions. CI-host reflink support unmeasured |
| E-1 scoped view | library, spec and handoff done 2026-11-08; verbs and Linux side not | `wt/adr126-e1-scoped-view` 4455d1cdc (4 commits over a757fe2cc): `crates/weftos-store/src/drive/view.rs` plus `view/{spec,lease,tree,work}.rs`; spec `docs/architecture/store-scoped-view.md` (322 lines) | weftos-store rustfs 141 to 164 passed, 13 to 14 ignored (23 view unit tests, 1 live cycle on loopback RustFS); clawft-weave store 61 unchanged | Materialised scoped view: `base/` read-only (hard links to the drive files), `/work` as clone or overlay upper, `.state/` never bound, control keys filtered at any depth, single-writer lease at `.drive/leases/<hash>.json` for writable views only, publish stages each edit against the etag it was made from (raced edit becomes a 412 conflict even after a refresh). Live solo run, noisy, load 3-10: open of 300 files 5.3 s (about half is the view's own local work, unprofiled), refresh 51 ms, publish 140 ms, close 147 ms. Not done: `weaver store view` verbs (spec 6.3), anything bwrap, overlayfs or Landlock (Mac has none), whiteouts unexercised, opaque directories not detected (decided 2026-11-08: read with the `xattr` crate, no unsafe; a new dependency, so it needs the owner's diff-gate approvals entry first; implementation pending), overlay mode does not refresh `/work` while mounted. Handoff for the Linux side: `docs/research/adr126-e1-scoped-view-handoff-weftos-pg-2026-11-08.md` (copied to `target-0.8.5`) |
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
| ~~ADR-126 Q2: archived bucket retention~~ Answered 2026-11-08: archives do not expire; no retention threshold | ADR-126 "Open questions" | S2 promotion and `leftovers` need no age threshold | aepod s2 |
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
| E-1 scoped view (spec section 9): deny `.drive/leases/*` writes in `policy.rs`; per-prefix STS credentials so the read-only half is enforced at the store; lease TTL and renewal cadence; overlay opaque xattr (decided 2026-11-08: `xattr` crate, no unsafe, implementation pending the approvals entry; section 9 question 4 is closed); whether refreshing the work prefix only at the next open is acceptable for overlay mode; where view roots live; reword Q9/R9 ("scoped view" is what gets bound, not a mount) | `docs/architecture/store-scoped-view.md` section 9 on `wt/adr126-e1-scoped-view` | The Linux side (weftos-pg) and the `weaver store view` verbs; merging the branch after `wt/adr126-s3-drive` | Owner decides; Mac builds verbs; weftos-pg builds the Linux side; aepod merges |
| Drive hardening: `stale_tmp_age` (10 min guess); real SIGKILL crash test needs a gate approval for `tests_crash.rs` (process spawning) | `adr126-s3-materialise-watch` "Hardening" and "Hardening 2" sections on `wt/adr126-s3-harden2` | Restoring the real-kill form of the crash test | Owner approves or leaves the in-process form |
| Tombstone retention R; collection in `sync_up` (as built) or only in the ADR-131 daemon loop; whether a `stale_local` state is wanted (design says no) | `docs/research/adr126-s3-tombstone-collection-2026-11-08.md` sections 2, 3, 6 | Turning `collect_tombstones` on anywhere | Owner picks R; Mac flips the default and the hook placement |
| Bucket prefix rename `n-<id>` to `node-<id>` and the pinned bucket creator on `wt/adr126-s1-names`: keep, drop, or adapt to the owner's decision (RustFS user names are `node-<id>`, option B; `wk-<hash>` aliases are designed by S1 on aepod; only one lane creates each RustFS user). Also when and how to migrate PG's `n-<id>` bucket if kept | `docs/research/adr126-s1-names-2026-11-08.md`; ADR-126 section 3 amendment on `wt/adr126-s1-names` | aepod s1 merging `wt/adr126-s1-names`; the PG bucket rename | Owner decides; aepod s1 merges; PG session renames under owner gate |
| Record `weaver store view renew` in the activity log and chain (today it records nothing, like the drive's claims) | spec section 6.3 on `wt/adr126-e1-view-verbs` | Nothing blocked; audit completeness | Owner; Mac adds the record |
| Snapshot cadence | owner decisions 2026-11-08 | Still open; nothing built assumes a cadence | Owner |
| Tapestry Phase 3 questions D1 to D6 | owner decisions 2026-11-08 | Still unanswered. The loopback test hub is approved and is not D1 | Owner |
| Grant signing: decided 2026-11-08, any other node or person may sign a grant and the daemon verifies and applies it. ADR-126 section 3 on `target-0.8.5` now carries the amendment line; the spec's section 3 note on `wt/adr126-e1-view-verbs` says the same. The grant-signing code path (who verifies, which key) is S1 work for aepod | ADR-126 section 3 amendment; spec section 3 | S1 grant model; S2 cloud-session reads (ADR-126 Q3 still open) | aepod s1 implements; no owner call left here |
| `xattr` crate for overlay opaque directories: add the owner's own entry to the diff-gate approvals (a new dependency) | decided 2026-11-08 | Implementation of opaque directory detection | Owner only; then Mac |
| Structured RPC errors for `store.view.*` and `store.drive.*` (`{code, message}` instead of one string), asked by the Tapestry item 10 launcher so it can branch on `LeaseHeld`, `Unpublished` and so on | this page; Tapestry lane message 2026-11-08 PM | A launcher that reacts to refusals without parsing text. The Tapestry lead (backend:4) answered 2026-11-08 PM: do not add it now; their host matches the error text conservatively and tests only against recorded replies (no weaver run); recorded as an owner call in their item 10 handoff too | Owner, later; nothing blocked |
| `gcloud auth login` on the Mac, plus a quiet 30 min window | seed-drive plan section 7 | D1 and D2 timings, a live read of the head | Owner; weftos-d1 |

## 5. Hand-offs

| From | To | What | Pointer |
|---|---|---|---|
| weftos-fs-research | aepod s2 | Merge `wt/adr126-s2-mac`, `-hooks`, `-s3-drive` in order; the S2 fix file list | next-steps row 3c; evidence doc section "Fixes after the adversarial review" |
| weftos-fs-research | aepod s1 | F2, F9, F15 to F18; note the new `put_if_match` and `delete_if_match` trait methods that touch S1 code | review doc; next-steps row 9a |
| weftos-fs-research | aepod | Diff the CI-host mesh-mock copy against 0a05fe7, then replace it with a clone | mesh-mock `docs/repo-plan.md` |
| weftos-fs-research | aepod s3 | A short-lived credential scoped to one bucket and prefix, including put and delete on `<prefix>.drive-tombstones/` | `adr126-s3-materialise-watch` next step 5 |
| weftos-fs-research | PG session (weftos-pg) | Linux bwrap side of E-1: launcher shim from `view.json`, Landlock inside the bwrap child, refresh trigger, checks C2 isolation, overlay end to end, opaque directory, clone route, hard links; run on `weftos-ci` with a throwaway RustFS; report to `docs/grok-build-fork/spike-results/` | `docs/research/adr126-e1-scoped-view-handoff-weftos-pg-2026-11-08.md` on `target-0.8.5`; spec on the branch |
| weftos-fs-research | Tapestry backend:4 (read-only pointer, no edits by us) and weftos-pg | E3 scoped-view alignment: frozen `view.json` schema 1 contract, sequence table (who calls what for open, run, refresh, publish, close), item 10 and e2e check mapping, failure modes both sides must agree on, the Q9/R9 one-line rewording proposed not applied, six gaps (no root-image vs project flag in `view.json`, lease renewal owner unstated, close order unstated, under-root path check left to the launcher, who supplies the spec for a Tapestry session, WSL view roots must be on ext4 not 9p) | `docs/research/adr126-e3-scoped-view-alignment-2026-11-08.md` on `target-0.8.5` |
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
- **Owner answers recorded in notes but not in the ADR.** Resolved for Q5, Q8 and Q9: ADR-131 now records Q5 and Q8 as answered on 2026-11-07 and Q9 as noted owner-side, still an owner call and not scheduled. The next-steps doc cites "ADR-126 section 4a" for the Q8 answer; that section exists only on `wt/adr126-s2-hooks` (fc7e0c0e5 adds "4a. The L1 follower view"), not yet on `target-0.8.5`, so the citation resolves once that branch lands.
- **`wt/adr126-s2-meta` placement.** Resolved: it is in the merge order in section 2, second, after `wt/adr126-s2-mac`.

## 9. Left open at close-out (2026-11-08, afternoon)

Owner release note in force: no local testing release, no tag, no push, no install over the
production weaver; the RustFS and Tapestry release happens all at once later.

| Open item | Where it stands | Who moves it |
|---|---|---|
| Landing the Mac branches | Eleven `wt/adr126-*` branches, none pushed, order in section 2. All targeted tests, clippy and worktree diff gates pass at each tip. Live tests ran on loopback RustFS only | aepod (S1 first, owner approval), then the Mac branches in order |
| Linux side of the scoped view | Launcher shim, Landlock, five e2e checks on `weftos-ci`; contract frozen in the alignment note | weftos-pg; Tapestry backend:4 for item 10 |
| Spec derivation from the governance grant | Operator supplies `--spec` today | Mac, after the owner answers spec section 9 question 2 |
| Real-SIGKILL crash test | In 7f4eac450, needs a gate approval for process spawning | Owner |
| Tombstone retention R and hook placement | Placeholder 7 days, collection default off | Owner |
| PG `n-<id>` bucket migration | Not renamed; orphan risk stated above | Owner gate, PG session |
| D1 and D2 timings | None exist; needs `gcloud auth login` and a quiet window | Owner, weftos-d1 |
| S1 findings F2, F9, F15 to F18 | Open | aepod s1 |
| `weaver store drive` and `view` against a live daemon | Never run; the owner's no-install rule stands | After the release |
| ADR-126 Q3, ADR-131 Q1 to 4, 6, 7, 9; mesh-mock remote and path-dep versioning; WMR2 digest signing; agent-git-flow push-per-land | Unchanged since the morning status | Owner |

Owner decisions 11:21 to 11:43 applied (2026-11-08): the opaque-directory question now reads as decided (`xattr` crate, no unsafe, implementation pending the approvals entry) in the spec, the alignment note and the handoff; the `wt/adr126-s1-names` row is marked pending owner confirmation because the bucket rename went beyond the RustFS user-name decision; ADR-126 Q2 is answered (archives do not expire); SEEDDRIVE_REDO default strict is recorded on the weftos-d1 row; the grant-signing decision is listed as a wording gap. Still open: snapshot cadence, Phase 3 D1 to D6, ADR-126 Q3, spec section 9 questions 1, 2, 3, 5, 6, 7 and the RustFS user-name alias design (S1, aepod).
