# RustFS small-file write path and the client-side ring (2026-10-30)

Research for ADR-126 (mesh object store) and ADR-128 (mesh mock). Read-only: nothing was run against the RustFS servers. Sources are the RustFS 1.0.4 tag (tag date 2026-10-25) and main at 84e24e7 from the research volume clone, `gh` for upstream issues, and the `feat/shm-ring` branch of the RuVector fork (PR #1272). Line numbers are for the 1.0.4 tag unless stated.

Evidence labels used below:

- **Verified**: read in source or docs this session.
- **Inferred**: follows from verified facts, not observed.
- **Recalled**: prior-art facts from memory of the primary sources. They were not re-fetched this session, so check them before they go into an ADR.

## 1. Executive summary and recommendation

**Recommendation.** Keep the write-behind design, but change three things.

1. **Do not build the ring on RuVector #1272.** It is a lossy, in-memory, fixed-slot ring (verified, section 5). Build the ring as an append-only segment file with checksummed frames and group-commit fsync. #1272 is at most a wake-up channel between local processes.
2. **Make each segment self-describing.** Records, then a footer index, then a fixed trailer, with the segment named by its content hash. The index update to the tree service then becomes a pointer to a segment hash, and the file-to-offset map can always be rebuilt from the object itself.
3. **Ack at the ring fsync, not at the upload.** Other hosts see data only at layer seal, which waits for the uploads.

**Why packing is right whatever the cause of the 43/s cap.** Section 2 shows that a small RustFS PUT in the default `strict` mode costs a chain of fsyncs on a single drive, and upstream has seen a similar flat small-PUT curve on NVMe. Packing turns one fsync chain per file into one per segment.

**Top findings**

1. **Audit is not on the PUT request path.** The handler drops an `OperationHelper`, whose `Drop` spawns a background task (section 2.2). The single-threaded sink cannot throttle PUTs by being slow. The "single-threaded Python sink caps PUTs" hypothesis is therefore unlikely, though not disproved.
2. **Audit can still cost I/O.** With a queue directory set, every event does a blocking create, write, `sync_all`, rename and directory fsync on a tokio worker thread (section 2.2). That competes for the same disk and runtime threads as the PUTs.
3. **The default durability tier is `strict`.** Inline (small) objects fsync xl.meta and the directory entries of the commit renames (docs table, section 2.3). The measured 26 ms per request, flat from 8 to 128 writers, fits a serialized fsync chain better than it fits the audit sink. This is inferred; nobody has counted the fsyncs per PUT.
4. **RustFS rejects append.** `x-amz-write-offset-bytes` returns NotImplemented (`rustfs/src/storage/access.rs:697-700`, test at `:4909`). Segments cannot grow in place. Multipart with `UploadPartCopy` can concatenate existing segments server side.
5. **Range reads of a packed segment read whole shard blocks on the server.** A 4 KiB range costs about a 1 MiB block read on the drive, but only the range goes over the wire (inferred from `bitrot.rs:554`). Cache by segment or by aligned block on the client.

**Audit dispatch answer.** Asynchronous and off the request path. The S3 handler holds an `OperationHelper`. On drop (`rustfs/src/storage/helper.rs:412-426`) it calls `spawn_background_with_context` (`helper.rs:66-77`), which spawns the task onto the runtime (`helper.rs:48-60`). The task runs `AuditLogger::log` (`crates/audit/src/global.rs:143`), then `AuditPipeline::dispatch` (`crates/audit/src/pipeline.rs:49-110`), then `WebhookTarget::save` (`crates/targets/src/target/webhook.rs:586-633`). `complete()` (`helper.rs:319-373`) only builds the entry. The PUT response does not wait for any of it.

## 2. RustFS internals (verified unless marked)

### 2.1 Small-object PUT path

- **Inlining.** `StorageClass::should_inline` (`crates/ecstore/src/config/storageclass.rs:243-256`): inline when the shard size is at most `inline_block`, or at most `inline_block / 8` for a versioned bucket. The default `inline_block` is 128 KiB (`:103`), with a total budget of 256 KiB divided by data shards (`:104`, `:265-276`). On a single drive that is 128 KiB unversioned and **16 KiB versioned**. Env: `RUSTFS_STORAGE_CLASS_INLINE_BLOCK` (`:90`). Single drive is from the S0 results in ADR-126; the shard count was not read from the PG instance.
- **Inline versus part files.** Inline data lives inside `xl.meta`. Larger objects get `part.N` files whose shard payload is fdatasynced before the commit rename (durability docs).
- **O_DIRECT.** Only for erasure shard and part data writes, Linux only, **default off** (`crates/ecstore/src/disk/local.rs:1218-1232`). Large sequential reads can use O_DIRECT above 4 MiB (`:1189-1216`). Small and inline objects never use it.
- **Durability tiers.** `DurabilityMode` is `Strict`, `Relaxed`, `None` or `LegacyOff` (`local.rs:1478-1530`), resolved once per process from `RUSTFS_DURABILITY_MODE` (`:1261`, `:1568`) or the legacy `RUSTFS_DRIVE_SYNC_ENABLE` (`:1256`). `effective_durability(volume)` (`:1757-1766`) pins system volumes to `Strict`, then applies a per-bucket override, then the global mode. Docs: `docs/operations/durability-modes.md`.
- **Which write points sync, per the docs table.** Strict syncs shard files, multipart parts, xl.meta contents, inline objects, rollback backups and the parent directory of every commit rename. Relaxed syncs only shard and part payload, so **inline objects get no per-object fsync**. The docs say single-node deployments must stay on `strict`. This session did not count the fsyncs of one PUT; that needs strace on a throwaway instance.
- **Per-bucket override** without a restart: admin `PUT /rustfs/admin/v3/bucket-durability/<bucket>` (docs; the tier is read per write at the commit point). Seeded for new buckets by `RUSTFS_NEW_BUCKET_DURABILITY_MODE` (`crates/ecstore/src/bucket/durability.rs:70-90`).
- **Group commit.** `RUSTFS_EXPERIMENTAL_DST_DIR_FSYNC_GROUP_COMMIT_ENABLE` and `RUSTFS_EXPERIMENTAL_FILE_FDATASYNC_GROUP_COMMIT_ENABLE`, both default false (`crates/ecstore/src/disk/os.rs:525-528`). Their effect on small PUTs was not measured.
- **Locks.** Writes take a per-object namespace write lock (`crates/ecstore/src/set_disk/mod.rs:5091-5138`). Per object, so different keys on one node should not contend (inferred). The distributed lock issues upstream (#7966, #7962) concern multi-node deployments.
- **Post-write hook.** Every PutObject awaits `enqueue_transition_after_write` (`crates/ecstore/src/store/mod.rs:398`, called at `:1391`, `:1466`, `:1737`). Upstream #8560 (closed 2026-10-16) reported that with **any lifecycle rule on the bucket** this listed the whole parent directory on every write. Whether 1.0.4 includes the fix, and whether `proj-weftos` has a lifecycle rule, are open (section 6).

### 2.2 Audit dispatch, in detail

| Step | Where | Behaviour |
|---|---|---|
| Handler end | `rustfs/src/storage/helper.rs:412-426` | `Drop for OperationHelper` spawns `AuditLogger::log` |
| Spawn | `helper.rs:48-60`, `:66-77` | `Handle::spawn` on the current runtime, otherwise a new thread; fire and forget |
| Fan-out | `crates/audit/src/pipeline.rs:49-110` | `join_all` of `target.save()` over all targets |
| Webhook with queue dir | `crates/targets/src/target/webhook.rs:586-605` | `persist_queued_payload_to_store`, which does not return until the event is on disk |
| Webhook without queue dir | `webhook.rs:606-633` | `send_body` inline in the spawned task |
| Store write | `crates/targets/src/store.rs:121-141`, `:562-596` | blocking `std::fs`: create tmp, write, `sync_all`, rename, then `fsync_dir`, on a tokio worker |
| Replay | `ReplayWorkerManager` (`crates/targets`) | the queue is drained by a replay worker; its concurrency was not read |

Consequences (inferred):

- A slow sink cannot slow a PUT directly. With a queue dir, events pile up on disk; without one, spawned tasks pile up in memory.
- With a queue dir, each audited request adds two fsyncs on a blocking call inside async code. At 43 requests a second that is not much load. At many hundreds it would be, on the same filesystem journal as the object writes.
- Knobs: `RUSTFS_AUDIT_ENABLE` (default off), `RUSTFS_AUDIT_WEBHOOK_QUEUE_DIR` and `RUSTFS_AUDIT_WEBHOOK_QUEUE_LIMIT` (`crates/config/src/audit/webhook.rs:19-20`). Only an on/off switch, so any audit-off test is a restart of the instance.

### 2.3 Tuning knobs

| Env | Default | Effect on small PUTs |
|---|---|---|
| `RUSTFS_DURABILITY_MODE` | `strict` | `relaxed` and `none` remove the metadata fsyncs; power-loss risk (section 2.1) |
| `RUSTFS_NEW_BUCKET_DURABILITY_MODE` | `inherit` | per-bucket tier at creation |
| `RUSTFS_DRIVE_SYNC_ENABLE` | `true` | legacy all-off switch; also drops system-critical fsyncs, do not use |
| `RUSTFS_STORAGE_CLASS_INLINE_BLOCK` | 128 KiB | inline threshold per shard |
| `RUSTFS_RUNTIME_WORKER_THREADS` | 16 (`crates/config/src/constants/runtime.rs:18-48`) | tokio workers; blocking audit fsyncs run here |
| `RUSTFS_RUNTIME_MAX_BLOCKING_THREADS` | not read | blocking pool |
| `RUSTFS_PUT_FOREGROUND_ADMISSION_*` | off (small PUTs) | `crates/config/src/constants/object.rs:346-375`; a gate, not a speed-up |
| `RUSTFS_PUT_LARGE_FOREGROUND_ADMISSION_*` | on, large PUTs only | does not apply to small PUTs |

There is no setting to raise the small-PUT concurrency. The flat curve is not an admission limit.

### 2.4 Append, compose, concatenate

- **Append: not supported.** A PUT carrying `x-amz-write-offset-bytes` is refused with NotImplemented (`access.rs:697-700`, `:4895-4915`).
- **Compose: yes, through multipart.** `UploadPartCopy` is implemented (`rustfs/src/app/multipart_usecase.rs:1628`) and supports byte ranges. `EntityTooSmall` exists in the error enum (`crates/ecstore/src/error/mod.rs:231`). Whether the 5 MiB minimum part size is enforced was not located, so assume S3 semantics: every part except the last at least 5 MiB. Eight MiB segments satisfy that.
- **Audit gap.** `UploadPartCopy`, like `UploadPart`, maps to no event (`crates/s3-ops/src/lib.rs:336-342`), so it never appears in the audit stream, and `CreateMultipartUpload` is lost on replay (ADR-126 audit follow-up). Compaction by multipart is therefore invisible to audit until the PRs in the research volume land.
- **Conditional PUT.** `If-None-Match: *` verified in S0 (412 on existing key).

### 2.5 Upstream issues

- **#7011** (closed 2026-09-03): small-object PUT 7.6x slower than MinIO on identical hardware (4 KiB at concurrency 8: 380 per second against 2,890). The reporter measured about 21 ms per PUT regardless of size (4 KiB to 1 MiB), a curve that flattened at 500 per second by concurrency 32, and 96% of kernel time in futex. Suspects were a global `RwLock` on the Reed-Solomon codec cache and the distributed namespace lock. The maintainer asked for higher-concurrency and `perf` data; the closing change was not traced. This is the same shape as our cap: flat, size-insensitive, a few tens of milliseconds a request.
- **#8560** (closed 2026-10-16): PUT latency grows with the parent directory size when the bucket has any lifecycle rule.
- **#7966**, **#7962**: distributed lock convoy and timeouts. Not applicable to one node (inferred).
- **#8961** (open): scanner memory with about 2 million objects on one disk (1.0.3). Relevant if loose small objects ever reach that count; packing avoids it.

### 2.6 Ranged GET on a packed segment

- Segments are well above the inline limit, so they are stored as `part.N` shard files with per-block bitrot checksums.
- The read computes an encoded range over whole shard blocks (`crates/ecstore/src/io_support/bitrot.rs:554`, used at `:694`, `:770`). The default erasure block is 1 MiB (`crates/ecstore/src/object_api/mod.rs:56`).
- Inferred: a small ranged GET reads and checksums at least one 1 MiB block on the server, and returns only the requested bytes. Cost is server disk and CPU, not WAN bytes.
- Objects up to 128 KiB can be served from memory directly (`crates/ecstore/src/set_disk/mod.rs:748-757`), and objects up to 1 MiB have an inline fast path (`:761`). Segments miss both.

## 3. Prior art

Everything in this table is **recalled**, not re-fetched. Primary sources are named so they can be checked.

| System | Aggregation unit | Durability point | Index | Compaction and GC | Crash recovery | Copy or avoid |
|---|---|---|---|---|---|---|
| JuiceFS (docs: internals, cache) | chunk 64 MiB, slice, block 4 MiB; small files are one object per file, not packed | with `--writeback`, the local staging write; upload is async | slice lists in a metadata engine (Redis, TiKV, SQL) | background slice merge per chunk | staged blocks re-uploaded from the cache dir | Copy: writeback staging and per-block cache. Avoid: no small-file packing, so it does not fix our problem |
| SeaweedFS (wiki: volumes, needles) | volume about 30 GB, append-only needles (cookie, id, size, data, CRC, pad) | needle append (fsync optional) | in-memory needle map or LevelDB, about 16 bytes per file | tombstone delete, vacuum at 30% garbage | index rebuilt by scanning the volume | Copy: needle framing, CRC, scan rebuild. Avoid: mutable volumes on a server we do not control |
| Haystack (Beaver et al., OSDI 2010) | volume about 100 GB, needles | needle append | in-memory, about 10 bytes per photo, with an index file checkpoint | compaction copies live needles | replay the volume tail past the index checkpoint | Copy: one read per file and index-as-checkpoint. Avoid: in-memory index at our scale |
| f4 (Muralidhar et al., OSDI 2014) | immutable warm volumes, Reed-Solomon coded | n/a (cooled from Haystack) | same as Haystack | rare delete by key rotation | erasure repair | Copy: sealed volumes are never rewritten |
| git packfiles (pack-format, multi-pack-index docs) | `.pack` plus `.idx` v2 (fan-out, sorted ids, CRC32, offsets) | pack written to a temp file, `.idx` renamed last | `.idx`, with a trailing checksum on both files | `git gc` and repack; geometric repack | an incomplete pack has no `.idx` and is ignored | Copy: idx last as the commit point, trailing checksum. Delta compression is not needed |
| Kafka tiered storage (KIP-405) | closed log segments (`segment.bytes` 1 GiB, `segment.ms` 7 days) plus offset, time and transaction indexes | the local log append; the tier is a copy | remote log metadata topic | remote retention | segment uploaded again if metadata lacks it | Copy: upload only closed segments with their indexes. Avoid: segment sizes that large |
| WarpStream | agents batch for tens to hundreds of ms into one object per flush | **ack after the object PUT and the metadata commit** | a central metadata store | merge small files by compaction jobs | none needed (no local state) | Copy: the compaction jobs. Avoid: ack at upload, which is slow on our WAN |
| AutoMQ | a small block-store WAL (EBS), then batched objects | ack after the WAL | stream-set object index | background merge | replay the WAL | Copy: local WAL, upload later, which is our design |
| Litestream and LiteFS | SQLite WAL frames or LTX files | async; the last second or more can be lost (default sync interval 1 s) | generation, snapshot and LTX position | retention-based pruning | restore a snapshot, apply LTX files | Copy: the replication position as an explicit "uploaded through" marker. Avoid: relying on a replica for the ack |
| ZFS ZIL and SLOG (OpenZFS docs) | log records on a separate device; transaction group flush about every 5 s | ZIL write; the log is read only after a crash | none (log replay) | freed when the txg commits | replay the ZIL at import | Copy: small writes go to the log, large ones go direct to the pool (`zfs_immediate_write_sz`, 32 KiB) and log only a pointer |
| RocksDB (WAL and SST) | WAL, then memtable flush to a 64 MiB SST | WAL append; fsync per option; group commit by a leader writer | MANIFEST lists the live files; SST footer index | leveled compaction, write amplification of about 10 to 30 | replay the WAL, CRC per record in 32 KiB blocks, stop at the first bad record | Copy: group commit, CRC per record, manifest after data. Avoid: leveled compaction (write amplification) |
| Ceph BlueStore | small writes below a size threshold go through the RocksDB WAL (deferred), large ones go direct to new blocks | KV commit | RocksDB | deferred write replay | replay the KV log | Copy: size-threshold routing |
| S3 Express One Zone | in-place append, up to 10,000 appends, through `x-amz-write-offset-bytes` | per append request | none | none | none | Not available: RustFS rejects it (section 2.4) |
| SlateDB (closest match) | LSM on object storage; WAL objects flushed on an interval (about 100 ms by default), then SSTs | ack after the WAL object PUT (configurable) | a manifest object updated by compare-and-swap | compaction in the background | read the manifest, replay WAL objects | Copy: manifest CAS and content-named immutable objects. Avoid: ack at upload |
| Neon safekeepers | WAL replicated to a quorum first, then image and delta layer files to S3 | quorum ack | layer map | layer compaction | replay from safekeepers | Copy: separate "durable" from "uploaded" |

Read-across:

- Every system separates "acknowledged" from "in the object store", except the diskless ones (WarpStream, SlateDB at its default), which pay an object-store round trip for it.
- The commit point of a self-describing object is written last (git idx, SST footer, LTX trailer).
- Every recovery path scans framed, checksummed records and stops at the first bad one.

## 4. Design risks and recommended choices

### 4.1 Segment size

- Upload time is the first limit. At 10 to 12 MB/s from the Mac, 8 MiB takes about 0.7 to 0.85 s, 32 MiB about 3 s. The 1 MiB loose-object run in the mock gave about 10 MB/s, so the link, not RustFS, is the limit (verified in the mock report).
- RustFS cost is per request, not per byte, once the object is above the inline size. At the observed 43 requests a second, an 8 MiB segment would still need to be cut faster than 43 a second to hit the cap, which a 10 MB/s link cannot do.
- **Choice: cut at 8 MiB or after 2 s with data in the open segment, or at a layer-seal request**, whichever is first. The size is a ceiling, not a target. Small tails are fine because cutting 43 segments a second is not reachable. Keep 2 to 4 uploads in flight and measure whether more than 2 helps on this WAN; beyond the link bandwidth it will not.
- Do not go below 4 MiB: it multiplies requests and the bitrot-block read amplification for no visibility gain. Do not go above 16 MiB: the seal wait for the last segment grows and the loss of one in-flight PUT grows.

### 4.2 Byte-range GET efficiency

- Server side, at least one 1 MiB block is read and checksummed per ranged GET (section 2.6). Wire bytes are exact.
- **Choice:** the client fetches by segment when it needs more than a few files from it, otherwise by aligned 1 MiB blocks into a local block cache. Order files into segments by directory so that a checkout reads few segments (git does this with sorted pack objects). A random 4 KiB read per file against RustFS is the case to avoid.
- For a large `node_modules` or `.cargo` tree, the mock already moved 524 MB in 21 packs, so the read pattern is whole packs.

### 4.3 Crash between segment upload and index update

The problem is an object in the bucket that no index names, or an index that names an object that is not there.

- **Choice:** name the segment by the blake3 hash of its bytes and upload with `If-None-Match: *`. A 412 means the same content is already there, so it counts as success. The upload is idempotent.
- Order: write the segment PUT, then the index record, then mark the local segment uploaded. A crash at any point replays to the same end state, because every step is idempotent and deterministic.
- The index is a pointer plus the footer. The tree service stores "layer L references segment S". The file-to-offset map is in S's footer, reached with a tail GET (read the trailer, then the footer). If the service copy of the map is lost, it is rebuilt from the objects.
- **Orphans** (uploaded, never referenced by a sealed layer) are collected by mark and sweep from the layers, with a grace period longer than the longest open layer. This is the GC item already listed for B2 in the mock report.
- The reverse failure (index names a missing segment) cannot happen if the seal checks the 200 or 412 before it writes the reference. The mock's existence check at accept time stays as the second guard.

### 4.4 Compaction and write amplification

- Segments are immutable and content-addressed, and layers are an immutable lineage, so garbage appears only when layers are deleted.
- **Choice for v1: no compaction.** Delete only segments with zero live blobs after the grace period. Write amplification is 1.
- **Later:** rewrite segments below 50% live, and merge small segments with multipart `UploadPartCopy` so the bytes never cross the WAN. Leveled or tiered schemes are not needed at this churn. With a 50% threshold the worst case is rewriting one live byte per dead byte (2x on the rewritten set). Compaction through multipart is invisible to RustFS audit today (section 2.4).
- Deduplicate before appending: if the tree service already knows the blob hash, do not write it again. This is the larger saving than compaction.

### 4.5 Ack at ring fsync or at upload

- **Choice: ack at the ring fsync.** At 10 MB/s with WAN latency, acking at upload would make every `write()` seconds long; WarpStream and SlateDB pay that only because they have no local disk to ack on.
- Multi-host correctness comes from the seal, not the ack. A write is private to the writing host until the layer seals; a layer seal waits for all of its segments' uploads and their references. Another host reads only sealed layers, so it never sees a blob whose bytes are only in someone's ring.
- Loss model to state in ADR-128: losing the host's disk loses the unsealed work of that host, and nothing else. Orphans from a crash are collected by GC.
- On macOS a plain `fsync` does not flush the device cache; `F_FULLFSYNC` does. Rust's `File::sync_all` uses it on Apple targets, which is believed true but should be checked against the toolchain pin. The group-commit fsync must be the full one.

## 5. The ring primitive: RuVector PR #1272

`gh pr view 1272 -R ruvnet/RuVector` reports branch `feat/shm-ring`, state OPEN, title "feat(shm-ring): shared-memory record ring with a caller-chosen byte size", adding the `ruvector-shm-ring` crate. Read from the branch (verified).

**What it is.** A single-writer, many-reader record ring over shared memory (`crates/ruvector-shm-ring/README.md`, `src/ring.rs`).

- **Not durable.** `allocate` creates a Linux memfd, or on macOS a POSIX shm object whose name is `shm_unlink`ed before it returns (`src/mem.rs:197-290`). There is no file path, so nothing survives a process exit. Contents go with the last close.
- **Not file-backed by default, but mappable from a file.** `Mapping::map(fd, len, writable)` (`mem.rs:86-134`) calls `mmap(MAP_SHARED)` on any descriptor, so a regular file would map. The crate offers no API for that, no `msync`, no `fsync`, and no recovery.
- **Lossy by design.** The writer never waits. A reader that falls behind skips to the oldest intact record (`ring.rs:188-206`); the slot for `seq` is `(seq - 1) % slot_count` (`ring.rs:15`), so an unread record is overwritten.
- **Fixed-size slots.** `Geometry::fit(max_record, ring_bytes)` makes every slot `max_record` plus a 32-byte header rounded to 64 (`src/geom.rs`). With a 512 KiB `max_record` every slot is 512 KiB, so a 200-byte source file wastes about all of its slot.
- **No record checksum.** Integrity is the seq word read before and after the copy (`ring.rs:15-18`, `:211-229`). That detects a concurrent overwrite, not a torn or partially flushed record after a power loss.
- **Ordering is process-level only.** The writer stores BUSY, writes fields, stores seq, then stores head (`ring.rs:149-162`). Atomics order other threads. They do not order when the kernel writes the pages to disk, so on a crash a slot can have its seq word on disk and its payload not.
- **Single writer, not enforced.** `attach` does not stop an earlier writer; "two live writers corrupt the ring" (`ring.rs:99-103`). A durable ring would need a lock (flock on the file).
- **64-bit only** (`ring.rs:245-254`).

**Fit.** It cannot serve as the crash-safe write-ahead ring. To do so it would need: a file path and open and recover API; an fsync or `msync` barrier with group commit; a CRC (or hash) per record; variable-length records with a log-structured layout; no-overwrite plus backpressure instead of overwrite; a writer lock; and a recovery scan that truncates at the first bad record. That is a different data structure.

**Where it could still help.** As a lossy wake-up or "new blob at ring offset N" feed from the writing process (overlay or FUSE) to the uploader and to local readers, where loss is harmless because the WAL file is the truth and can be rescanned. A plain pipe or a channel does the same job inside one process. Recommend not depending on it for the first slice.

## 6. Concrete changes to the ring design

Replace the description in ADR-126 and ADR-128 ("small files are appended to a local ring and fsynced") with:

1. **WAL file, not shared memory.** One open segment file per host and project in a ring directory: `<seq>.open`. Record frame: magic, length, blake3 of the content, CRC32C of the header, then the bytes. No file path in the frame; paths and modes live in the layer's op list, which refers to blobs by hash. This keeps the ring a content store and makes dedup trivial.
2. **Group commit.** One writer thread appends; fsync once per batch or every few milliseconds, whichever is first. Ack waiters when the fsync returns. Use a full fsync on macOS.
3. **Cut.** At 8 MiB, after 2 s, or on a seal request. Write a footer (sorted index of blob hash, offset, length) and a fixed trailer (magic, footer length, footer CRC, content hash), fsync, then rename `<seq>.open` to `<hash>.sealed` and fsync the directory.
4. **Upload.** `PUT segments/<hash>` with `If-None-Match: *`; treat 200 and 412 as success. Then record the reference with the tree service. Then rename to `<hash>.uploaded` and keep the file as a read cache under an LRU.
5. **Reads.** In order: open segment (in-memory map of hash to offset), sealed and uploaded local segments (`pread`), the block cache, then ranged GET of aligned 1 MiB blocks (or a whole-segment fetch when several files from one segment are wanted).
6. **Seal of a layer.** Waits for every referenced segment to be uploaded and referenced. A layer never contains a blob that is only in a ring.
7. **Crash replay.** Scan the ring directory: truncate each `.open` file at the first bad frame, seal and upload it; re-upload any `.sealed` file (a 412 is fine); re-emit references. Nothing needs a journal because every step is idempotent.
8. **GC.** Mark and sweep from sealed layers with a grace period; v1 deletes only fully dead segments.
9. **Metadata.** Keep the binary, compressed layer metadata item from the mock report; this design does not change it.
10. **RuVector #1272** is not on the critical path. Revisit it only if a cross-process wake-up channel is needed.

## 7. Open questions

1. **What caps small PUTs at 43 per second?** This research makes the audit sink unlikely as the direct cause (section 2.2) and `strict` fsyncs the leading candidate (section 2.3). A 2x2 on a throwaway instance would settle it: audit on or off against `strict` or `relaxed`, with the per-bucket override available without a restart. Add `iostat` or `fio` fsync latency for the PG data directory, and strace counting of fsyncs per PUT. Another agent is measuring; these cells may already be covered.
2. **Does `proj-weftos` have a lifecycle rule, and is it versioned?** A lifecycle rule triggers the #8560 behaviour if 1.0.4 lacks the fix. Versioning cuts the inline limit to 16 KiB.
3. **Is the 5 MiB minimum part size enforced by RustFS?** It decides whether `UploadPartCopy` can merge small segments.
4. **Does `UploadPartCopy` read through the same bitrot path and fsync strictness, and how fast is it for 8 MiB parts?** Not measured.
5. **Replay worker concurrency for the audit queue** was not read. If the sink is slow, the queue grows on disk; the queue limit behaviour (`RUSTFS_AUDIT_WEBHOOK_QUEUE_LIMIT`) when full was not read.
6. **Prior-art figures** in section 3 need a pass against the primary sources before they are quoted.
7. **Upload concurrency** (2 against 4 in flight) on the Mac to PG link needs one measurement.
8. **Should hot loose blobs under a size, say 16 KiB, bypass segments for single-file latency?** Probably no. Measure the seal-to-visible latency with the 2 s cut first.
