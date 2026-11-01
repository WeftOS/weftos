# ADR-131: The seed drive: a project mounted as a materialised native tree over the seed store, with the seed cone as the primary file holder and PG as standby and backup

- **Status**: Proposed (2026-11-01). Design only: nothing in this ADR has been built, deployed or written to any bucket or server.
- **Date**: 2026-11-01
- **Deciders**: owner
- **Prior decisions**: the decision store, consulted on 2026-11-01, holds nothing on a client-facing mount; ADR-126, ADR-128 and ADR-130 are the records on this topic.
- **Research**: `docs/research/seed-drive-design-2026-11-01.md` (evidence, platform documentation, code citations, verified versus inferred).
- **Builds on**:
  - ADR-130: the GCS seed store, the seed cone (SC), signed heads, the lease, the waker, create-only upload URLs;
  - ADR-128: RVF layered trees, copy-on-write branches, the flat `(depth, name)` merge, the ring write-behind; this ADR answers its gate G2 (the mount route) for client nodes;
  - ADR-126: the mesh object store (RustFS), kept as the optional L1 tier;
  - ADR-129: `weftos-pq-sign`, the Ed25519 + ML-DSA-65 dual signature;
  - ADR-057: per-path read access; ADR-103: the per-user daemon and machine mesh service; ADR-114: `weftos://` names.
- **Changes**: ADR-128 G2 is answered for client nodes (a materialised tree, not FUSE). ADR-130 §9 (client data) is made concrete. The layer entry format gains attributes (mtime, directories, chunk lists). ADR-128 and ADR-130 carry Updated notes pointing here.

## Context

The owner, 2026-11-01: "we need to understand how to build the drive, so we can make SC the primary file holder." Earlier direction (2026-10-29): "a copy from remote to local should never need to happen"; git can live elsewhere; RVF carries metadata; "weftos can wrap both".

What the measurements already say:

| Fact | Source |
|---|---|
| Native macFUSE makes stat-heavy work slow: warm `git status` on WeftOS 2.1 to 2.5 s against 0.09 to 0.22 s on local disk; Linux kernel tree 31 to 34 s against 0.31 s. The cost is the macFUSE round trip per `lstat`, not metadata latency (0.16 ms) | JuiceFS spike, T1 "Mac native (macFUSE)" |
| The same FUSE mount inside a Linux VM is fine: warm `git status` 0.11 s (WeftOS) | JuiceFS spike |
| APFS clones of a content-addressed cache give a working tree with no bytes moved: the Mac adopted its checkout in 12 s and fetched 18 KB | Mesh mock, scenario 5 |
| An unprivileged user-namespace overlayfs over per-layer directories works on a host with no reflinks and no root | Mesh mock, "What differed" |
| A warm join of a new head is 0.45 to 0.54 s and three to five GETs; a cold base join is 14.0 s on the Mac, 3.8 s on the CI host | ADR-130 gates 3 and 5, dual signing |
| Last client upload to new head visible: 1.1 to 1.4 s with the SC awake; 72 s when it has to be woken | ADR-130 gate 5, production build |
| Layers carry no mtime, so a restored `target/` rebuilds | Mesh mock, open findings |
| Conflicts must be records, never silent drops | ADR-128 §7, mesh mock finding 8, ADR-130 gate 5 |

So the drive cannot put a FUSE round trip under every `stat` on the Mac, and the store already gives a cheap way to build a real local tree.

## Decision

### 1. The drive is a materialised native tree, kept in step with the seed store by a drive daemon

**The seed drive** is a directory at a fixed path per project (for example `~/weft/<project>` on a laptop, the project's existing path on a server). Every file in it is a **real file on the local file system**, so `stat`, `open`, `read`, `mmap`, `fsync` and `rename` are native. A per-user drive daemon (part of the WeftOS daemon, ADR-103) does four things around it:

1. **Materialise.** Files are placed from a local content-addressed cache (CAS) by copy-on-write clone (`clonefile` on APFS, `FICLONE` on XFS or btrfs), so placing a file moves no bytes. A blob enters the CAS once, is hash-checked once, and is kept for good (content-addressed objects never change).
2. **Capture.** A file-system watcher (FSEvents, inotify or fanotify, ReadDirectoryChangesW) marks changed paths; a scan against a `(size, mtime, hash)` index confirms them; changed contents are snapshotted and appended to the ring (ADR-128).
3. **Seal and send.** At a checkpoint (quiescence, an explicit `weft drive seal`, or a timer) the daemon seals a signed layer and uploads it to the inbox (§3.3).
4. **Refresh.** On a new signed head it applies the delta to the paths the user has not changed locally, and turns overlaps into visible conflicts (§3.5, §3.6).

FUSE is not on the hot path on any client. It stays available on Linux for server-side views (§2).

### 2. Per platform

| Platform | Primary | Cold data | Not chosen, and why |
|---|---|---|---|
| **macOS** (this Mac: macOS 27.0.1, APFS, macFUSE 5.4.0 kext loaded) | **Materialise plus watch.** APFS `clonefile` from a CAS on the same volume; FSEvents plus the index scan; git's built-in fsmonitor on the tree | **Sparse by path** in v1: dependency trees and large data are not placed until asked for (`weft drive fetch <path>`, or a rule file), and are listed in `.weft/sparse`. A File Provider replicated extension (dataless files that materialise on open) is the later route for cold data | **macFUSE:** T1 measured 10 to 25 times local for warm `git status`, and a kext needs reduced security on Apple Silicon. **FSKit** (macOS 15.4+, and macFUSE 5's `-obackend=fskit`): no readdirplus (one getattr per entry), no extended attributes, no byte-range locks, no per-file fsync callback, a reported 2 to 4 times the CPU of the kext, and a data-corruption mitigation as recently as the macFUSE 5.5.0 pre-release. Both stay on watch for a read-only "browse any head" view |
| **Linux** (the CI host, PG, the SC, the ARM VM) | **Materialise plus watch** for a working tree: reflink where the file system has it, otherwise a materialised read-only base plus an **unprivileged overlayfs** upper per branch (the mock's route). On overlay, the upper directory *is* the change set, so capture is a scan of the upper, with inotify for timing | **fanotify pre-content events** (`FAN_PRE_ACCESS`, Linux 6.14+, needs `CAP_SYS_ADMIN`) fill placeholder files on first read where the daemon has that capability (PG, the SC). Sparse by path elsewhere | **FUSE3** is allowed as a secondary route for read-mostly server views (many branches on one host): in-VM T1 was 0.11 s warm. Long attribute and entry caches, and FUSE passthrough (`FOPEN_PASSTHROUGH`, needs `CAP_SYS_ADMIN`) for data. Not used for agent working trees |
| **Windows** (lowest priority; not prototyped) | **Projected File System (ProjFS)**: NTFS placeholders hydrated on first read, the VFS for Git pattern, so `stat` is native NTFS | ProjFS placeholders are the cold path by construction | **WinFsp** is a FUSE-style user-mode file system and carries the same per-call cost question as macFUSE; fallback only. The Cloud Files API is the alternative if hydration progress and offline state are needed |

**Why materialise rather than mount.** The tools that matter (git, cargo, rust-analyzer, IDE indexers, node) are stat-heavy; a native tree gives them native speed, native write semantics and full offline use. What the drive gives up is instant presence of every byte of a huge tree: cold paths are sparse or placeholders. That matches the measurement that the store wins on adoption and warm joins, not on cold full-tree joins (mesh mock scenario 5; ADR-130 gate 9 cost control 4).

**On "no copy from remote to local".** The drive keeps the owner's rule where it matters: nothing is cloned or re-downloaded as a whole. A blob crosses the network once per device, only when a path that needs it is placed, and it is never copied again locally (clones, overlay lowers, placeholders). Adopting an existing checkout moves almost nothing (12 s, 18 KB in the mock).

### 3. The data path, end to end

```
 device                                   GCP                                    other devices
 ------                                   ---                                    -------------
 app writes file (native fsync)
 watcher -> index scan -> snapshot (clone)
 ring append (group commit)  -> local
 checkpoint: seal layer, device-signed
 PUT create-only  ------------------->  inbox bucket  --notify-->  SC (woken by waker if stopped)
   (URL from SC, or waker under A+)                                verify device registry
                                                                    flat (depth,name) merge
                                                                    conflicts/ records
                                                                    seal, repack, dual-sign
                                        seed bucket  <-- create-only objects, head CAS
                                        head finalize --Pub/Sub--> site relay (PG daemon) --mesh--> drive daemon
                                                                                                  verify head (pins, seq, chain)
                                                                                                  warm join (delta manifest)
                                                                                                  prefetch, clone into place
```

#### 3.1 Open and read

- A placed path is a native file. A sparse path is fetched on request; a placeholder (Linux fanotify, Windows ProjFS, later macOS File Provider) is filled on first access.
- **Fetch:** range-read the blob from its pack through a signed GET URL (kept alive, one round trip plus about 25 ms of service time: 58 ms from the CI host, 102 ms from the Mac, ADR-130 gate 3), check its SHAKE-256 against the manifest, write it to the CAS, clone it into place. Verified once, cached forever. Whole packs are fetched when two or more wanted blobs share one.
- **Prefetch on refresh:** the daemon fetches every blob a new head changes under the placed paths before applying it, so switching to a new head is a local operation.

#### 3.2 Write and capture

- Applications write the native file. Their `fsync` is the local file system's. Nothing on the write path waits on the network.
- The watcher marks paths. FSEvents coalesces and can drop events (`MustScanSubDirs`), so a dropped or overflowed event becomes a subtree rescan against the index, as git's fsmonitor does.
- **Snapshot before hash.** A changed file is first cloned to a private snapshot (APFS clone or reflink: atomic, no bytes moved), then hashed and appended from the snapshot, so a file being written while it is captured is never hashed half-written. Without reflinks the daemon copies, then re-checks `(size, mtime)` and retries if the file moved.
- Blobs under 256 KiB go to the ring (`RING_SMALL_MAX`); larger ones go into the CAS and upload loose; files over 64 MiB are split by content-defined chunking so an edit re-sends only changed chunks (the integrator's entry type already has a chunk list, `Attrs.chunks`).
- **Two durability levels, shown in `weft drive status`:** *local* (the file system has it, and the ring after its group commit) and *published* (in a signed head). A layer is never signed into a head before its objects are in GCS (ADR-130 §10).

#### 3.3 Seal and upload

- Checkpoints, not saves: by default a layer is sealed after 2 s of quiet on the tree, on `weft drive seal`, before a refresh that touches changed paths, and on shutdown. One layer per checkpoint; its parent is the head the tree is based on.
- The layer is signed with the **device key** (§5.1): Ed25519 + ML-DSA-65 from the start, so clients need no later cut-over.
- **Upload** goes to `incoming/<device>/<layer>/{pack,layer.json}` through create-only URLs (`x-goog-if-generation-match: 0`), size-bound with `x-goog-content-length-range` (the gate 5 hardening item).
- **Who mints the URLs (the A+ question).** Today only the SC mints them, so a device facing a stopped SC waits about 72 s. **Recommendation: A+ with a separate inbox bucket.** Each tenant gets an inbox bucket next to its seed bucket. The waker, after verifying a signed wake request from an enrolled device, mints create-only, size-bound, 15-minute PUT URLs under that device's own prefix in the **inbox bucket only**. Its service account holds `storage.objects.create` on the inbox bucket and nothing on the seed bucket: a compromised waker can add junk to an inbox (costing money, refused by the SC) but cannot read, overwrite or publish anything. The inbox notification wakes the SC as `incoming/` does today. If the owner declines A+, the drive still works: sealed layers wait in the local ring and upload when the SC is up (offline is a normal state, §3.7).

#### 3.4 Integrate, sign, notify

Unchanged from ADR-130: the SC verifies each layer against the device registry, merges in flat `(depth, name)` order with `git merge-tree`, records conflicts, seals the master layer, repacks, publishes create-only, and moves the project's head by generation compare-and-swap with a dual-signed head (chain = the project id). Measured: last upload to new head 1.1 to 1.4 s with the SC awake.

**Notification to devices.** Devices have no GCP identity, so they cannot hold a Pub/Sub subscription. Recommendation: **one subscriber per site**, the WeftOS daemon on PG (or the SC itself), rebroadcasts "head of project P moved to seq N" over the mesh (ADR-103). The relay is untrusted: a device verifies the head's dual signature, seq and chain as any reader does (ADR-130), so a lying relay can only delay. As a fallback each device polls the head through a read URL every 30 to 60 s (one GET, about 50 to 80 ms kept alive). Workload Identity Federation for heavy readers stays the ADR-130 target. The freeze gap ADR-130 left open (a replaying bucket hides new heads) is closed for drives by a staleness bound: a drive warns when its accepted head is older than the relay's last announced seq, or older than a bound when the SC is known to be up.

#### 3.5 Refresh

On a new head the daemon fetches the delta manifest (a warm join: 0.45 to 0.54 s, three to five GETs), prefetches blobs for placed paths, then for each changed path:

- **not changed locally:** replace it atomically (clone to a temporary name in the same directory, set mode and mtime, `rename`), so open editors see a normal external change;
- **changed locally, and the head carries this device's own merged layer for it:** clear it from the pending set;
- **changed locally and also in the head by someone else:** leave the local file alone and mark the path *diverged*; the next sealed layer carries it and the SC merges or records a conflict. A device never merges file contents itself (mock finding 5: a client rebase moves disjoint paths only).

#### 3.6 Conflicts: records, never silent drops

- The SC writes `conflicts/<layer>/record.json` (kind, device, both sides' hunks with markers, git's messages, the merged layers that touched the file) plus the refused layer and its pack, as in gate 5, and names the conflict in the next head's manifest under the author device.
- **What the user sees:** `weft drive status` lists it; the drive writes `.weft/conflicts/<id>/` with `ours`, `theirs`, `base` and `merged` (with markers) for each file; a desktop notification; and a board card or fix-up task for agent lanes (mock finding 8).
- **Partial layers.** The refused layer's clean paths are re-sealed by the daemon as a new layer, so one conflicting file does not hold back the rest (the mock lost clean edits to whole-layer exclusion).
- **Resolution** is an ordinary edit of the conflicted file plus `weft drive resolve <id>`, which seals a layer whose parent is the head that recorded the conflict.

#### 3.7 Offline

Placed paths read and write normally offline. Sealed layers queue in the ring and on disk; on reconnect they upload in order, and any that conflict are recorded as above. Sparse or placeholder paths that were never fetched fail with a clear error (`EIO` and a `.weft/offline` notice), never a hang.

#### 3.8 Crash safety

The working tree is the user's data on the local file system; the ring is a write-behind of sealed snapshots, with replay as measured (mesh mock crash tests: four kill points, every blob read back, no double index entries). After a daemon crash: replay the ring, then rescan the tree against the persisted index, so an edit made while the daemon was down is captured by the next scan. On the Mac, plain `fsync` does not flush the drive cache; the ring's group commit uses `F_FULLFSYNC` (`MESH_RING_FULLFSYNC` in the mock, not yet measured; D1 measures it).

### 4. Entry attributes (an ADR-128 format change)

| Attribute | Rule |
|---|---|
| **mtime** | Carried in nanoseconds in the manifest's attribute table, which is signed with the manifest but **kept out of the canonical bytes that name a layer**, so identical edits still produce identical layer names. On materialise the file gets the recorded mtime. Paths with none get a deterministic mtime from the layer's logical time, so outputs are newer than their inputs. Closes the mock's `target/` rebuild finding |
| **Mode** | Tracked files follow git: executable or not (`norm_mode`). Exact permission bits are carried for untracked and dependency layers. Owner and group are never carried; the local user owns every file |
| **Directories** | Explicit entries, so empty directories survive (git has none; a drive needs them) |
| **Symlinks** | Carried as the link text. On materialise, an absolute link or one that resolves outside the project root is refused for client projects and placed as a stub with a warning |
| **Hard links** | Not preserved: placed as separate files |
| **Extended attributes** | Not carried. `com.apple.quarantine` and similar are stripped on capture |
| **Names** | Stored as NFC. A tree with two paths equal under case folding is refused on a case-insensitive volume with a conflict record, never silently merged |
| **Large files** | Over 64 MiB: content-defined chunks in `Attrs.chunks`, range-fetched lazily |

### 5. Identity, separation and confidentiality

#### 5.1 Devices

- Each device generates its own dual key pair (`weftos-pq-sign`, WPQK v2) on the device; private halves stay in a 0600 file or the platform keychain.
- **Enrolment:** the device prints an enrolment request (public keys, device name, requested projects). The owner approves it with `weft drive enrol` on a trusted machine. Approval adds the device to a **signed device registry** object per tenant (`registry/devices`, dual-signed and sequenced like a head). The registry replaces the SC's `clients.json` allowlist and the waker's `waker_clients` Terraform variable, so enrolment needs no Terraform apply.
- **Revocation** is a new registry version. From then on the SC refuses that device's layers and the waker refuses its wake and URL requests. Layers it already published stay valid history.

#### 5.2 Separation

- **One chain per project.** The head's signed `chain` field (ADR-130) is the project id, so a head of one project can never be accepted as another's.
- **WeftOS-internal and public projects:** the existing seed bucket, a prefix per project (`p/<project-id>/head`, `.../manifest/`, `.../packs/`), one lease for the SC.
- **Client projects: one seed bucket and one inbox bucket per client**, with a prefix per project inside. The bucket is the IAM, encryption-key, deletion and billing boundary, so ending an engagement is one bucket pair. Project ids in object names are opaque, never client or project names.
- Client data on PG keeps the isolation rules: files under `/data1`, services as the client user.

#### 5.3 Confidentiality: encryption before upload

- **Per-project data key (DEK)**, 256-bit. Each blob record, layer document and manifest is sealed with AES-256-GCM-SIV on the device (or the importer) before it leaves. Records inside a pack are sealed one by one, so a range read decrypts on its own.
- **Object names are keyed:** a keyed SHAKE-256 of the plaintext hash under a per-project naming key, so the bucket sees neither plaintext hashes nor equality across projects. Deduplication works within a project only. Convergent encryption across projects is rejected: it lets anyone who can guess a file confirm it is present.
- **Verification is unchanged in kind:** a reader decrypts and checks the plaintext SHAKE-256 against the manifest. Heads stay plaintext and dual-signed but hold only opaque ids, a sequence and hashes. Paths live only inside encrypted manifests (ADR-128 §8).
- **Key custody:** each DEK is wrapped by a per-client key-encryption key in **Cloud KMS** (not enabled in the project yet, ADR-130 §8; enabling it is an owner step). The SC's service account gets `cryptoKeyDecrypter` on that client's key only. Devices receive the DEK at enrolment, wrapped to the device with hybrid X25519 + ML-KEM-768 (post-quantum against harvest-now-decrypt-later), so devices never call KMS. ML-KEM is a new dependency and goes through the diff gate.
- **The SC sees plaintext** while it merges. Its Local SSD is encrypted by Compute Engine and cannot take customer keys, so the SC wipes a client project's workspace on the idle exit, and the SC is a trusted node for each client it integrates. If the owner does not accept that for a client, that client integrates on PG only and never uses GCS (ADR-130 §9's second option).
- Bucket-level CMEK may be added as defence in depth; it does not replace client-side encryption.

### 6. Making the SC the primary file holder

**Target roles.** GCS holds the durable copy of every project. The SC is the only writer. PG becomes the standby writer (ADR-130 gate 7), a full verified replica of every head, the backup outside GCP, and optionally the L1 RustFS for dependency trees on the LAN.

#### 6.1 Import (per project; contents are read only by the importer process)

The importer runs on PG **as the user who owns the project directory** (the client user for `/data1/weavelogic-clients/*`, the `weftos` user for `/data1/weavelogic-internal/*`):

1. **Git projects:** read from the git object store, not the working tree: `git ls-tree -r -z HEAD` plus one `git cat-file --batch` stream, hashed, encrypted (client projects), packed. The seed manifest's git tree hash must equal `HEAD^{tree}`, the check gates 3 and 5 used.
2. **Working-tree extras and non-git directories:** a filesystem walk with an explicit include list (untracked data, models, local config the project needs). A deny list and a secret scan keep `.env` files, keys and credentials out. Dependency and build trees are left out (rebuilt, or on L1). The walk's tree hash, computed on PG, must equal the tree hash a reader computes from the published manifest.
3. **History:** git history is not converted; it stays in git. WeftOS keeps GitHub. A client project's history is uploaded as an encrypted `git bundle` in the client's bucket for the SC's integrator, or stays on PG; the owner chooses per project.
4. The importer uploads its layers through create-only URLs like any device; the SC verifies the tree hash and publishes **seq 1** of the project chain.

#### 6.2 Cutover, per project, never all at once

| Phase | What happens | Exit |
|---|---|---|
| 0. Shadow | Import; PG stays authoritative. A PG drive client re-imports changes from the PG path every few minutes, one way, and compares tree hashes | Two weeks with every comparison equal |
| 1. Freeze and flip | Writes to the PG path stop (a read-only bind mount for that project, applied through Terraform in an owner-approved window); final import; tree hash verified; the SC head becomes the source of truth | Tree hash of the head equals the PG path |
| 2. PG as a client | The PG drive daemon materialises the head **into the same path**, so PG services keep reading the path they read today. A service that writes there is captured like any device and its writes become layers | Services pass their own checks for a week |
| 3. Steady state | PG holds the standby lease role and mirrors every head and every object to its own disk (a verified pull of the bucket: the backup outside GCP) | |

**Existing PG services.** Readers see no path change. Writers move from "writes are the truth" to "writes are published within a checkpoint plus an integrate" (seconds with the SC awake, about 72 s to wake). A service that needs cross-machine read-after-write faster than that is listed in the import plan and stays PG-primary until it is changed.

**Rollback** at any phase: stop the PG drive client, lift the read-only bind, and the PG path is authoritative again. Because PG materialises every head into that path, nothing written through the SC is lost; the path is checked against the last head by tree hash. The GCS chain stays as history.

**Order:** WeftOS itself (public, already in the bucket), then other internal projects, then one client project after the owner decides §5.3 for that client.

## Alternatives considered

| Alternative | Verdict |
|---|---|
| **FUSE everywhere** (macFUSE, FUSE3, WinFsp) with metadata from RVF layers | Rejected for working trees: T1 showed the Mac's per-stat round trip, not metadata latency, is the cost. Kept for Linux server views |
| **FSKit** | Not yet: missing readdirplus, xattrs, locks and an fsync callback; higher CPU; young. Re-evaluate each macOS release |
| **macOS File Provider for the whole tree** | Rejected as the primary: it puts the tree under `~/Library/CloudStorage`, needs a signed app extension, and `fileproviderd` has public reports of stalling on large trees. Kept for cold data later |
| **NFS from a local VM** (the T1 route) | Rejected: warm Linux-tree `git status` 26.5 s over NFS on the Mac |
| **rclone mount** | Rejected for working trees: no locking, last close wins, per-client caches (rclone research). Kept for read-only browsing |
| **JuiceFS** | Rejected: it needs its own metadata engine, whose round trip is what ADR-128 replaced with local published metadata |
| **Encrypt on the writer only** (ADR-130 §9 as written) | Insufficient: devices upload layers straight to GCS, so they must encrypt before upload too |

## Gates for the prototype

Run on the existing SC and bucket, a test chain (`drive-test`) and, for D4, a throwaway test tenant bucket pair. Nothing touches `head/master` or `/data1`.

| # | Gate | Pass when | Measure |
|---|---|---|---|
| **D0** | Entry attributes v2 (§4) in the layer and manifest format | Property tests against a reference model (create, write, rename, chmod, symlink, empty directory, delete) round-trip; a restored `target/` layer gives `cargo check` with **0 units rebuilt** | rebuilt units, format size |
| **D1 (first)** | **Mac drive for WeftOS** from the seed head: materialise by clone, FSEvents capture, seal, ring | Warm `git status` **at most 2× local** (local 0.09 to 0.22 s; macFUSE was 2.1 to 2.5 s); cold base materialise at most 15 s (gate 3: 14.0 s); refresh to a new head at most 1 s; edit to sealed layer at most 3 s after quiet; `kill -9` of the daemon at four points loses no edit (the rescan finds it); `F_FULLFSYNC` ring ack measured | `git status`, `cargo check`, rust-analyzer index time on the drive against a plain checkout |
| D2 | Linux drive on the CI host (overlayfs) and the round trip | An edit on the Mac is placed on the CI host within **5 s** with the SC awake and **90 s** with it asleep; under A+ the device's upload never waits for the SC | each hop from server timestamps |
| D3 | Conflicts | Same line edited on two devices: one merges, the other gets a record in `.weft/conflicts` within one head cycle; its clean paths are re-sealed and merge; `weft drive resolve` publishes | time to surface; lost edits (must be 0) |
| D4 | Tenant separation and encryption | Test tenant: objects reveal no plaintext hash or path; a device without the DEK cannot read; a revoked device's layer is refused by the SC and its wake by the waker; the gate 8 tamper suite passes on the encrypted chain | encryption overhead on seal and join |
| D5 | Migration drill | Import of a copy of an internal project (git import plus walk) into the test chain: tree hash equal; a PG drive client materialises it into a scratch path; a rollback drill restores the scratch path by tree hash | import time, bytes, objects |
| D6 | Lazy cold data on Linux | fanotify pre-content fills placeholders on PG or the SC (kernel 6.14+ checked first) with `git status` unaffected | first-read latency |

**Kill criteria.** D1 failing on `git status` (over 2× local) or on any lost edit is a design failure. D4 leaking a path or a plaintext hash is a design failure.

**Cost.** The prototype runs on the Spot SC at $0.167 an hour: about 20 SC hours is about $3.40. A test tenant bucket pair, a KMS key ($0.06 per key version a month) and its operations, and Pub/Sub stay under $1. Egress stays in the free tier (base joins are about 107 MB). **About $5 in all.** Production adds, per client, two buckets (cents at small sizes), one KMS key, and SC hours (ADR-130's duty-cycle table).

## Consequences

- Working trees run at native speed on every platform, offline included; the price is that cold data is sparse or placeholder-backed and is fetched on first use.
- The drive daemon becomes a correctness-critical component (capture, index, refresh), gated by D0, D1 and D3.
- PG stops being the store of record; it keeps the standby, replica and backup roles, and its HDD leaves the publish path.
- Client confidentiality depends on client-side encryption and on trusting the SC per client.
- The SC's uptime becomes user-visible latency for publishing (not for local work); A+ removes it from the upload path.

## Open questions for the owner

1. **A+:** may the waker mint create-only, size-bound PUT URLs into a separate inbox bucket (no access to the seed bucket)?
2. **Client data in GCS:** for each client, encrypt and publish (with the SC trusted to see plaintext while merging), or PG-only? And enable Cloud KMS for key custody?
3. **Bucket per client** (recommended) or a prefix per client in one bucket?
4. **First project** to cut over after WeftOS, and the first client project.
5. **macOS cold data:** accept sparse-by-path in v1 and build a File Provider extension later (needs an Apple Developer ID and a signed app extension)?
6. **Windows:** in scope now, or after macOS and Linux?
7. **Git history for client projects:** an encrypted bundle in the client bucket, or kept on PG?
8. **Head notifications:** a relay over the mesh from one site subscriber (recommended), or WIF identities per node?
9. **SC wake latency:** add a weekday pre-warm schedule so drive users rarely wait 72 s?
