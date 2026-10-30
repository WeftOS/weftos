# RVF as a container format, and DiskANN: a source study for ADR-128

Date: 2026-10-29. Read-only study, no code changed.

Source: ruvnet/ruvector `main` at `fefc2ec9` (shallow clone). Paths below are relative to that repo. Paths starting `rvf/` mean `crates/rvf/`, and `rvf-runtime/` means `crates/rvf/rvf-runtime/`. Where a claim comes from a published crate rather than `main`, the crate version is named. The ruvnet-brain tool was not used (it was failing), so everything here is from source. ADR-128's RVF row is out of date, and this study is the basis for rewriting it.

## Summary

- RVF is a container: append-only, 64-byte-aligned segments of many types, found through a tail manifest.
- `RvfStore` (the runtime) gives you more than ADR-128 assumed:
  - native `branch()`/`freeze()` with parent read-through (already in 0.3.2, the version we lock);
  - keyed lookup of per-record metadata and file-level key/value metadata (new in 0.4.0);
  - Ed25519 signing of the whole file;
  - a verified streaming protocol.
- It does not give you:
  - user-defined segment types;
  - inheritance of anything but vectors and metadata;
  - multi-parent composition;
  - merge or promote-back.
- The two sharpest findings for ADR-128 are about `branch()`, not about the format:
  - It sizes a dense membership bitmap from the highest vector id, capped at 64 MiB. Path-hash ids therefore cannot be used as vector ids.
  - A child's metadata is a full copy, not a reference to the parent's.
- DiskANN is a separate crate, not part of RVF. It has no copy-on-write and no membership filter, and its search ignores product quantization.

## 1. The container model

- **Segments.**
  - Every segment starts with a fixed 64-byte header: magic `RVFS`, type byte, flags, segment id, payload length, timestamp, hash algorithm, compression and a 16-byte content hash (`rvf-types/src/segment.rs:6-40`).
  - Segments are padded to 64 bytes (`rvf-runtime/src/write_path.rs`, the `SegmentWriter` docs).
- **Type registry.**
  - `SegmentType` is a closed `repr(u8)` enum (`rvf-types/src/segment_type.rs:9-77`):
    - 0x01-0x11: VEC, INDEX, OVERLAY, JOURNAL, MANIFEST, QUANT, META, HOT, SKETCH, WITNESS, PROFILE, CRYPTO, META_IDX, KERNEL, EBPF, WASM, DASHBOARD.
    - 0x20-0x23: COW_MAP, REFCOUNT, MEMBERSHIP, DELTA.
    - 0x30-0x36: federation and learning payloads.
  - `TryFrom<u8>` returns `Err(byte)` for anything else (`segment_type.rs:81-113`, and a test asserting 0x12 is unknown at line 170).
  - ADR-031 says 0x24-0xEF are "available" and 0xF0-0xFF reserved (`docs/adr/ADR-026-rvcow-branching-and-real-cognitive-containers.md`; `rvf/docs/adr/ADR-031-*.md` §5.2 is the same text).
- **Manifest.**
  - There are two levels (`rvf-manifest/src/lib.rs:1-8`): a Level-0 root page of 4096 bytes at the end of the file (magic `RVM0`) and a Level-1 directory.
  - `RvfStore` does not use `rvf-manifest`'s directory codec. It writes its own manifest directory of `(seg_id, offset, len, type)` entries (`rvf-runtime/src/store/security.rs:30-36` says so explicitly).
  - Readers find the manifest by tail scan.
- **Our own segment types or typed payloads.**
  - Not supported through `RvfStore`.
    - Its only generic payload entry points are `embed_kernel/ebpf/wasm/dashboard` (`store.rs:2087-2540`).
    - There is no "append segment of type N" API.
  - The framing layer is type-agnostic: `rvf_wire::writer::write_segment(seg_type: u8, payload, flags, segment_id)` (`rvf-wire/src/writer.rs`).
  - `compact()` preserves unknown-type segments byte for byte by scanning the file (`store.rs:1685-1688`, `4567`), and `ErrorCode::UnknownSegmentType` is documented as "advisory, not fatal".
  - **Not established:** whether a segment appended outside `RvfStore` stays in the runtime's directory across later commits. The runtime rewrites its own directory, so it may be untracked until a compaction.
  - **The practical typed carrier is META_SEG.** The AGI container manifest is itself a META segment (`rvf-runtime/src/agi_container.rs:1-8`), with tagged blobs (`rvf-types/src/agi_container.rs:54-96`).
- **Integrity, witness and signing.**
  - Per-segment content hash.
    - The default is XXH3-128, which is integrity only. SHAKE-256-128 is used when signing (`rvf-runtime/src/hashing.rs:1-12`, `policy_api.rs:15-25`).
  - Witness chain.
    - A WITNESS_SEG is appended after ingest, delete and compact by default, and after query if `audit_queries` is on (`options.rs:66-87`).
  - Signing is whole-file, not per segment.
    - `create_signed`, `open_signed` and `open_with_policy` (`store/policy_api.rs:26,48,71`).
    - The Level-0 page carries an Ed25519 signature (`root_page.rs:1-25`) and commits to a per-segment SHAKE hash trailer. A swapped segment is refused (`security.rs:7-25`).
    - Only Ed25519 exists. The doc comment says ML-DSA-65 is named in ADR-033 but "only Ed25519 exists in this codebase today" (`policy_api.rs:21-23`). Our `weftos-rvf-crypto` fork adds ML-DSA, and it is not established that it plugs into this path.
    - A freshness choice is mandatory for signed opens (`options.rs:128-134`).

## 2. Copy-on-write and lineage

- **What `branch()` does** (`store.rs:3020-3096`).
  - It calls `derive()`, creates a `CowEngine` with every cluster a `ParentRef`, and creates a membership filter of the parent's live vector ids.
  - Vectors are not copied. The child reads through to the parent (`store.rs:1228`, `query_via_index_cow` at about 960).
  - Writes copy one cluster (`CowMapEntry::LocalOffset`, `rvf-runtime/src/cow_map.rs`).
  - Clusters are 4096 bytes or more, with `vectors_per_cluster = 4096 / (dim*4)`.
  - Only the flat-array map format is implemented (`cow_map.rs:3-5`).
- **`derive()` is not copy-on-write.**
  - It sets `cow_engine: None` (`store.rs:3217`).
  - It ignores its `DerivationType` argument (`_derivation_type`, `store.rs:3146`).
  - **clawft-cow-memory calls `derive()` (`node.rs:101`), so it gets lineage and no read-through.** That is why it needs its own layer walk.
- **`freeze()`** marks the engine frozen at an epoch, writes a manifest and sets the handle read-only (`store.rs:3098-3112`).
- **Segment types that inherit.**
  - Vectors, through the cluster map and membership filter.
  - Per-vector and file metadata. These are not read through. `derive()` clones the parent's in-memory metadata (`store.rs:3196-3197`). The child's first commit is a full snapshot, because its chain starts empty (comment at `store.rs:3198-3201`). On reopen, a child with no local META takes the parent's whole (`store.rs:3739-3742`).
  - The HNSW index is shared through the dual-graph query (parent index plus child index, over-fetch 4x, child wins). This is skipped when a metadata filter or `force_exact` is set (`store.rs:909-933`).
  - Nothing else. Kernel, WASM, witness and custom payloads are not inherited.
- **Identity and verification.**
  - `FileIdentity` is `file_id`, `parent_id`, `parent_hash` and `lineage_depth` (`rvf-types/src/lineage.rs:48-60`).
  - Open verifies parent id and depth, then compares a hash (`store.rs:3655-3680`), with a depth cap of 1024 (`store.rs:57`).
  - **That hash is weak.** `compute_own_manifest_hash` hashes only the last 64 KB of the parent with a non-cryptographic XOR-fold named `simple_shake256_256` (`store.rs:3233-3256`, `4546-4558`). The source comment says production lineage should use `rvf_crypto::compute_manifest_hash`. Lineage is tamper-evident only if you also sign.
  - **Branches and derived children are unsigned.** `derive` builds `SecurityState::permissive()` (`store.rs:3225`). Whether `branch()` works under a signed parent is not established.
- **Several parents.**
  - Not supported. There is one `parent_id` and one `parent_path`.
  - `DerivationType::Merge` exists as a label (`lineage.rs:15`, `rvf-cli/src/cmd/derive.rs:29`), but no code path merges.
  - `rvf-federation` is federated learning (PII stripping, differential privacy, FedAvg; `rvf-federation/src/lib.rs:1-8`). It is not store composition.
- **Merge or promote-back.** None in `rvf-runtime` or `rvf-cli` (grep of `fn merge`/`fn promote`). Our `promote` is ours.

## 3. Keyed data

- **Per-record metadata.**
  - `get_metadata(vector_id) -> Option<Vec<MetadataEntry>>` (`store.rs:2744`).
  - Write it with `ingest_batch_with_metadata(vectors, ids, &[VectorMetadata])` (`store.rs:506`).
  - Values are Null, Bool, U64, I64, F64, String or Bytes. Fields are `u16` ids. A delta can carry `DeleteField` or `delete_record` (`options.rs:295-387`).
  - Both entry points are absent in 0.3.2 and present in 0.4.0.
  - **A record needs a vector.** Metadata ids are validated against live vectors or the membership filter (`store.rs:3944-3953`). The minimum dimension is 1.
- **File-level metadata.**
  - `get_file_metadata(key)`, `set_file_metadata(key, value)` (`store.rs:2754-2771`). The `rvf.` prefix is reserved.
  - Each set writes one META generation, fsyncs, then writes a manifest and fsyncs again (`store.rs:2956-2985`). Use it for tens of keys, not 90k.
- **Persistence.**
  - META_SEG is a generation chain: a full snapshot, then deltas, with a fresh snapshot after 32 deltas (`store.rs:63-69`, `4073-4082`).
  - A delta carries only changed file keys (`store.rs:3978-4000`).
  - Limits (`rvf-types/src/metadata.rs:14-19`): 64 deltas, 256 MiB decoded, 16.7M records, 65,536 fields, 16 MiB per value.
  - So file metadata is not rewritten whole per commit, but a branch's first commit is a full snapshot.
- **Lookup cost.**
  - The in-memory store is `BTreeMap<u64, BTreeMap<u16, MetadataValue>>` (`filter.rs:66-68`), loaded fully at open. Lookup is O(log n).
  - Filtering by field value is a linear scan with a budget of 1M records and 256 MiB (`options.rs:193-205`).
- **META_IDX_SEG** is a type code only. Nothing in the runtime, wire or index crates reads or writes it (grep).
- **90k keyed records, then.**
  - The data volume is fine. At about 100 bytes per record that is about 9 MB per full snapshot.
  - The id scheme is the problem (next section).

### The id constraint that breaks "path hash as id"

`branch()` allocates the membership bitmap at `max_vector_id + 1` bits and fails with `MembershipInvalid` when that exceeds `MAX_MEMBERSHIP_FILTER_BYTES`, 64 MiB (`store.rs:60`, `3025-3058`). That is about 537M ids. Hashed 64-bit path ids will not fit. Use dense ordinals assigned per lineage, and keep a path-to-ordinal map as metadata or in a layer above RVF.

## 4. DiskANN

- **What it is.** `ruvector-diskann` 2.3.2 is a Vamana graph (alpha-robust prune, medoid entry) with optional product quantization (`ruvector-diskann/src/lib.rs`, `graph.rs`, `pq.rs`).
- **Not inside RVF.**
  - No `rvf-*` dependency (`ruvector-diskann/Cargo.toml`).
  - Nothing in `crates/rvf` references it except a comment in `rvf-index/src/hnsw.rs:7` saying RVF's HNSW borrows Vamana-style pruning.
  - It persists as a directory of separate files: `vectors.bin`, `graph.bin`, `ids.json`, `tombstones.bin`, `pq.bin`, `pq_codes.bin`, `config.json` (`index.rs:318-450`).
  - It uses string ids.
- **The index that lives in RVF is HNSW.**
  - `rvf-runtime/src/index_path.rs:1-25` builds HNSW lazily, appends it as an INDEX_SEG on close, and rebuilds it if stale.
  - It activates only at 1024 or more vectors, and falls back to a brute-force scan under a metadata filter or when more than 25% of vectors are deleted.
  - The index API has a "progressive" three-layer form (`rvf-index/src/progressive.rs`, `lib.rs:1-7`), but `RvfStore` only persists the full HNSW.
  - RaBitQ (1-bit codes plus exact rescore) is the opt-in memory saver (`rabitq_path.rs`). It is disabled for COW and membership stores (`store.rs:787-796`).
- **Copy-on-write and membership.**
  - DiskANN has neither. Deletes are tombstones, and an mmap-loaded index rejects inserts (`index.rs:109-114`).
  - A branch would mean copying a whole index directory.
- **Scale and memory.**
  - Vectors can be mmap-backed with `load_mmap`.
  - The graph is `Vec<Vec<u32>>` read fully into RAM (`graph.rs:66-72`, `index.rs:626-640`), as is the id map. At degree 64 that is about 250 bytes per node before the strings.
  - So "disk-based" means the vectors are on disk. The graph is not.
  - **Search ignores PQ.** `search_with` calls `greedy_search_fast(&self.vectors, ...)` and re-ranks with exact distances (`index.rs:229-262`). PQ codes are trained and saved but never read on the query path, although ADR-144 describes PQ filtering (`docs/adr/ADR-144-diskann-vamana-implementation.md`).
  - Recall figures in the ADR are for 2K vectors at 64 dimensions. I found no billion-scale measurement. Treat the "billion-scale" claim as unverified.
- **The "silent brute-force stub" note.** The `diskann` cargo feature and `DiskAnnBackend` in that note are WeftOS kernel code, not ruvector's. Nothing in ruvector matches. The finding stands independently: our feature is off by default.
- **For agent memory at scale.** The practical options are RVF's HNSW (up to a few million vectors in RAM), RaBitQ for compression, or `ruvector-lsm-ann` (2.3.0, an LSM-style write-optimised index aimed at agent memory; not read in depth). DiskANN as shipped does not add much over HNSW for our sizes.

## 5. Publication and streaming

- **Protocol** ("project Ofer", ADR-353, status Proposed in the file, code present on `main`).
  - A small signed descriptor commits to every byte of an unchanged RVF body (`docs/adr/ADR-353-rvf-stream-descriptor.md`, Decision).
  - The descriptor holds a segment table (id, type, stage, offset, length, chunk-table hash), SHA-256 chunk hashes and an Ed25519 signature (`rvf-types/src/stream.rs:1-40`).
  - A receiver releases chunks only after they match, and pins the highest epoch seen per `(signer, file_id)` (`rvf-wire/src/stream/mod.rs`).
  - The descriptor can be inline or a sidecar `name.rvf.rvsd`.
- **Transport.**
  - The client talks to a `Transport` trait that returns a frame stream from a byte offset, with resume and per-frame retries (`rvf-runtime/src/progressive_stream/client.rs:1-60`).
  - `StreamPlan` makes the frame stream addressable by byte range (`plan.rs:1-10`).
  - `rvf-server` ships an HTTP `Range` transport and a TCP one (`rvf-server/src/stream_client.rs:1-4`). Its normal routes are `/v1/ingest`, `/v1/query`, `/v1/delete`, `/v1/status` (`rvf-server/src/http.rs`).
- **S3 and fetch by hash.**
  - Not native. The framing is a frame stream built by `StreamPlan`, not the raw file. And it is whole-body, not "fetch one sealed layer by its hash".
  - It is buildable: store one sealed layer per object plus its `.rvsd`, and implement `Transport` over ranged GETs with frame reconstruction. **Not established:** whether a raw-object adapter is simpler than storing the frame stream itself.
  - Whole-file Ed25519 signing (section 1) plus a content-addressed object name is the simpler fallback for sealed layers, and it does not need Ofer.

## 6. Upgrade from 0.3

| Crate | crates.io latest | `main` | WeftOS lock |
|---|---|---|---|
| rvf-runtime | 0.5.0 (2026-10-28) | 0.5.0 | 0.3.2 |
| rvf-types | 0.3.0 (2026-10-28) | 0.3.0 | 0.2.1 |
| rvf-wire | 0.2.0 | 0.2.0 | (weftos fork 0.2) |
| rvf-crypto | 0.3.0 | 0.3.0 | weftos-rvf-crypto 0.3.0 (fork) |
| rvf-index / rvf-manifest | 0.2.1 / 0.2.0 | same | not used directly |
| rvf-federation | 0.1.0 | 0.1.0 | not used |
| ruvector-diskann | 2.3.2 | 2.3.2 | not used |

rvf-runtime 0.4.0 shipped 2026-10-27, so the metadata API and the signed-open API are one day old at 0.4.0 and two days at 0.5.0. Treat them as unproven.

Breaking changes, from diffing the 0.3.2 and 0.4.0 crate sources against `main` (`rvf-runtime/src/options.rs`, `store.rs`):

- `RvfOptions` gained `trusted_signers`, `freshness`, `serve_signed_prefix`. `QueryOptions` gained four fields (including `include_metadata`). `SearchResult` gained `metadata`. clawft-cow-memory builds `RvfOptions` with `..Default::default()` (`branchable_memory.rs:81,149`, `manifest.rs:200`), so it is unaffected. Any struct literal of `SearchResult` would break.
- `MetadataValue` gained `Null`, `Bool`, `DeleteField` (exhaustive matches break).
- `rvf_types::ErrorCode` is now `#[non_exhaustive]` (`rvf-types/src/error.rs:9`). rvf-types 0.2.1 to 0.3.0 adds `metadata.rs` and `stream.rs`. **The full rvf-types diff is not established**, because the line endings made a text diff unusable.
- Unchanged signatures: `create`, `open`, `open_readonly`, `ingest_batch`, `query`, `delete`, `derive`, `branch`, `freeze`.
- The 0.5.0 delta (0.4.0 to 0.5.0) was not diffed.
- Dependency risk. rvf-runtime 0.5 depends on upstream `rvf-crypto` 0.3.0 (`rvf-runtime/Cargo.toml`). We use a fork named `weftos-rvf-crypto`. Whether two crypto crates co-resolve cleanly is not established.

**Is the layer walk still needed?**

- Not for the reason in `chain_walk.rs` (its header says `RvfStore::query` "does not read through the COW boundary" and cites 0.2.0).
- 0.3.2 already has `branch()` with parent read-through (`store.rs` line 2255 in the 0.3.2 crate; the 0.3.2 crate also has `parent_store` and `query_via_index_cow`).
- The walk is needed today because the crate uses `derive()`, not `branch()`.
- Switching to `branch()` removes the vector walk. It brings constraints:
  - the parent must be frozen and unchanged (hash check);
  - a single parent only;
  - dense ids (section 3);
  - the tombstone handling the crate does itself (`BranchableMemory::delete` never calls `RvfStore::delete`) would need re-verifying against the membership filter.
- A multi-source compose (several inherited layers) still needs our own walk.

## 7. Fit for ADR-128

| ADR-128 need | Verdict | Mechanism |
|---|---|---|
| Per-branch CoW tree metadata (paths, entries, dir listings, chunk refs) | **Buildable on top** | Records as `ingest_batch_with_metadata` with a dense ordinal id, a 1-dimension vector and `Bytes` fields. `branch()` for the lineage, `get_metadata` for lookup. Keep path-to-ordinal and directory structure in the records. Cost to measure in T2: a branch's first metadata commit is a full copy, and 90k records is roughly 9 MB. Lookup is O(log n) after a full load at open. |
| Branch cost under 50 ms, 10-layer lookup at most 2x | **Not established** | `branch()` writes two manifests and builds a membership bitmap. Child reads fall through to the parent for vectors, but metadata is copied, not chained. Needs the T2 benchmark. |
| Agent memory layers | **Native** (vectors) / **buildable** (facts and packs) | Vector retrieval with branch/freeze is the designed use. Non-vector facts go in metadata or META blobs. |
| Compose about 5 layers per job | **Not supported natively** | One parent only. Compose in our layer (an ordered list of read-only stores plus a writable tip), as `clawft-cow-memory` does for `inherited`. Precedence per record type is ours to define. |
| Lineage-ordered merges | **Buildable** | `parent_id` and `lineage_depth` give the DAG edge. No merge or promote exists. The integrator reads the lineage, and promote is replayed by us. Use a real hash for the dependency edge, not `parent_hash`. |
| Content-addressed, signed layers | **Partly native** | Ed25519 whole-file signing with per-segment SHAKE hashes exists, but children are unsigned and the lineage hash is weak. Hash and sign each sealed layer ourselves (SHA-256 of the file plus our Ed25519 or ML-DSA envelope, ADR-063). |
| Publication over the substrate, fetch by hash | **Buildable** | Announce `file_id`, `parent_id`, content hash. Fetch from the object store. Verify the signature, then open read-only. Ofer streaming adds incremental verification and is optional. |
| A WeftOS-defined keyed segment inside RVF (T2 fallback) | **Not supported** | The type enum is closed and `RvfStore` cannot append custom types. The fallback has to be a META blob or a sidecar file. A fork of rvf-types/runtime is the only way to get a real segment type, which defeats staying on upstream. |

## What I would tell main

1. Rewrite the RVF row. In 0.3.2 `branch`, `freeze` and read-through already exist. In 0.4.0 and later there are keyed metadata and signed open. Segment types are closed.
2. Drop "path hash as id" from T2. Use dense ordinals.
3. Add to T2 the first-commit full-copy cost and the weak parent hash.
4. Treat DiskANN as out of scope for ADR-128. It has no CoW and keeps its graph in RAM.
5. Decide whether the dependency on a two-day-old runtime is acceptable. The non-ruv alternative for keyed tree metadata is SQLite or a small sorted-table format (one file per layer, a content hash, and a parent reference). It has no vector semantics but has lineage, lookup and signing without these constraints. RVF's advantage is the shared carrier with agent memory. If the tree metadata does not need vectors, that argument is the only one for RVF there.
