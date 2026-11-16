# WO-069 gate runbook: ADR-126 section 4a, moving a client project onto shared worktrees

Lane F2, 2026-11-15, base `1fca1a1f9` (branch `w2/wo069-f2-gate-runbook`). This is the run plan and the scripts for the evidence the
gate still owes. **No script in `scripts/store/wo069-gate-*` has been run for real**; each was tested with `--help`, `--dry-run`
and `scripts/tests/wo069-gate-scripts.test.sh` only. Lane F3 (2026-11-15) added the follower-half test `crates/weftos-store/tests/s2_follower_half.rs` and the `fh` step of `scripts/store/wo069-drills.sh`; `--only fh --release` was run for real on the Mac against a throwaway local RustFS (one machine, one clock; the two-machine path in item 1 is still unrun). Expect first-run defects and read the "first run" risks before trusting a result.
Sources: ADR-126 section 4a (gate items 1 to 8), the drills report `wo069-4a-drills-20261114.md`, the harness in `scripts/store/wo069-drills*.sh`.

## Constraints (they shape every step)

- **Hosts.** The MacBook (writer, macOS arm64). `aepod-xpc` (weftos-ci WSL: Linux x86-64, rootless podman, RustFS 1.0.4 image already
  local; the Windows side of the same box for item 2). `photo-gallery` (PG): PRODUCTION. Its RustFS, weftos, meshd and cog-host are
  never touched or restarted; no script here targets it, every script refuses its address, name or port (exit 4), and a throwaway
  server on a PG scratch port is avoided by this plan.
- **Owner hold, round-3 question 3.** A follower DAEMON on aepod-xpc (a store-capable weaver there; the installed one is 0.8.4 with no
  `store` command; the CI rule says a live check in a container never uses a store-capable weaver) is open. Every step that needs it is
  marked **needs owner go (Q3)** and the scripts refuse it unless `WO069_OWNER_GO_Q3=1` (exit 3). Where there is an alternative that
  needs no decision, it is the default. Note for the owner: ADR-126 records that on 2026-11-09 19:44 ET the owner lifted the install
  stop on store-capable weavers; whether that already answers Q3 is the owner's to say, the scripts do not assume it.
- **The client project's real tree is outside this lane.** Never read it. Item 3 is a script the owner (or the session that owns the
  tree) runs locally; it prints counts and sizes and no path or name; nothing is uploaded.
- **No PG store, no production key.** Credentials only as 0600 files, generated on the spot, never on a command line, never printed,
  deleted afterwards (`down` removes them and checks). The credential directory is on the notes volume, never `/tmp` or `$HOME`.
  Scratch ports are loopback only; the second host is reached through one ssh forward. Everything created on a remote host is removed
  and the removal is checked.
- **Nothing is built on the Mac for this.** Owner rule 2026-11-09: lane builds run in podman on weftos-ci. The Mac weaver for these drills
  is an existing macOS build of the integration branch (for example the one the benchmark worktree already has), accepted only after
  `wo069-gate-item8-writers.sh --min-commit <fence commit> local=...` says OK for it. Linux builds: `ci-push.sh` / `ci-run.sh`
  from `/Volumes/ai-models/research/weftos-agent-notes/scratch/ci/README.md` (targeted tests only, never `--workspace`).
- **Real clocks.** Items 4 and 5 and part of 6 need real elapsed time. They are run as resumable jobs with state on disk; nothing
  stays running between checks except where stated (item 5 merger drill: 31 minutes).

## What I found that changes or limits the plan

| id | finding | effect |
|---|---|---|
| F-1 | **Item 1, library follower: no follower-half test binary existed (closed by lane F3).** The tests in `crates/weftos-store/tests/` minted their own buckets and keys and published from inside the test; none read another machine's publish with a supplied read-only key. | Lane F3 added `s2_follower_half.rs` (`writer_half`, `follower_half`, `both_halves_on_one_machine`). `--follower=library` now runs the follower half on the second host (Linux binary built in CI, run in the CI image) and keeps the raw SigV4 probe as a cross-check. See item 1. |
| F-2 | **The daemon has no clock seam.** `now_ms` is `SystemTime::now` (`control.rs:218`); `LeftoverPolicy`, the 30 min claim TTL and the object grace are constants. | "A writer a day ahead" cannot be a real independent clock without libfaketime in a Linux container (Q3 plus a second decision: libfaketime is not in the CI image) or a system clock change (never done). Item 6a runs by **stamp injection** through raw S3, and says so. |
| F-3 | **Prior two-machine evidence exists and is not cited in the ADR.** `scratch/drill-2machine/results.md` (2026-11-13, tip 54b830472, older than the fence and schema 2): two real nodes (an xpc container and a PG-host container, store on xpc), a writer killed mid-publish, a merger killed deterministically mid-claim, and a **real 30 minute stale claim** taken over by the other daemon at 30:01 of its own observation. | It shortens item 5 (the method is proven: hold the claim's PUT answer, kill, wait) but it ran older code, and the claim path has changed since (fence, announcement, 12 calls). It must be redone on this branch. It also shows a store-capable weaver ran on xpc and PG before this hold; I found no recorded owner go for that. |
| F-4 | **Item 4: only two thresholds have an env override.** `WEFTOS_WORKTREE_ORPHAN_AFTER_HOURS` (24 h, whole hours, minimum 1) and `..._ORPHAN_ARCHIVE_AFTER_DAYS` (14 d, minimum 1). Forgotten-open 3 d, unclaimed handoff 24 h, stuck cleanup 24 h (`LeftoverPolicy::default`), the 30 min claim TTL, the 10 min skew and the 24 h object grace have none. The hash-cache re-hash has `WEFTOS_WORKTREE_HASH_REHASH_HOURS` (default 24, 0 = every publish). | Shortened runs are smoke tests only (table below). The long waits are unavoidable. |
| F-5 | **Item 2: the other session's Windows work is a different crate.** `adr131-windows-bucket-load-inventory-2026-11-15.md` ports `weftos-seed-store` (ADR-131 seed-bucket join) to Windows. Gate item 2 is a Windows node loading a **worktree bucket** (the `weftos-store` fetch path, with its path refusals). Nothing of that has been compiled or run on Windows, and `weftos-store` has unix-only code (`share_publish.rs`, `share_hashcache.rs`, `fs.rs`, `rustfs.rs` use `os::unix` under or beside `cfg(unix)`). | Item 2 is not evidenced by that note. See item 2: what to ask for. |
| F-6 | **Item 3 against PG's RustFS cannot be measured without touching PG.** | Use the xpc throwaway for a real network, and ask the owner for a read-only key and a window if PG numbers are wanted (owner decision). |
| F-7 | Two first-run unknowns in my scripts: (R-mint) minting scoped keys with `curl --aws-sigv4` against `/rustfs/admin/v3/*` was never tried; (R-daemon-ro) a daemon started with a read-only key may fail at start if it writes control state. | Both fail fast and loudly. R-daemon-ro is itself a finding if it happens. Fallback for R-mint: root key for the writer daemon, as lane I did, with only the follower key scoped. |
| F-8 | **macOS monotonic clocks stop in sleep.** The daemon's claim watch is a monotonic observation. | The 31 minute taker loop runs under `caffeinate -ims`. A Mac that sleeps or a daemon that restarts resets the 30 minutes. |
| F-9 | **Item 8 cannot enumerate who holds a write key**, and PG's daemon cannot be asked by script without touching PG. | The script takes a host list, skips PG by default and counts a skipped row as not passed. The operator supplies the list, including spare laptops. |

## Waits that are real, and what a shortened run is worth

| threshold | value | override | a shortened run is |
|---|---|---|---|
| claim TTL (daemon: its own observation of an unchanged record) | 30 min | none (`CLAIM_TTL_MS`) | not available; wait 31 min |
| claim skew | 10 min | none | not available |
| orphan grace (path missing before the daemon marks it) | 24 h | `WEFTOS_WORKTREE_ORPHAN_AFTER_HOURS=1` | smoke of the chain, not evidence for 24 h |
| orphan archive delay (from marking) | 14 d | `WEFTOS_WORKTREE_ORPHAN_ARCHIVE_AFTER_DAYS=1` | smoke only |
| forgotten open / unclaimed handoff / stuck cleanup | 3 d / 24 h / 24 h | none | injected stamps (lane H did this) = no new evidence |
| object grace after a manifest stops naming an object | 24 h (bundles 1 h) | none for the daemon | not available |
| hash-cache full re-hash | 24 h | `WEFTOS_WORKTREE_HASH_REHASH_HOURS` | `=1` is a legitimate test of the mechanism at 1 h, not of the default |

Unattended: item 4 leaves `meta.env` and the RustFS data dir under `$WO069_GATE_STATE` and stops everything; `check` restarts both from
that state. The resume command is printed by `start`. Item 5's merger drill leaves one background loop (31 minutes).
Checks: **T+5 min** baseline (nothing listed); **T+23 h** negatives (nothing reported yet, grace holds, old object still there);
**T+25 h** unclaimed handoff, orphan marked and listed, old object deleted, full re-hash caught the edit; **T+3 d 1 h** forgotten open;
**T+15 d 2 h** gc due (14 d from the marking at T+25 h); then `finish`. Item 5 merger: **T+31 min**.

## Item 1: two machines, two principals, two clocks

**Requires.** Real RustFS, publish, poll, search, sync-docs, claim with two principals on two machines; a follower sees a publish within one poll interval.
**Held.** One machine, two principals (14 of 14); two real S3 keys, 8 of 8 writes refused (`s2_drills_live`); store on aepod-xpc through one forward, publish seen on the first poll in 10 of 10 (median 392 ms) (drills report, lane I addendum). Clocks agreed to the second, daemons both on the Mac. Lane F3: the writer half and the follower half as separate tests (`crates/weftos-store/tests/s2_follower_half.rs`), run together on one machine by `scripts/store/wo069-drills.sh --only fh --release`.
**Owed.** Writer and follower on different machines on different clocks; the same against PG's store (out of this lane).

**What runs where (`--follower=library`, no owner decision).** About 25 minutes.

| machine | runs | key | clock |
|---|---|---|---|
| the Mac | `writer_half` (the test binary, macOS): creates the two buckets on round 1, publishes a small synthetic tree with head `h<round>`, refreshes the registry entry and the digest as the daemon does; a writer daemon and a follower daemon for the daemon-path verbs | writer key (RW on the project and worktree buckets) | local, prints `published_ms` |
| aepod-xpc | `follower_half` (the same test, built for Linux x86-64, run in the CI image under rootless podman; no weaver, no daemon): poll (digest, then `--full`), older-writer verdict, search, sync-docs, browse, fetch (tree, and the bundle when the manifest has one), the eight refused writes; plus the raw SigV4 probe as a cross-check | read-only key | the host's, prints `seen_ms` |

**Env files (by path, mode 0600, never printed).** `WO069_GATE_CRED_DIR` is a 0700 directory on the notes volume (not `/tmp`, not `$HOME`). `mint-keys` writes `writer.env` and `follower.env` there (`MESH_S3_ENDPOINT`, `MESH_S3_ACCESS_KEY`, `MESH_S3_SECRET_KEY`, `MESH_S3_REGION`; the endpoint is the Mac's ssh-forwarded port) and copies the follower one to `~/ci-jobs/wo069-gate-rfs/follower.env` on the host (0600; its endpoint is the host's own loopback). The writer env stays on the Mac: `WEFTOS_FH_WRITER_ENV=$WO069_GATE_CRED_DIR/writer.env`. The follower half reads `/rfs/follower.env` inside the container (the host directory is mounted at `/rfs`). The script refuses an env file that is not 0600, is under `/tmp` or `$HOME`, or names PG.

**Build the follower half (Linux x86-64, in CI, then copy).** No Mac build for the Linux side (owner rule 2026-11-09). A test binary, not a daemon, so Q3 does not apply (the CI rule about a store-capable weaver concerns the weaver).
```
D=/Volumes/ai-models/research/weftos-agent-notes/scratch/ci
$D/ci-push.sh wo069-fh <worktree> HEAD
$D/ci-run.sh  wo069-fh build weftos 'cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run'
$D/ci-wait.sh wo069-fh build
mkdir -p /Volumes/ai-models/research/weftos-agent-notes/scratch/wo069/bin
B=$(ssh aepod@aepod-xpc 'ls -t ~/ci-jobs/wo069-fh/target/release/deps/s2_follower_half-* | grep -v "\.d$" | head -1')
scp aepod@aepod-xpc:"${B#/home/aepod/}" /Volumes/ai-models/research/weftos-agent-notes/scratch/wo069/bin/s2_follower_half.linux-x86_64
ssh aepod@aepod-xpc rm -rf ~/ci-jobs/wo069-fh
```
(`${B#/home/aepod/}` makes the path relative to the home directory, which `scp` resolves; adjust the prefix if the home differs.) The Mac binary is built by the script itself with a targeted `cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run --message-format=json` (or pass a prebuilt one in `WO069_GATE_FH_BIN`). The Linux binary runs inside `localhost/tapestry/build:1.95-ci-wasm` so the glibc is the one it was built against.

**Run.**
1. `WO069_GATE_CRED_DIR=<notes dir> scripts/store/wo069-gate-remote-rustfs.sh up aepod@aepod-xpc` (2 min: image must be local, port 19431, 4 CPUs, 4 GiB, ssh forward).
2. `... mint-keys aepod@aepod-xpc`: writer key (s3:* on all buckets of the throwaway, no admin) and read-only follower key; probes both; copies the follower env to the host (0600).
3. `WO069_GATE_WRITER_ENV=$WO069_GATE_CRED_DIR/writer.env WO069_GATE_FOLLOWER_ENV=$WO069_GATE_CRED_DIR/follower.env WO069_GATE_FH_LINUX_BIN=<the scp'd binary> scripts/store/wo069-gate-item1-two-machines.sh --follower=library --trials 10 --poll-interval-ms 30000`. `--dry-run` prints every command below and runs none.
4. `... remote-rustfs.sh down aepod@aepod-xpc` and read the RESULT lines.

**The commands, side by side (what the script runs; run them by hand to repeat one round).** `PB` is `p-` plus the lowercase project ULID; the worktree bucket is derived from it and the slug when `WEFTOS_FH_WORKTREE_BUCKET` is not set.
```
# Mac, round N (round 1 also creates the buckets with WEFTOS_FH_CREATE=1):
env WEFTOS_FH_WRITER_ENV=$W WEFTOS_FH_PROJECT_BUCKET=$PB WEFTOS_FH_SLUG=feat-fh WEFTOS_FH_ROUND=N WEFTOS_FH_CREATE=1 \
    $MACBIN writer_half --exact --ignored --nocapture --test-threads=1          # prints FH|writer|round=N|published_ms=<Mac ms>|head=hN
# aepod-xpc, started BEFORE the writer round, waits up to WAIT_MS for head hN, polling every POLL_MS on its own clock:
ssh aepod@aepod-xpc 'podman run --rm --pull=never --network host --userns=keep-id \
    -e WEFTOS_FH_FOLLOWER_ENV=/rfs/follower.env -e WEFTOS_FH_PROJECT_BUCKET=<PB> -e WEFTOS_FH_SLUG=feat-fh -e WEFTOS_FH_STATE=/rfs/fh-state.json \
    -e WEFTOS_FH_EXPECT_HEAD=hN -e WEFTOS_FH_WAIT_MS=35000 -e WEFTOS_FH_POLL_MS=30000 \
    -v $HOME/ci-jobs/wo069-gate-rfs:/rfs -w /rfs localhost/tapestry/build:1.95-ci-wasm \
    /rfs/s2_follower_half follower_half --exact --ignored --nocapture --test-threads=1'
```
The follower half prints `FH|follower|<step>|PASS|<ms>|<detail>` for `poll`, `poll-full`, `older-writer`, `search`, `sync-docs`, `browse`, `fetch`, `refused-1` to `refused-8` (each names the error it got) and `refuse-8-writes`; `FH|follower|clock|now_ms=<host ms>`; `FH|follower|poll|seen_ms=<host ms>|head=<head>`; and in the poll detail the number of polls it took. `older-writer` prints the verdict from the registry entry and from the bucket (lane F1's check; expected `clean`, a verdict fails the run). Each refusal is asserted to be an access error (`forbidden` / `AccessDenied`), not a missing object, a bad state or a transport error; the delete targets a pool object the manifest names and the test checks it is still there afterwards. The state file in `/rfs` is kept between rounds, so round N reports `HeadMoved` from `h(N-1)`; delete it to start from `New`.

**Publish-to-seen latency across two clocks.** `latency = seen_ms - offset - published_ms`, where `published_ms` is the Mac's clock when the registry refresh finished (the `FH|writer|...` line), `seen_ms` is the host's clock when the first poll that reported the head returned (the `FH|follower|poll|seen_ms=` line) and `offset = host clock - Mac clock`. Measure the offset with one ssh round trip, the same way the script does (the `clocks` row), and again after the trials (`clocks-after`):
```
t0=$(python3 -c 'import time; print(int(time.time()*1000))'); r=$(ssh aepod@aepod-xpc 'date +%s%3N'); t1=$(python3 -c 'import time; print(int(time.time()*1000))')
echo "offset_ms=$(( r - (t0 + t1) / 2 )) rtt_ms=$(( t1 - t0 ))"
```
**Caveat.** This is not NTP-grade: the offset is only good to about half the round trip (plus ssh and command startup asymmetry), here tens of ms, and the two clocks drift between the two measurements (the script reports the drift). Read a latency as `+- rtt/2 +- drift`. The pass criterion (a publish seen within one poll interval plus 2 s) is far looser than that error; a latency near zero or negative means the offset error is larger than the real latency, not that the follower saw the head before it existed. On one machine (`both_halves_on_one_machine`) there is one clock and no offset.

**Expected lines.** `RESULT|item1|follower-half-on-second-host|PASS|...`; `publish-seen-within-one-poll|PASS|...|10/10 within interval+2s on the second host's clock`; `raw-probe-cross-check|PASS|...`; `second-host-eight-writes-refused|PASS|...|REFUSED 8 of 8`; `clocks` and `clocks-after`; every `step` PASS.
**Pass.** 10 of 10 seen within one poll interval + 2 s on the second clock; the eight library refusals and the eight raw 403s; follower verbs pass under a real read-only key on both machines; older-writer verdict clean.
**Not covered in library mode.** A claim by the second machine (recorded `NOTRUN`); a store-capable follower daemon on the second host.
**One machine, any time (no ssh, no owner go).** `WEFTOS_STORE_TEST_RUSTFS_BIN=<rustfs> scripts/store/wo069-drills.sh --only fh --release`: starts a throwaway RustFS, mints a writer key and a read-only key, runs writer round 1, the follower half, writer round 2, the follower half again (head moved, seen within one poll), then removes the buckets and keys. It needs no weaver.
**Run with the owner's go: `--follower=daemon` (needs owner go (Q3)).** Refuses without `WO069_OWNER_GO_Q3=1`. Prerequisites: a Linux weaver of the branch built in CI (`ci-push.sh`, `ci-run.sh ... 'scripts/build.sh native-debug'`, copy `target/debug/weaver` out of the lane target), the sources from the same push; set `WO069_GATE_REMOTE_WEAVER` and `WO069_GATE_REMOTE_SRC`. The host runs a follower daemon (read-only key) and a merger daemon (a copy of the writer key, agent `merger`) in rootless podman with their own HOME and runtime dir, and does follow, search, sync-docs, browse, fetch and the claim itself. About 40 minutes plus the build (11 min loaded). Cleanup: both containers and `$RDIR/node` removed and checked.
**Risks.** R-mint, R-daemon-ro (above); R-fh (first run of the podman line: `--userns=keep-id` and a 0600 env file read inside the container, the host's WSL clock source); a 30 s poll with ten trials is about 6 minutes of idle waiting by design; the writer key is `s3:*` on every bucket of the throwaway, so "writer RW on its own bucket only" is not enforced here (lane H's 1a did that).
**Needs the other session / owner.** PG-store variant: owner (read-only key, window).

## Item 2: a Windows node loads a worktree bucket natively

**Requires.** A Windows node fetches a worktree bucket; reserved names, trailing dots and spaces, `<>:"|?*`, `git~1`, case collisions, long paths, CRLF.
**Held.** None (the refusals are unit-tested on every platform; nothing was compiled or run on Windows for `weftos-store`).
**Not this lane's to run.** What is needed from the other session: (1) `weftos-store` (feature `rustfs`) and the weaver's `store worktree fetch` path compiled for `x86_64-pc-windows-msvc` (the other session proved the toolchain on aepod-xpc for `weftos-seed-store`; `weftos-store` has `os::unix` code that may not compile); (2) a throwaway RustFS reachable from the Windows side (the container on xpc binds WSL loopback; WSL2 forwards localhost to Windows by default, to be confirmed); (3) a worktree bucket published from the Mac by item 1's writer with hostile names (`CON`, `NUL`, `COM1`, `a.`, `a `, `x:y`, `Readme`/`README`, a 270-character path, a CRLF file); (4) a read-only key; (5) the expected outcome per name (refused loudly, not skipped silently), recorded in the table below. Drive (ProjFS) is after D1/D2 and out of this item.
**Risks.** Compile failures are themselves the result; antivirus on 6k small files; long paths unsupported by the other session's port.

## Item 3: metadata latency at the client project's real size

**Requires.** `list` and `head` latency for a tree of the client project's size.
**Held.** Synthetic 1k, 10k, 20k, 50k files on loopback (drills report). A digest-hit poll is one GET, under 1 ms to 3 ms; first publish of 50 000 files 2 438 s; list of the tree prefix 25.9 s; over a real network a HEAD is about 80 ms.
**Owed.** The real file count, total size, largest file and directory shape; the same measurements against PG's RustFS.
**Run (the owner or the tree's session, locally): `scripts/store/wo069-gate-item3-tree-stats.sh <dir>`.** Seconds to minutes. It prints integers and fixed labels only: `files`, `total_bytes`, `largest_bytes`, `symlinks`, `files_over_16MiB`, a size histogram, `directories`, `max_depth`, `mean_depth_x100`, a depth histogram, `fits_100000_files`, `fits_4GiB`, `fits_16MiB_per_file`, and the first-publish minutes at 30 and 68 PUT/s. For a git directory it counts what a publish would share (`git ls-files -co --exclude-standard`); `--all-files` counts everything. Paste the output into the results table. It writes nothing and uploads nothing.
**Pass.** The tree fits all three ceilings; otherwise the gate result is "does not fit", which is a result.
**With the numbers:** run `wo069-drills.sh --only scale` with `WEFTOS_SCALE_SIZES=<count>` on the xpc throwaway (`WEFTOS_DRILLS_RUSTFS_ENV` pointing at the forward) for latency at that size over a real network. **PG numbers: needs an owner decision** (a read-only key and a window; or accept xpc numbers).

## Item 4: leftover and handoff on real clocks

**Requires.** A worktree left open past the threshold shows in `leftovers`; a handoff landed by a second agent on another machine, then promotion, bucket deletion and the registry entry gone.
**Held.** Thresholds crossed by injected stamps; handoff claimed, fetched, landed with git on a second clone on the same machine, promoted, retired.
**Owed.** A real leftover over a real day; a handoff landed on a second machine.
**Run: `scripts/store/wo069-gate-item4-leftover.sh start`** (5 minutes; `WO069_GATE_STATE`, `WEFTOS_STORE_TEST_RUSTFS_BIN`; local RustFS and daemon on the Mac; the data directory persists). It publishes `open`, `hand` (handoff never claimed), `orph` (git worktree removed), `land` (handoff, claim, git land in a second clone, promote, archive), `grace` (g.md edited twice; `big.bin` replaced with same size and mtime). Then `check` at the times above, `finish` last. Each row asserts "not reported" before its threshold and "reported" after it, with the elapsed seconds. Rows: `open-forgotten`, `hand-unclaimed`, `orphan-marked`, `orphan-listed`, `orphan-gc-due`, `land-stays-gone`, `object-grace-24h`, `rehash-24h`, `rehash-at-start`.
**Expected.** T+23 h: every row "correctly NOT reported", grace holds, old object present. T+25 h: unclaimed and orphan reported, old object deleted, `big.bin` updated by the full pass. T+3 d 1 h: forgotten open. T+15 d 2 h: `due=true`; `finish` retires the orphan and verifies bucket and entry gone.
**Pass.** Every row PASS at its time. `rehash-at-start`/`rehash-24h` before 24 h record the OLD bytes by design (a same-size, same-mtime edit is invisible to the sweep's digest and the cache); that is documented behaviour and a hazard to state to operators, not a failure.
**Second machine: `--merger=second-host` (needs owner go (Q3))** for the claim and land of `land`. **Without Q3:** the landing on a second machine is not obtainable; the same-machine result stays what the ADR holds. **Smoke:** `--smoke` (orphan chain only, all lines tagged SMOKE).
**Risks.** The Mac must be awake when `check` runs, nothing more; a leftover `.wt` bucket if `finish` is skipped (the state dir is the only record; remove it by hand). `leftovers` row matching reads text lines (slug and a why word); a changed output format needs the pattern updated.

## Item 5: failure drills

**Requires.** Kill the writer mid-publish, the merger mid-claim, the daemon during a registry refresh; nothing lost; a stale claim recoverable.
**Held.** Publish cut at four points; stale claims with the library clock; `kill -9` at 0.8, 2.5, 4.5 s; the refresh cut deterministically in a library test. The 2026-11-13 two-machine drill (F-3) on older code.
**Owed.** A real writer process kill on a machine publishing with its own key; the merger killed mid-claim and the daemon's own 30 minutes; the refresh cut at a known point in the daemon.
**Run (decision-free, Mac daemons, any throwaway store reached over loopback):**
- `wo069-gate-item5-failures.sh writer-kill` (about 5 minutes against local, 15 against the xpc forward): seven kills at 0.4 to 5.5 s into a 300-file publish with the writer's own key (a publish of 300 files takes about 5 s on the Mac against a local store: later offsets fall after the publish, and the row says so); per kill the served manifest is fetched with no checksum error and every fetched file equals the worktree's, the rerun completes, the fetched tree equals the worktree. Rows `kill-at-<d>s-*`.
- `merger-kill` then, 31 minutes later, `merger-check`: a loopback proxy (`wo069-gate-hold-proxy.py`) forwards the claim's conditional PUT of the handoff record in full, withholds the answer, the merger daemon is killed 0.3 s later; a taker daemon claims every 60 s under `caffeinate`. Pass: refusals up to 29:00, takeover at the first attempt after 30:00 of the taker's observation (observation >= 1800 s, >= 29 refusals), epoch 2, `promote --epoch 1` refused.
- `refresh-kill` (about 3 minutes): the proxy drops the PUT of `worktrees/_digest.json`, the daemon is killed right after the entry PUT. Pass: the digest follower sees no move, the `--full` follower sees it, the next refresh heals it.
**What can be timed from outside, honestly.** At the wire: after a named request is applied and before its answer; between two named requests; at a wall-clock offset. Not: a kill at a named instruction inside the daemon, or inside RustFS. A "kill inside the refresh" here means between its two PUTs as seen on the wire. A kill landing in the middle of one request's bytes is not controlled.
**Second-machine merger (the 2026-11-13 shape) needs owner go (Q3).** Its alternative is the two Mac daemons above.
**Risks.** A Mac that sleeps or a taker daemon that restarts resets the 30 minutes (F-8); the proxy only matches a request line in the first TCP read; with the forwarded store a 300-file publish takes about 25 s, so adjust `WO069_GATE_DELAYS`.

## Item 6: follower drills on real clocks

**Requires.** A writer a day ahead; corrupt and unknown-schema entries; empty or wrong project bucket; a merged entry whose cleanup never ran; interrupted `sync-docs` then a deleted doc; `overlaps` on two real worktrees with different bases; two overlapping `follow` runs on one state file.
**Held.** The same list in the library on injected clocks and on a local RustFS (lane Q: 16 drills, plus the fixes 6a-1 to 6g); `overlaps` false shares 67 to 90 percent without change lists, 0 with them.
**Owed.** The same through the real daemon and CLI, on real clocks.
**Run: `wo069-gate-item6-followers.sh [--only a,...,g]`** (about 25 minutes, throwaway store, Mac daemons):
a writer stamp +24 h through raw S3 (real follower clock; **not** an independent writer clock, F-2); b corrupt, mislabelled and schema-99 digest; c emptied registry and a wrong project ULID; d merged entry stamped 23 h and 25 h ago (stuck_cleanup only at 25 h); e daemon killed mid `sync-docs`, a doc deleted, second sync; f `overlaps` on two real sparse worktrees of this repo's `docs/adr` and `scripts/store` at bases `WO069_GATE_GAP` commits apart, truth from `git merge-tree`; g ten rounds of three concurrent `follow --state FILE`.
**Pass.** Every row PASS; f reports the false share and must show 0 missed conflicts. **a2 (writer under libfaketime): refused; needs owner go (Q3) and libfaketime in the CI image; otherwise "cannot be done without changing a system clock: needs owner decision".**
**Risks.** Output formats are matched loosely (grep on slug and a why word). A forged registry object is invisible to a digest-driven reader until a refresh rewrites the digest, and the daemon's metadata cache answers for up to 5 s without asking the store: the drills refresh or delete the digest and wait 6 s. Forged objects are deleted by the script and the throwaway is removed anyway. State directories must stay short (a unix socket path under 104 bytes: `<run dir>/child-ipc/child.sock`), so the scripts use `target/g5.*` and `target/g6.*`.

## Item 7: ADR-131 D1 and D2 pass first

**Not this lane's.** Needed from the other session: pass or fail for D1 (policy) and D2 (Linux drive), each with the commit and date, and whether the macOS D1 drive is the prerequisite the card names. Evidence so far: none from these drills. Nothing to run here; record the answer in the table.

## Item 8: every writer daemon has the fence and the conditional manifest write

**Requires.** Each machine that writes the project's worktree buckets reports the build that contains `w2/wo069-worktree-mirror`'s fence or later; no machine with a write key runs an older daemon.
**Run (read-only; 2 minutes): `scripts/store/wo069-gate-item8-writers.sh --min-commit <fence commit> hosts.txt`** with one line per host `label ssh-target [weaver path]`. It runs `weaver --version` over ssh and checks the build commit against `--min-commit` with `git merge-base --is-ancestor`. The fence commit is the first one with both the claim fence (lane Y, `19e6bf5b1` announcement and `eeab2bd2c` fenced land) and the conditional manifest write (lane D3, `759ccad5a`); the merge that contains them all is the one to pass (the operator picks it; no version number exists). **Pass:** 0 OLDER, 0 UNKNOWN, 0 skipped. PG is skipped by default and counts as not passed until its operator answers. It cannot find a stale daemon on a machine nobody listed.
**Risks.** `weaver --version` of a binary is not the running daemon's build; a daemon started before an upgrade keeps running the old one. The operator also checks `kernel status` on each machine (not run here: it touches the daemon).

## Schedule

| block | what | wall time | needs |
|---|---|---|---|
| **1. First, any time after the benchmark** | `item4 start` (starts the long clock); `item8` on the Mac binary | 10 min | Mac weaver, local rustfs binary |
| **2. One sitting on the Mac, no second machine, no owner go** | `item5 writer-kill`, `refresh-kill`, `merger-kill` (31 min wait, do item 6 meanwhile), `merger-check`; `item6` a to g | about 1 h 30 | throwaway local RustFS (`WEFTOS_STORE_TEST_RUSTFS_BIN`) or the xpc forward |
| **3. Second machine, no owner go** | remote-rustfs `up`, `mint-keys`; `item1 --follower=library`; optionally block 2 again with the store on xpc; `down` | about 1 h | ssh to aepod-xpc, image local |
| **4. Second machine, owner go (Q3)** | CI build of the weaver; `item1 --follower=daemon`; `item4 --merger=second-host`; second-host merger variant of item 5 | about 2 h | Q3 |
| **5. Days** | `item4 check` at T+23 h, T+25 h, T+3 d 1 h, T+15 d 2 h, `finish` | minutes each | the Mac awake at those times |
| **6. Others** | item 2 (Windows session), item 7 (ADR-131 session), item 3 (owner, locally), PG numbers (owner) | n/a | other people |

## Environment variables and ports

| name | default | used by | meaning |
|---|---|---|---|
| `WO069_GATE_CRED_DIR` | none, required | remote-rustfs | local 0700 dir for 0600 credential files; notes volume; short path (ssh socket limit) |
| `WO069_GATE_RFS_NAME` / `_IMAGE` | `wo069-gate-rustfs` / `docker.io/rustfs/rustfs:1.0.4` | remote-rustfs | container name, local image (never pulled) |
| `WO069_GATE_RFS_PORT` / `WO069_GATE_FWD_PORT` | 19431 / 19431 | remote-rustfs | remote loopback port / local forward port (the 2026-11-13 drill used 19411, 19412) |
| `WO069_GATE_REMOTE_DIR` | `~/ci-jobs/wo069-gate-rfs` | remote-rustfs, item1 | data, env files, tmp on the host (removed by `down`) |
| `WO069_GATE_WRITER_ENV`, `WO069_GATE_FOLLOWER_ENV` | none | items 1, 5, 6 | 0600 env files (endpoint, access, secret, region) |
| `WO069_GATE_PROJECT` | random ULID | item 1 | project id |
| `WO069_GATE_FH_LINUX_BIN`, `_FH_BIN`, `_FH_IMAGE` | none, built by the script, `localhost/tapestry/build:1.95-ci-wasm` | item 1 | library mode: the Linux x86-64 `s2_follower_half` built in CI; a prebuilt Mac one; the image the follower half runs in |
| `WO069_GATE_REMOTE_WEAVER`, `_REMOTE_SRC` | none | item 1 | daemon mode: Linux weaver and source on the host |
| `WEFTOS_FH_*` | see item 1 | the `s2_follower_half` test | writer env file, follower env file, project bucket, worktree bucket (optional), slug, round, state file, expected head, wait and poll ms, bundle, create |
| `WO069_GATE_STATE` | none | items 4, 5 | durable state dir (0700, notes volume) |
| `WO069_GATE_FILES`, `_DELAYS` | 300, "0.4 0.9 1.5 2.2 3 4 5.5" | item 5 | publish size, kill offsets (s) |
| `WO069_GATE_GAP`, `_PATHS` | 200, "docs/adr scripts/store" | item 6f | base distance, sparse paths |
| `WO069_GATE_MIN_COMMIT` | none | item 8 | the fence commit |
| `WO069_OWNER_GO_Q3` | unset | items 1, 4, 6 | set to 1 only after the owner says go |
| `WEFTOS_E2E_WEAVER`, `WEFTOS_STORE_TEST_RUSTFS_BIN`, `KEEP` | `target/debug/weaver`, none, unset | all, item 4, all | weaver, rustfs binary, keep work dirs |
| `WEFTOS_STORE_RUSTFS_ENV`, `WEFTOS_WORKTREE_ORPHAN_AFTER_HOURS`, `..._ORPHAN_ARCHIVE_AFTER_DAYS`, `..._HASH_REHASH_HOURS` | | daemon | set by the scripts for the daemon they start (item 4 `--smoke`) |

Ports: Mac loopback 19431 (ssh forward to the host); xpc loopback 19431 (RustFS); item 4 local RustFS and item 5 proxy use a free
loopback port chosen at run time. Nothing listens on a non-loopback address. Nothing is opened on PG.

## Results (paste the `RESULT|` lines or numbers here)

| item 1 | value |
|---|---|
| clock offset, rtt (ms), after the trials, drift | |
| publish seen on the second clock: 10 latencies (ms) | |
| eight writes refused on the host (library half, raw probe) | /8, /8 |
| older-writer verdict (follower half) | |
| polls per trial, raw-probe cross-check latencies | |
| follower verbs under the read-only key | follow / search / sync-docs / browse / fetch / put refused |
| claim by the second machine (daemon mode only) | |

| item 3 (counts from the owner) | value |
|---|---|
| files / total bytes / largest / over 16 MiB / symlinks | |
| directories / max depth / mean depth | |
| fits 100 000 files, 4 GiB, 16 MiB per file | |

| item 4 row | T+ (s) | result | note |
|---|---|---|---|
| open-forgotten, hand-unclaimed, orphan-marked, orphan-listed, orphan-gc-due, land-stays-gone, object-grace-24h, rehash-24h | | | |

| item 5 row | result | note |
|---|---|---|
| kill-at-0.5s ... 12s (3 rows each) | | |
| merger-killed-mid-claim, takeover (observation s, refusals), takeover-epoch-2, stale-epoch-fenced | | |
| refresh-cut, digest-follower-behind, full-follower-sees-move, next-refresh-heals | | |

| item 6 row | result | note |
|---|---|---|
| a, b (3), c (2), d (2), e, f (reported, true, false share, missed), g | | |

| items 2, 7, 8 | value |
|---|---|
| item 2: per hostile name, outcome on Windows | |
| item 7: D1, D2 pass or fail, commit, date | |
| item 8: host, version, commit, verdict | |

## Block 3: Linux build of the follower-half test binary

Lane G2 (2026-11-15). Prepared offline; nothing here has been run. Block 3 waits for the owner: every remote step refuses unless `WO069_OWNER_GO_BLOCK3=1` is set, and the production host (photo-gallery, 100.90.170.87, :9489) is refused. This section replaces the hand-typed "Build the follower half" snippet in item 1 with a script (`scripts/store/wo069-gate-build-linux-bin.sh`, test `scripts/tests/wo069-gate-build-linux-bin.test.sh`); do not run both.

**What it builds.** The test executable of `crates/weftos-store/tests/s2_follower_half.rs` for Linux x86-64, in a weftos-ci container (rootless podman, `localhost/tapestry/build:1.95-ci-wasm`), reached only through `scratch/ci/ci-push.sh`, `ci-run.sh`, `ci-wait.sh` (a git bundle over ssh to `~/ci-jobs/wo069-fh/src`; one of six job slots). No Mac build, no daemon, no RustFS is started. The in-container command is `cargo test -p weftos-store --features rustfs --release --test s2_follower_half --no-run --locked`. `--locked` is right here: `Cargo.lock` is committed, `cargo metadata --offline --locked` resolves cleanly on the branch, and the CI recipe's own examples pass `--locked`. `--no-run` builds the standalone test executable without running it. The script then picks the newest ELF under `/target/release/deps/s2_follower_half-*` (not the `.d` file), copies it to `/target/wo069-fh/s2_follower_half` and writes `s2_follower_half.sha256` and `s2_follower_half.file` beside it. Inside the container `/target` is `~/ci-jobs/wo069-fh/target` on the host (the CI README: per-lane target dir).

**Prerequisites.** The owner's go (`WO069_OWNER_GO_BLOCK3=1`); ssh to aepod@aepod-xpc with BatchMode keys (the ci scripts hard-code that host); the image present on the host (it is the CI default); a free job slot; about 4 GB free on the host; the notes volume mounted; the commit to build already committed on the branch (default ref: `w2/wo069-worktree-mirror`; pass `--ref` for another, the commit is printed). Run `scripts/store/wo069-gate-build-linux-bin.sh plan` first: it prints every command and runs nothing.

**The four commands, in order** (set the go in the environment of each; `plan` shows them with the exact quoting):
```
export WO069_OWNER_GO_BLOCK3=1
S=scripts/store/wo069-gate-build-linux-bin.sh
$S push [--ref REF]     # ci-push.sh wo069-fh <repo> <commit>
$S build                # ci-run.sh wo069-fh build weftos '<cargo ... --no-run --locked; locate ELF; stage + sha256 + file>', then ci-wait.sh (polls every 30 s)
$S fetch                # scp to scratch/wo069/bin/s2_follower_half.linux-x86_64, sha256 check, x86-64 ELF check, prints the export line
$S clean                # rm -rf ~/ci-jobs/wo069-fh on the host, verifies it is gone
```
`build --no-wait` returns after the job starts and `wait` finishes it, for a session that must not hold a call open for the whole build. Each subcommand also takes `--dry-run`.

**Expected time and disk (estimates, not measured).** Cold target dir: 8 to 15 minutes (release, `lto = true`, `codegen-units = 1`; reqwest with rustls, tokio and hyper are the bulk). Disk: 2 to 4 GB under `~/ci-jobs/wo069-fh/target`; the staged binary 10 to 40 MB. The host has about 60 GB free, so run `clean` when done. A rerun on the same lane is faster; `clean` removes that cache.

**What to check.** `build` ends with the job status line (`exit 0`; anything else stops the script) and prints the sha256 and the `file` line from the container: expect `ELF 64-bit LSB ... x86-64`. `fetch` repeats it locally: the sha256 of the copied file must equal the one written in the container, and the header must be an x86-64 ELF (`$S elf-check <file>` does the second check alone). Not verifiable offline: that the crate builds in the container with the shared `/cargo` cache (a cold cache needs crates.io from xpc), the status-line format `ci-wait.sh` prints for a failed job, and the glibc match below. A failed first run is a result: read `scratch/ci/logs/wo069-fh/build.log`.

**Where it runs.** The binary is dynamically linked against glibc of the build image (Debian trixie); reqwest uses rustls, so there is no libssl dependency. It is meant to run in the same image on the same host under podman, which is exactly what `wo069-gate-item1-two-machines.sh --follower=library` does (it copies the file to `~/ci-jobs/wo069-gate-rfs/`, mounts that at `/rfs`, and runs `/rfs/s2_follower_half` in `$WO069_GATE_FH_IMAGE`, default the same image, `--network host`). It was not built to run on the WSL distro's own userland or on a Mac. At run time it needs only the follower env file (read-only key) and network reach to the throwaway RustFS; no weaver and no daemon. If the image tag the item-1 script runs (`WO069_GATE_FH_IMAGE`) differs from the one the CI host builds in by default, the glibc can differ: keep them equal.

**Feeding item 1.**
```
export WO069_GATE_FH_LINUX_BIN=/Volumes/ai-models/research/weftos-agent-notes/scratch/wo069/bin/s2_follower_half.linux-x86_64   # printed by fetch
WO069_GATE_WRITER_ENV=... WO069_GATE_FOLLOWER_ENV=... scripts/store/wo069-gate-item1-two-machines.sh --follower=library --trials 10 --poll-interval-ms 3000
```
That script scp's the binary to the host and runs `follower_half` in the image; the build job does not need to exist any more by then, so run `clean` before or after, but keep the fetched file.

**Cleanup.** `$S clean` (verifies `~/ci-jobs/wo069-fh` is gone). The local bundle is removed by `ci-push.sh`; logs stay under `scratch/ci/logs/wo069-fh/`. The fetched binary stays on the notes volume until item 1 is done.
