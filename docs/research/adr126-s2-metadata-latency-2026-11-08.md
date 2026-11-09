# ADR-126 S2: metadata latency at Sansone's size (2026-11-08)

Gate item 3 of the S2 worktree-store review: "Metadata caching is only partial: measure list and head
latency for a worktree of Sansone's size before assuming one-poll-per-worktree is affordable." Board
card 11b00a2e-4e82-4ba9-a5e6-eb70fb6dadde.

**Answer:** yes, polling is affordable. A follower poll is one list plus one small get per registered
worktree. On loopback it costs 2 ms of store time at 20 worktrees. Extrapolated to the real PG store at
the measured 58 ms round trip, it costs about 1.2 s at 20 worktrees, once per 30 to 60 s. The time is
round trips, not bytes and not parsing. The larger risk the run exposed is storage, not polling: on a
contended disk every object PUT took about 0.3 s, which makes a first publish of 10k files impractical.

## Conditions

| Item | Value |
|---|---|
| `weaver` | 0.8.4 build 9c4b828d (prebuilt, two commits behind be64b6014; the difference is a test only) |
| RustFS | 1.0.4 (git 46337942), macOS arm64, loopback 127.0.0.1:19410, console off, keys in a 0600 file |
| Machine | Mac17,6, Apple M5 Max, macOS (Darwin 27.0.0). Other lanes were building and testing: load average 3.2 to 6.5 during the runs (per-run load is in the tables) |
| Daemon | isolated: `HOME` and `WEFTOS_RUNTIME_DIR` set to the scratch dir, `WEFTOS_STORE_RUSTFS_ENV` naming the env file. `~/.weftos` and `~/.clawft` were not written by this lane |
| Main storage condition | RustFS data dir on a 1.5 GiB RAM disk (APFS). Synthetic trees stay on the external volume; fetch and sync-docs destinations are on the RAM disk |
| Slow storage condition | RustFS data dir on the shared external volume (`/Volumes/ai-models`), busy with other lanes. Only partly measured, see below |
| Client | every S2 timing is a whole `weaver store worktree <verb>` process (CLI start, RPC to the daemon, the daemon's S3 work). Raw S3 timings use a stdlib Python SigV4 client with one keep-alive connection |

Synthetic tree "sansone" (seeded, deterministic, no client files): 9,517 tracked files, 800 untracked
not ignored, 2,395 `.md` with titles, 101 MB in total, 283 files of 50 KB or more, one `package-lock.json`
of 411 KB (the brief said 450 KB; the generator truncates), one branch `feat/bench-work` three commits
ahead of `master`, five uncommitted edits. Publish writes 10,317 tree objects plus the manifest, handoff,
meta and the git bundle: the bucket holds 10,321 keys. The manifest is 2.0 MB. A registry entry is 355 bytes.

Not done: the 6,000-file WeftOS-like profile. The generator supports it (`weftos`), but writing the
sansone tree took 38 minutes on the contended volume, and the time went to the main profile.

## Results on the RAM-disk RustFS (median and worst of 3 unless noted)

### Publish (share) of the whole tree

| Run | Wall | Uploaded | Rate | Load avg |
|---|---|---|---|---|
| 1 (first ever, files cold) | 712.3 s | 10,317 | 14.5 PUT/s | 4.13 |
| 2 (second worktree, same files) | 277.0 s | 10,317 | 37 PUT/s | 3.80 |
| 3 (third worktree, same files) | 17.7 s | 10,317 | 581 PUT/s | 3.74 |
| Re-publish, nothing changed (3 runs) | 21.6 s, 1.70 s, 1.59 s: median 1.70, worst 21.6 | 0 (10,317 unchanged) | | 3.73, 3.51, 3.49 |

The spread is not store speed: raw 5 KB PUTs on this RustFS took about 1.4 ms each (15 samples), which
would be 15 s for 10k objects, matching run 3. Runs 1 and 2 were slow while the source files were read
cold from the busy external volume and warmed up. I did not isolate this (no cold-cache control), so it is
an inference from the three runs. An unchanged re-publish skips all PUTs and costs about 1.6 s (it reads the 2 MB manifest and re-hashes).

### Follow (registry poll)

Registry entries for N above 1 are copies of the real entry under other slugs, written straight into
the project bucket with the raw client (a poll only reads the registry, and an entry is 355 bytes
whatever the tree size). N=1 is the real registry with one worktree.

| N | follow, cold state | follow, warm state | S3 list `worktrees/` p50 / p99 | S3 all N gets, sequential p50 / p99 |
|---|---|---|---|---|
| 1 | 8 ms (runs 8, 22, 8; worst 22) | 26 ms (26, 31, 25; worst 31) | 0.24 / 3.26 ms | 0.11 / 0.32 ms |
| 5 | 8 ms (8, 8, 10) | 52 ms (90, 52, 42; worst 90) | 0.53 / 3.60 ms | 0.54 / 1.16 ms |
| 20 | 11 ms (11, 11, 10) | 25 ms (25, 24, 66; worst 66) | 1.25 / 4.10 ms | 1.98 / 3.64 ms |

Raw numbers are 50 samples each. One get is about 0.1 ms. Process start and RPC dominate the CLI wall
time (8 to 90 ms, noisy under load); the store part is under 3 ms even at N=20. List versus get versus
parse: list 1.25 ms, 20 gets 1.98 ms, JSON parse of 20 small entries is below the resolution of this
measurement. Process start and the state file are the rest.

### Other verbs (loopback, whole process)

| Verb | Median | Worst | Load avg | Notes |
|---|---|---|---|---|
| search "note" (1 real worktree plus 19 padded entries) | 18 ms | 21 ms | 3.36 | only one manifest appears to have been read: hits came from one slug |
| search "note" (3 real worktrees) | 30 ms | 97 ms | 5.91 | each 2 MB manifest costs about 6 ms (GET plus parse) |
| leftovers (20 entries) | 10 ms | 11 ms | 3.36 | registry only, no manifests |
| overlaps a b (both 10k files) | 24 ms | 88 ms | 4.50 | two manifests |
| sync-docs, 2,395 docs, fresh cache | 0.70 s | 0.80 s | 4.50 | about 3.4k docs/s |
| fetch whole tree, 10,317 files, 101 MB, fresh dest | 3.37 s | 3.81 s | 5.16 to 6.51 | about 3.1k files/s, includes the git bundle |

### Raw S3 metadata on the ~10k-key worktree bucket (10,321 keys)

| Call | p50 | p99 | n |
|---|---|---|---|
| ListObjectsV2, one page of 1000 | 95.2 ms | 107.5 ms | 110 |
| ListObjectsV2, whole bucket (11 pages) | 1002 ms | 1017 ms | 10 |
| HeadObject, a tree object | 0.17 ms | 4.24 ms | 50 |
| HeadObject, 20 objects spread over the bucket | 0.13 ms | 0.29 ms | 50 |
| HeadObject, registry entry | 0.14 ms | 0.28 ms | 50 |
| GetObject, 7.6 KB object | 0.13 ms | 0.19 ms | 50 |

Listing is the only slow metadata call: about 95 ms per 1000 keys, so about 1 s for the whole bucket.
The follower never lists the worktree bucket; it lists only the small registry prefix in the project bucket.

## Slow-disk condition (RustFS data dir on the shared external volume)

| Call | External volume | RAM disk |
|---|---|---|
| PUT 5 KB, p50 (range) | about 270 ms (188 to 612, n=15, measured while a publish ran) | 1.4 ms (n=15) |
| CreateBucket | 12.1 s and 15.5 s (2 calls) | 11 ms (1 call) |
| `store worktree create` (two buckets) | 30.6 s, 16.5 s, 19.0 s (3 runs); 21.6 s once more | 2.2 s |
| Publish progress | 7, 12, 21 objects at 20 s steps, about 0.3 object/s, so 10k objects would take about 9 hours | see above |

These are from a loaded shared volume and a single probe each, so treat them as an order of magnitude.
The point: RustFS makes durable writes per object, and on slow storage the write path, not the metadata
path, is what hurts. PG's disk class decides whether a first publish is minutes or hours; that was not measured.

## Mac to PG round trip (read-only, no credentials)

20 plain `GET /` requests to `http://100.90.170.87:9700/` over the tailnet (answers were 403, which is fine),
against the same on loopback:

| Target | Keep-alive request p50 / p99 | New connection each p50 / p99 |
|---|---|---|
| PG over tailnet | 58.0 / 113.8 ms | 116.7 / 427.2 ms |
| Loopback RustFS | 0.07 / 0.12 ms | 0.14 / 2.78 ms |

I did not check whether the tailnet path was direct or relayed.

## Extrapolation to a Mac follower polling PG

The S3 client is a pooled `reqwest` blocking client (`crates/weftos-store/src/rustfs.rs`), so a long-lived daemon
reuses connections: one round trip per call. A poll is 1 list + N gets, run one after another.

| N | Calls | Poll at keep-alive p50 (58 ms) | At p99 (114 ms) | If every call opened a new connection (117 ms) |
|---|---|---|---|---|
| 1 | 2 | 0.12 s | 0.23 s | 0.23 s |
| 5 | 6 | 0.35 s | 0.68 s | 0.70 s |
| 20 | 21 | 1.2 s | 2.4 s | 2.5 s |

Server time (0.1 to 1.3 ms per call) is below 2 percent of that. At one poll per 30 s and N=20, a follower
spends about 4 percent of its time in the poll and sends 0.7 requests per second; at 60 s, half of that. For
the verbs that read a manifest, each costs one extra 2.0 MB GET plus about 6 ms of parse. Transfer time
over the tailnet was not measured (no credentials, so no large object to fetch). Pure arithmetic: 0.16 s at
100 Mbit/s, 1.6 s at 10 Mbit/s, plus a round trip.

## Conclusion

- **One poll per worktree per 30 to 60 s is affordable** at Sansone's size, including N=20 (about 1.2 s of
  mostly waiting per poll). The registry entry is 355 bytes and does not grow with the tree.
- **Where the time goes:** round trips. List 1.25 ms, gets 0.1 ms each, parse unmeasurable, all on loopback;
  at 58 ms per call the 21 sequential calls are the cost. Listing the worktree bucket (95 ms per 1000 keys)
  is not on the poll path.
- **What does scale with tree size:** the 2.0 MB manifest. Search, overlaps and sync-docs read it (about 6 ms
  parse each, plus one transfer), and a no-change re-publish reads it (1.6 s).
- **Gaps:** PG PUT and bandwidth numbers, the WeftOS-sized tree, a cold-cache control for the slow first
  publish, and whether the tailnet route is direct.

## Options if the poll ever matters (largest measured gain first)

1. **Issue the N gets in parallel.** No format change. Cuts N=20 from about 21 round trips to about 2
   (1.2 s to about 0.12 s at 58 ms). Cheapest option; the S1 backend trait is sequential today.
2. **One registry digest object, written by the daemon.** The daemon is already the only writer of
   `worktrees/<slug>.json`. If it also rewrites `worktrees/_index.json` (all entries, about 355 B each, 7 KB
   at N=20) on each refresh, a poll is one get, or one conditional get that returns "not modified" with no
   body. N=20 goes from 21 round trips to 1 (1.2 s to 0.06 s). Cost: one more write per publish and claim,
   and a fallback to list-and-get if the digest is missing or stale.
3. **Cache manifests by content hash.** Put the manifest hash in the registry entry (or `_index.json`) and
   have followers keep the last manifest per worktree, so search, overlaps and sync-docs fetch the 2.0 MB
   object only when the hash changed. Saves one 2 MB transfer and about 6 ms of parse per worktree per call.
   Splitting the docs index (about 2.4k entries) from the file list would shrink the transfer a further
   75 percent or so by the document ratio (estimate, not measured).

## Commands

Everything below runs from the worktree; `$S` is a scratch directory (not `/tmp`), `$W` the prebuilt `weaver`.

```bash
S=/Volumes/ai-models/weftos-worktrees/adr126-s2-meta-scratch
W=/Users/mathewbeane/weftos-s2-build-9c4b828d/target/release/weaver
R=/Volumes/ai-models/research/tcs-spike-rustfs-target/release/rustfs
B=scripts/bench
# env file $S/rustfs.env (0600): MESH_S3_ENDPOINT=http://127.0.0.1:19410, MESH_S3_ACCESS_KEY, MESH_S3_SECRET_KEY, MESH_S3_REGION=us-east-1
# RAM disk for the data dir
DEV=$(hdiutil attach -nomount ram://3145728 | awk '{print $1}'); diskutil erasevolume APFS s2metaram $DEV
# RustFS (keys come from the env file)
(set -a; . $S/rustfs.env; RUSTFS_ACCESS_KEY=$MESH_S3_ACCESS_KEY RUSTFS_SECRET_KEY=$MESH_S3_SECRET_KEY RUSTFS_CONSOLE_ENABLE=false \
  $R server --address 127.0.0.1:19410 /Volumes/s2metaram/rustfs-data &)
# isolated daemon
export HOME=$S/home WEFTOS_RUNTIME_DIR=$S/rt WEFTOS_STORE_RUSTFS_ENV=$S/rustfs.env BENCH_DEST_ROOT=/Volumes/s2metaram
cd $S && $W kernel start --new-chain
# trees
python3 -I $B/adr126_s2_meta_gen.py $S/trees/sansone sansone        # 38 min on the busy volume
# measurements (each repeats 3 times and appends to $S/logs/results.jsonl)
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S create sansone sansone-a   # also -b, -c
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S publish sansone sansone-a  # then republish
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S fanout sansone-a 20        # pad the registry
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S follow sansone-a
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S search
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S leftovers
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S overlaps sansone-a sansone-b
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S syncdocs sansone-a
python3 -I $B/adr126_s2_meta_bench.py $W $S/rustfs.env $S fetch sansone-a
# raw S3 metadata and the PG round trip
python3 -I $B/adr126_s2_meta_s3.py s3 $S/rustfs.env w-01jbenchs2meta000000000a1a-abcf3259aa30 50
python3 -I $B/adr126_s2_meta_s3.py net http://100.90.170.87:9700/ 20
python3 -I $B/adr126_s2_meta_s3.py net http://127.0.0.1:19410/ 20
```

The per-N raw registry timings and the N=3 case came from a small throwaway helper in the scratch dir that
deleted or added registry objects between runs and timed one list plus all gets 50 times; its output is
the "S3 list" and "S3 all N gets" columns above. The project id is the fixed ULID `01JBENCHS2META000000000A1A`
in the bench script. Stop sequence: `weaver kernel stop`, kill RustFS, `hdiutil detach $DEV`.
