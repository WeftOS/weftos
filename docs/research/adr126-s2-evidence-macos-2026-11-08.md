# ADR-126 S2 evidence: real RustFS on macOS (2026-11-08)

Card 11b00a2e-4e82-4ba9-a5e6-eb70fb6dadde. Branch `wt/adr126-s2-mac`, based on `be64b6014`.
This is evidence for the gate in ADR-126 section 4a ("Gate for moving a project (Sansone) onto
shared worktrees"). It does not change the ADR text.

## What this run is, and is not

- **One Mac, two principals.** Two weaver daemons ran on this Mac with different node keys, runtime
  dirs and HOME directories, both pointed at the same RustFS. The acting principals were
  `agent-A@host-A` (writer) and `agent-B@host-B` (merger and follower), set through `WEFTOS_AGENT`
  and `HOSTNAME`. This is **not** two physical machines. That stays owed.
- **Real server, real disk.** RustFS 1.0.4, macOS arm64 (binary sha256 starts `723a13076b18046a`),
  single drive, no parity, console off, loopback only (`127.0.0.1:19311`), random credentials in a
  0600 file in the scratch dir. The data dir was on `/Volumes/ai-models` (external APFS volume).
  A single small PUT measured 1.4 to 4.4 s there while other builds ran. Every duration below is
  dominated by that, so treat the times as upper bounds for a laptop internal disk (the 2026-11-03
  spike measured 50 ms p50 PUTs on the internal disk).
- **Real git, real agent-git-flow.** Two scratch repos (one per principal) with real `wt.sh new`,
  real `land.sh` and real `git bundle` round trips. `AGF_REMOTE` was empty (local only).
- **Daemon build:** `scripts/build.sh native-debug`, debug profile, from this branch. Daemons were
  restarted onto each fixed build; the fix commits are listed at the end.
- **Clocks.** The daemons and the follower used the real clock. Where a drill needed an old, future
  or stale timestamp it is marked: either the test passed an explicit `now_ms` to the S2 library
  against the real server, or a registry value was edited in the real bucket. The macOS system
  clock was never changed.
- **Isolation.** Daemons ran with `HOME` and `WEFTOS_RUNTIME_DIR` under the scratch dir. Listings of
  `~/.weftos`, `~/.weftos/run` and `~/.clawft` were taken before and after. The only file changes are
  `run/kernel.log` and `run/cluster_peers.json` (plus the directory mtimes that follow from them),
  which the owner's own user daemon (pid 87190, started the day before, holds `kernel.log` open)
  writes. No file of mine is there.

## Results by gate item

| Item | Result | Notes |
|---|---|---|
| 1. Real RustFS, two principals | **Pass** (one host) | Below. Two physical machines still owed. |
| 2. Windows node | Not run | Out of lane. |
| 3. Metadata latency at Sansone scale | Not run | Only small trees measured (4 worktrees, 64 files). |
| 4. Leftover and handoff drill | **Pass** | Idle threshold by injected clock and edited registry; handoff on real flow. |
| 5. Failure drill | **Pass**, with two findings | Kills were real `SIGKILL`s of the daemon. |
| 6. Follower drills | **Pass**, with two defects fixed | One defect found per item below. |
| 7. ADR-131 D1 and D2 | Not run | Prerequisite owned elsewhere. |

### Item 1: publish, poll, search, sync-docs, claim, fetch, land

Real-RustFS tests, run with `scripts/build.sh test weftos-store --features rustfs --ignored --filter real_rustfs`
(this needed a new `--ignored` flag in `build.sh`; nextest has no environment switch for it):

| Check | Result |
|---|---|
| `s2_done_when::two_machines_share_a_worktree_on_a_real_rustfs` | Pass, 82 to 111 s |
| `s2_done_when` on the local-directory store (non-ignored) | Pass (part of the 98-test crate slice) |
| `rustfs::tests::live_object_and_bucket_operations`, `live_service_done_when`, `live_a_key_is_scoped_then_disabled_then_refused` | Pass |
| `rustfs::tests::live_listing_pages_past_a_thousand_keys` | Not finished, stopped by hand (see "Not finished") |

Script-driven flow with two daemons on one RustFS (`scripts/worktree-store.sh`, project
`01K15MKXFZSEK79RF74VWAHJ1K`, worktree slug `wt-rooms`: 2 commits, an untracked planning doc, a
`.env`, an ignored file):

| Step | Principal | Result | Time |
|---|---|---|---|
| `new rooms` | A | Lane and store created (first call also made the project store) | 33.0 s; later lanes 7.5 to 15.9 s |
| `share --handoff` | A | 6 files uploaded, bundle uploaded, `.env` skipped as Secret | 26.4 s |
| `follow`, polling every 5 s | B | First poll after the share returned reported `new wt-rooms [handoff]` | seen 1.36 s after `share` returned |
| `search rooms` | B | 1 hit (`docs/planning/rooms-plan.md`, title "Rooms plan") | 0.04 s |
| `sync-docs wt-rooms` | B | 2 docs mirrored | 0.32 s |
| `claim wt-rooms` | B | `claimed` by `agent-B@host-B` | 2.02 s |
| `fetch wt-rooms` | B | 6 files written, no conflicts, bundle head matched; commits under `refs/shared/wt-rooms/` | 0.23 s |
| git: reset lane to fetched head, `land` | B | `land.sh` landed 2 commits on master, then the store promoted | 22.9 s |
| `follow` | B | Saw `state_changed` to claimed 2.3 s after the claim | one poll |

**Follower latency.** The follower polled every 5 s. The poll before the publish finished saw
nothing; the first poll after the publish returned saw it 1.36 s later. A poll costs 70 to 640 ms
here (one list and one get per worktree). A publish is visible to followers within one poll
interval, once its registry entry is written, which is the last step.

**Landing.** The promote step found nothing to promote (the lane held no `assets/` objects) and
archived the shared tree, bundle, manifest and handoff record under `weftos-archive`.

Commands (every one with `WTS_WEAVER` set to a wrapper that exports the principal's `HOME`,
`WEFTOS_RUNTIME_DIR`, `WEFTOS_AGENT`, `HOSTNAME`; `AGF_CONFIG` pointed at a per-host config file):

```
scripts/worktree-store.sh new rooms
scripts/worktree-store.sh share --handoff "rooms ready: please merge"
scripts/worktree-store.sh follow --state <file>        # B, in a 5 s loop
scripts/worktree-store.sh search rooms
scripts/worktree-store.sh sync-docs wt-rooms <cache>
scripts/worktree-store.sh claim wt-rooms
scripts/worktree-store.sh fetch wt-rooms <git clone made with --no-checkout>
WTS_SLUG=wt-rooms scripts/worktree-store.sh land       # in a lane reset to the fetched head
```

### Item 4: leftover and handoff

**Leftover threshold.** There is no clock seam in the daemon (`now_ms` comes from the daemon).
Two ways were used, both against the real server:

| Kind | How the age was set | Result |
|---|---|---|
| Open, idle 4 days | Library test, `publish_tree` called with `now - 4 days` | `forgotten_open` |
| Handoff unclaimed 2 days | Same | `unclaimed_handoff` |
| Claim older than 30 min | Same, plus a **real** wait (below) | `abandoned_claim` |
| Merged entry still registered, 2 days | Same, and by editing `updated_ms` of the real entry, then `weaver store worktree leftovers` | `stuck_cleanup` |
| Writer a day ahead | Same, and by editing the real entry's `updated_ms` to now + 1 day | `future_timestamp`, idle shown as 0 |
| Fresh worktree | Same | not reported |

Test: `drill_leftovers_report_each_kind_and_a_stale_claim_is_taken_over`. The CLI output was
`stuck_cleanup idle 48 h wt-rooms [merged]` and `future_timestamp idle 0 h wt-big [open]`.

**Handoff drill (`wt-doors2`).** A shared with a handoff. B followed, claimed, fetched, reset a lane
to the fetched head and landed with `land.sh` (master moved from `57536f7` to `af9bdfa`, 1 commit,
11.8 s). The registry entry then read `merged`. Bucket deletion ran on A's daemon:

```
weaver store archive weftos://local/projects/<ULID>/worktrees/wt-doors2/store --delete
archived 11 object(s) ...; bucket deleted      (119.8 s on a contended disk; 61 s for wt-rooms)
```

After it the worktree bucket was gone from RustFS, `worktrees/wt-doors2.json` was gone from the
project bucket, and B's `follow` printed `gone wt-doors2`. Before the fix in `0317788ef` the entry
stayed `claimed` forever after a land and nothing ever removed it (see defects).

**Real-clock stale-claim takeover (`wt-wa`).** B claimed at epoch 1794120394 and its daemon was
then not used for that worktree again.

| Time after the claim | Action | Result |
|---|---|---|
| 114 s | A claims | refused: `claimed by agent-B@host-B` |
| 114 s | A (the writer) republishes | refused: `claimed by agent-B@host-B` |
| 1359 s | A claims | refused |
| 1810 s | `leftovers` | `abandoned_claim wt-wa` |
| 1810 s | A claims | succeeded, merger `agent-A@host-A`, registry updated |

### Item 5: failure drill

All kills were `kill -9` of the weaver daemon that was doing the work. The CLI printed
`daemon closed connection without response`.

| Drill | What happened | Lost? | Recovery |
|---|---|---|---|
| Writer killed mid-publish, first publish of `wt-big` (60 files of 40 KB plus docs), killed 18 s in | 54 of 64 tree objects and `meta.json` were in the bucket. No manifest, no registry entry. Followers saw nothing of it. | No | Restarted daemon, reran `share`: 64 uploaded, 0 unchanged, then `fetch` from B matched the source tree byte for byte (same combined sha1 of the 60 files). |
| Merger killed mid-claim (`wt-wb`, merger A), killed the instant `handoff.json` changed on disk | Handoff said `claimed` by A. Registry still said `handoff`. B's follower saw `handoff`. | No | Restarted A; A's own `claim` succeeded and the registry became `claimed`. A third principal (B) was refused: `claimed by agent-A@host-A`. |
| Daemon killed during a registry refresh (republish of `wt-big` after a new commit), killed the instant the manifest changed | Manifest had the new head (`b229516e`); the registry still had the old head (`524ee83`). B's follower saw no `head_moved`. | No | Restarted A, reran `share`: 0 uploaded, 64 unchanged. Registry head became `b229516e`; B's follower then printed `head_moved wt-big`. |
| Stale claim | Covered in item 4 with a real 30 minute wait. | No | Takeover after 1810 s. |

Findings from this item (not fixed here):

1. **The registry can lag the worktree after a crash.** Between the data write and the registry
   write there is a window where followers see the old head or state. It heals on the next verb for
   that worktree (publish, claim or promote). No verb just re-derives the entry, and nothing does it
   on daemon start. A `store worktree refresh` verb, or a pass at daemon start, would close it.
2. **A killed first publish uploads everything again.** "Unchanged" is computed from the previous
   manifest, which does not exist yet. Correct, but wasteful for a large tree.

### Item 6: follower drills

| Drill | Real-server evidence | Result |
|---|---|---|
| Writer a day ahead | Library with `now + 1 day`, and a CLI run on an edited entry | `future_timestamp`, not treated as idle or as fresh |
| Corrupt entry and unknown-schema entry | Both written into the real project bucket. Library: strict listing errors, lenient listing names both, `poll` reports `unreadable` for each every poll and keeps the good worktree, clears when the bad entries are removed. CLI: `follow` printed `unreadable corrupt` and `unreadable future` | Pass for `follow` and `leftovers`. **`search` failed outright** before the fix (`Error: json: key must be a string at line 1 column 3`). After `4cb205eb8` it returns the hits and prints `(could not read registry entry corrupt)`. |
| Empty project bucket | Created an empty project bucket; CLI `follow` with a state file that knew two worktrees: `Error: invalid: registry is empty but 2 worktree(s) were known; wrong project bucket?`; state file unchanged. `--confirmed` then reported both `gone`. | Pass |
| Wrong (nonexistent) project bucket | CLI: `Error: backend: no such bucket p-...`; library: error and state untouched | Pass |
| Merged entry whose cleanup never ran | `wt-rooms` entry aged by 2 days: `stuck_cleanup`. `archive --delete` on the owning daemon removed it and `leftovers` went empty. | Pass (after the `0317788ef` fix) |
| Interrupted `sync-docs`, then a deleted doc | Test `drill_interrupted_sync_docs_then_a_deleted_doc_converges`: index lists a new doc that was never fetched, a doc half written, then the writer deletes two docs; the next run leaves exactly `docs/a.md` and the index. The interruption is simulated by writing the state a crash would leave, not by killing a process. | Pass |
| `overlaps`, different bases | Test `drill_overlaps_with_different_bases_over_reports`: 20 files, base 2 had upstream edits to 5 of them, one real conflict. Reported 9 paths: 1 real conflict, 8 false positives (89 percent). `same_base` was `false`. CLI on two real lanes (`wt-wa` base `524ee83`, `wt-wc` base `af9bdfa`) reported `src/lib.rs` with `same_base: false`. The manifests' `changes` lists would remove all 8 false positives; `overlaps` does not use them for content differences. | Pass as documented; the false-positive rate is the measurement the gate asked for |
| Two overlapping `follow` runs on one state file | Eight concurrent CLI `follow` calls on one `--state` file, three rounds. Before the fix 7 of 8 failed each round with `follow state ...: No such file or directory`. After `791f3e497` 0 of 24 failed and the file parsed. Library drill: two threads, 12 loops each, the shared file always parsed and the slug was reported as `new` at most twice (asserted). | Fixed |

## Defects found and fixed on this branch

| Commit | Defect | Evidence |
|---|---|---|
| `be63d8ab8` | `worktree-store.sh new` could not find the lane: it parsed `wt.sh list`, which prints lane names, not paths, so it ran `cd rooms` and the store was never created. Fixed by taking the lane path from `wt.sh new`'s stdout. The shell test did not cover `new`; two assertions now do and fail on the old script. | `cd: rooms: No such file or directory` |
| `0317788ef` | After a land, the registry entry stayed `claimed` (promote never refreshed it), and nothing ever removed an entry, so every landed worktree became a permanent `abandoned_claim` leftover. Promote now refreshes the entry to `merged`; `store archive --delete` of a worktree store removes its entry. | Registry read `claimed` after the land; entry gone after the fix |
| `791f3e497` | `FollowState::save` used a temp name per process. The daemon serves overlapping `follow` calls from threads of one process, so concurrent calls on one state file raced on the temp file. Now unique per call. New test fails on the old code. | 7 of 8 concurrent follows failed |
| `4cb205eb8` | `store.worktree.search` used the strict registry listing, so one corrupt or unknown-schema entry broke search while `follow` and `leftovers` survived. `store.worktree.create` failed when the project store existed in the backend but not in this daemon's own state (a second daemon on the same RustFS), and `worktree-store.sh new` swallowed it. | `Error: json: key must be a string...`; `wt-wb` bucket missing |
| `98ce3c6f3` | `scripts/build.sh test --ignored` added so real-service tests run through the script. | n/a |
| `cfbb5e7d2` | New `tests/s2_real_rustfs_drills.rs` (six ignored tests, real RustFS, injected `now_ms`). | All six pass |

Each fix has a test: shell assertions for `new`; `store_worktree::tests` for the registry refresh,
`forget`, search and second-daemon create; `s2_follower` for concurrent saves.

## Other findings, not fixed

1. **Lifecycle verbs belong to the daemon that created the store.** B's daemon answers
   `unknown store` to `store archive` for a worktree store A created, because store records and
   grants live in each daemon's own state file, not in RustFS. The S2 verbs work across daemons
   because they use the raw data plane, so only the bucket deletion step had to run on A. If more
   than one daemon is meant to manage one store, this needs a decision (shared state, or one store
   owner that others call). With the ADR's one store service per account it may be a non-issue.
2. **The registry is not reconciled** (finding 1 under item 5).
3. **`store.worktree.*` verbs record no chain event**, although the module header says the ones
   that change shared state do. Not checked further.
4. **On macOS the CLI host comes from `$HOSTNAME`** (it reads `/proc/sys/kernel/hostname`, which
   does not exist). Where that variable is unset, every Mac principal is `<agent>@unknown`. A
   process-free fix needs libc `gethostname`, which the diff gate treats as FFI, so it was left.
5. **`s2_done_when` real-RustFS time:** 82 to 111 s on this disk for a scenario of about 25
   objects, so roughly 1 s per object write.

## Not finished

- `rustfs::tests::live_listing_pages_past_a_thousand_keys` (an S1 test) writes 1,005 keys. At the
  write latency of this disk (about 3.3 s per PUT by then) it had written 569 keys after 31
  minutes, with 1,005 deletes still to come. It was stopped by hand, so it is neither a pass nor a
  fail. It tests S1 paging, not the S2 gate, and should be run on a faster disk.

## Still owed

- Two physical machines (item 1 as written).
- Windows (item 2) and the ProjFS drive.
- Metadata latency for a Sansone-sized worktree (item 3).
- ADR-131 D1 and D2 (item 7).
- A re-run of the timing numbers on an internal disk.
- A clock-skew drill that changes a real machine's clock (here only stored timestamps were skewed).

## Commits on `wt/adr126-s2-mac`

```
be63d8ab8 fix(store): worktree-store.sh new finds the lane from wt.sh's stdout path, not its name-only list
98ce3c6f3 test(store): build.sh test --ignored runs #[ignore]d real-service tests through nextest
0317788ef fix(store): worktree promote refreshes the registry to merged; archive --delete of a worktree store drops its registry entry
791f3e497 fix(store): FollowState::save uses a unique temp name per call so overlapping follows in one daemon do not lose each other's file
4cb205eb8 fix(store): worktree create succeeds when another daemon made the store; search names corrupt registry entries instead of failing
cfbb5e7d2 test(store): real-RustFS drills for leftovers, stale claims, corrupt entries, wrong bucket, shared state file, overlaps, interrupted sync-docs
```

Tests run: `scripts/build.sh test weftos-store --features rustfs` (98 passed, 11 ignored),
`scripts/build.sh test clawft-weave --filter store` (39 passed), `bash scripts/tests/worktree-store.test.sh`
(39 assertions passed). No full workspace suite was run, and clippy was not run on the changed crates.
