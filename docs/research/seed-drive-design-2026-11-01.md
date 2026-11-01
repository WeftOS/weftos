# Seed drive: design research (2026-11-01)

Supporting research for ADR-131 (the seed drive, and the seed cone as the primary file holder). Design only: nothing was built, deployed or written to any bucket, server or the CI host's export during this work.

Evidence labels:

- **Verified (measured)**: a number recorded in an earlier spike in this repository, cited by section.
- **Verified (source)**: read in code this session, cited by file and line.
- **Verified (docs)**: read in platform documentation or release notes this session, cited by URL.
- **Inferred**: follows from the above; not observed.

Code paths in the mock crate are relative to `weftos-mesh-mock/` on the research volume (outside this repository), as in ADR-128 and ADR-130.

## 1. The constraint that decides the Mac design

| Measurement | Mac local disk | Mac over NFS | Mac native macFUSE | Linux VM native FUSE | Label |
|---|---|---|---|---|---|
| `git status` WeftOS, warm | 0.09 to 0.22 s | 0.28 to 0.34 s | 2.1 to 2.5 s | 0.11 s | Verified (measured): `juicefs-spike-2026-10-29.md`, "Mac native (macFUSE)" |
| `git status` Linux kernel tree, warm | 0.31 s | 26.5 s | 31 to 34 s | 0.73 s | same |
| First `git status` in a fresh `juicefs clone` | | | 473 s | 1.1 s | same |
| Create 10k small files | 8.1 s | 66 s | 63 s | | same |

The metadata round trip in that run was 0.16 ms, so the macFUSE number is the kernel-to-daemon round trip per `lstat`, not the store (verified, same section). **Inferred:** any design that puts a user-space round trip under each `stat` on the Mac lands in the 2 s band for WeftOS and the 30 s band for a Linux-sized tree, whatever the store behind it. The JuiceFS spike lists untried levers (macFUSE attribute-cache options, `fsmonitor`, `core.untrackedCache`); they could narrow the gap for `git status` but not for cargo's or an IDE indexer's stats, which do not go through git's caches (inferred).

## 2. Platform options, with sources

### 2.1 macOS

| Option | Facts | Label |
|---|---|---|
| macFUSE kext | Loaded on this Mac: `io.macfuse.filesystems.macfuse.25 (5.4.0)`, macOS 27.0.1 (`sw_vers`, `kextstat` this session). Third-party kexts on Apple Silicon need the reduced-security boot policy | Verified (this Mac); policy: recalled |
| macFUSE FSKit backend | Selected with `-obackend=fskit`, libfuse3 only; the kext stays the default "until the FSKit API is on par with the VFS kernel API". 5.5.0 pre-release: a data-corruption issue on macOS 26 and 27 mitigated (empty `FUSE_READ` replies rejected, buffer tails cleared); FSKit has no per-file fsync callback; on macOS 27 the close callback cannot return errors | Verified (docs): [macFUSE 5.5.0 release](https://newreleases.io/project/github/macfuse/macfuse/release/macfuse-5.5.0), [macFUSE evolving beyond kexts](https://forums.macrumors.com/threads/macfuse-veracrypt.2485302/) |
| FSKit (Apple) | Minimum macOS 15.4; an app extension subclassing `FSUnaryFileSystem`, enabled by the user in System Settings | Verified (docs): [FSKit](https://developer.apple.com/documentation/fskit), [FSUnaryFileSystem](https://developer.apple.com/documentation/fskit/fsunaryfilesystem) |
| FSKit gaps reported by developers | No readdirplus (separate getattr per entry); no extended attributes; no POSIX byte-range locks; no inode eviction notice; one developer measured 100 to 150% CPU against 40% for the same macFUSE function and shelved the port | Verified (forum reports, not Apple statements): [Apple developer forum thread 831338](https://origin-devforums.apple.com/forums/thread/831338), [jfuse issue 145](https://github.com/cryptomator/jfuse/issues/145); early FSKit lacked network file systems: [thread 776322](https://origin-devforums.apple.com/forums/thread/776322) |
| File Provider (replicated extension) | Items are dataless (metadata only) or materialised; opening a dataless file calls `fetchContents`; the system may dematerialise to free space | Verified (docs): [Replicated file provider extension](https://developer.apple.com/documentation/fileprovider/replicated-file-provider-extension), [Synchronizing the file provider extension](https://developer.apple.com/documentation/fileprovider/synchronizing-the-file-provider-extension) |
| File Provider at scale | A developer report of `fileproviderd` stalling in sqlite work on a large tree (Sonoma); a Nextcloud PR measured 30,117 dataless transitions over 630 directories in seven minutes | Verified (reports, possibly dated): [developer forum profile](https://origin-devforums.apple.com/forums/profile/rjkroege), [nextcloud/desktop#10728](https://github.com/nextcloud/desktop/pull/10728) |
| APFS clones | `clonefile` places a file sharing the source's blocks; the mock uses `reflink_copy::reflink` on macOS (`src/client.rs:172-189`, `cow_file`) and to ingest into the CAS without moving bytes (`src/client.rs:191-209`) | Verified (source) |

**Choice: materialise plus watch, sparse cold paths (ADR-131 §2).** The tree is native APFS, so every measurement in the "Mac local disk" column applies (inferred from the T1 table plus the mock's adopt result). File Provider is the cold-data route for later, not the primary, because it fixes the location under `~/Library/CloudStorage` and needs an app extension (verified, docs), and because the scale reports above are about exactly the tree sizes we have (inferred).

### 2.2 Linux

| Option | Facts | Label |
|---|---|---|
| Unprivileged overlayfs | Used by the mock on the CI host (no reflinks, no root): per-layer lower directories, one upper, a private mount namespace, whiteouts as character devices; a hard-link farm broke at the ext4 65,000-link limit | Verified (measured): `adr128-mesh-mock-2026-10-30.md`, "What differed" and "What this changes" item 2 |
| Overlay branch cost | Under 1 s to mount, 0.24 MB of upper per cold-check job | Verified (measured): mock scenario 4 |
| FUSE in a VM | Warm `git status` 0.11 s on WeftOS; 0.73 s on the Linux tree | Verified (measured): JuiceFS spike |
| FUSE passthrough | `FOPEN_PASSTHROUGH` with a backing file id registered by `FUSE_DEV_IOC_BACKING_OPEN`; read, write, splice and mmap go to the backing file; needs `CAP_SYS_ADMIN`. It does not remove the getattr round trip | Verified (docs): [kernel FUSE passthrough](https://docs.kernel.org/filesystems/fuse/fuse-passthrough.html); merge version: [Phoronix](https://Phoronix.com/news/FUSE-Passthrough-In-6.9-Next) (pre-merge article; the exact release is not confirmed here) |
| fanotify pre-content | `FAN_PRE_ACCESS` since Linux 6.14, delivered to `FAN_CLASS_PRE_CONTENT` groups with a file-range record, so a daemon can fill content before a read; mmap hooks added in 6.14-rc7 | Verified (docs): [LWN 1015987](https://lwn.net/Articles/1015987), [LTP fanotify24 patch](https://lists.linux.it/pipermail/ltp/2025-March/042703.html) |

**Inferred:** fanotify pre-content is the Linux equivalent of dataless files, with native `stat` (the placeholder is a real file of the right size) and a fill only on read; it needs `CAP_SYS_ADMIN`, so it fits PG and the SC (system daemons) and not an unprivileged lane. The kernels of PG, the CI host and the SC were not checked in this design work (D6 checks first).

### 2.3 Windows

| Option | Facts | Label |
|---|---|---|
| ProjFS | Placeholders are NTFS files with a reparse point and no contents; reading hydrates them through the provider; VFS for Git stores the blob id as the placeholder's `contentId` | Verified (docs): [GVFS architecture](https://learn.microsoft.com/en-us/previous-versions/azure/devops/all/git/gvfs-architecture?view=tfs-2017), [ProjFS](https://learn.microsoft.com/ko-kr/windows/desktop/projfs), [VFSForGit FAQ](https://github.com/microsoft/VFSForGit/blob/3790eb06e14d88d71af11958414b59fd3cb2e65d/docs/faq.md) |
| ProjFS limits | No explicit hydrate call and no progress or offline indication; the Cloud Files API has both (community answer) | Verified (community, not official): [Microsoft Q&A](https://learn.microsoft.com/en-us/answers/questions/5981350/how-to-hydrate-a-projected-file-system-file-projfs) |
| WinFsp | Not researched in this session | Not established |

## 3. What the mock and the SC already provide

| Piece the drive needs | Exists today | Label |
|---|---|---|
| Entry type | `Ent { mode, size, hash, link }`, directories implied by paths, no mtime (`src/common.rs:15-25`); modes normalised to 0644/0755 (`src/client.rs:163-165`) | Verified (source) |
| Layer name covers entry fields | `canonical()` hashes parents, meta and each op's path, mode, size, hash and link (`src/common.rs:129-159`); `layer_name()` (`:161`). **Consequence:** an mtime added to `Ent` would change layer names, so ADR-131 §4 keeps mtime in a manifest attribute table outside these bytes | Verified (source); consequence inferred |
| Chunk list in the integrator's entry | `attrs_of` builds `Attrs` with `chunks` holding one 32-byte hash (`src/sc.rs:253-264`), so multi-chunk files fit the integrator's type | Verified (source) |
| Change index | `(size, mtime)` skip of re-hashing in `ent_of` and `scan` (`src/client.rs:239-268`); `mtime_ns` (`:167-169`) | Verified (source) |
| Adoption | `adopt` reuses every file whose hash the store already has (`src/client.rs:643`); measured 12 s, 18 KB on the Mac | Verified (source and measured) |
| Placing files from the CAS | `place` creates directories, symlinks or a CoW clone (`src/gcsflow.rs:343-357`) | Verified (source) |
| Tree-hash verification | `git_tree` and `treehash` compute a git tree hash from a materialised tree (`src/gcsflow.rs:453`, `:500`); used to prove the Mac, the CI host and the SC hold `d070bb36…` and `fc52a711…` | Verified (source, measured in ADR-130) |
| Ring | 256 KiB small-blob cut-off (`src/ring.rs:39`), `F_FULLFSYNC` on macOS when asked (`src/ring.rs:131-143`), crash replay (`:737`) | Verified (source); the `F_FULLFSYNC` cost is unmeasured |
| Dependency-tree skip | `git_ignored` (`src/common.rs:215-217`) drives the default join skip | Verified (source) |
| Create-only upload URLs | V4 URLs signing `x-goog-if-generation-match: 0` (`src/gcs.rs:334-356`), minted per layer under `incoming/<client>/<layer>/` (`src/sc.rs:503-516`); no size bound yet | Verified (source) |
| Device allowlist | A JSON map read by the SC (`src/sc.rs:628`, refusal at `:689`); the waker's own map comes from Terraform (ADR-130) | Verified (source) |
| Conflict records | `conflicts/<id>/{record.json, layer.json, pack}` (`src/sc.rs:779-782`) | Verified (source) |
| Chain binding | `set_chain` and `chain()`, default `master` (`src/head.rs:28-35`); the v2 head signs the chain | Verified (source) |
| Wake | Signed wake request with 120 s skew (`src/wake.rs:15-17`, `:38`) | Verified (source) |
| Lease | `lease/writer` with generation CAS (`src/lease.rs:14`, `:31`, `:69`) | Verified (source) |
| Dual signatures | `DualSignature::sign` and `verify` (`crates/weftos-pq-sign/src/dual.rs:134-197`), key files (`keyfile.rs:43`, `:105`) | Verified (source) |
| File watching in WeftOS | `notify` 8 is a workspace dependency (`Cargo.toml:290`); used in `crates/clawft-core/src/agent/skill_watcher.rs:26` | Verified (source) |
| FUSE, FSKit, ProjFS or fanotify code in WeftOS | None: a search of `crates/` for `fuser`, `fuse3`, `macfuse`, `fskit`, `winfsp`, `fanotify` and `FileProvider` found nothing | Verified (search) |
| RVF CoW memory | `clawft-cow-memory` (branch, checkpoint, promote) for agent memory; not a file-tree store | Verified (source, `crates/clawft-cow-memory/src/lib.rs:1-40`) |

## 4. Latency budget of the end-to-end path

All component figures are verified measurements from ADR-130 unless marked; the sums are inferred.

| Step | SC awake | SC stopped |
|---|---|---|
| Quiet before seal (ADR-131 default) | 2.0 s (design value) | 2.0 s |
| Seal and hash a small layer | under 0.1 s (mock: hashing and sealing 1.6 to 1.8 s for a 525 MB tree) | same |
| Upload 2 objects create-only | 0.3 to 0.4 s (gate 5) | without A+: wait for the SC, about 72 s; with A+: 0.3 to 0.4 s plus 2 s for a cold waker |
| Incoming notification to head published | 1.4 s (production build: 0.79 s pull plus 0.62 s integrate) | 72 s wake (production build) |
| Head finalize to message published | 0.07 s (Pub/Sub latency section) | same |
| Relay over the mesh | not measured; one tailnet hop is 26 to 39 ms (gate 2) | same |
| Warm join and place | 0.45 to 0.54 s (gates 3, 5) | same |
| **Total, edit to placed on another device** | **about 4.5 s** (inferred) | **about 77 s** (inferred) |

D2's bars (5 s and 90 s) are set from this table.

## 5. Encryption design notes

- **Why per-record sealing inside packs.** Readers range-read single blobs out of 8 MiB packs (gate 3: "range when fewer than two wanted blobs share a pack"). Sealing each record with its own nonce keeps range reads working; a whole-pack AEAD would force whole-pack reads (inferred).
- **Why AES-256-GCM-SIV.** The nonce can be derived from the keyed content hash without catastrophic failure on reuse, which makes re-uploads of the same blob byte-identical, so create-only PUTs stay idempotent (a 412 still means "already there"). Inferred from the properties of nonce-misuse-resistant AEADs; recalled, not re-read this session.
- **Why keyed names.** With plain SHAKE-256 names a bucket reader can test whether a known file (a public library, a leaked document) is in a client project. A per-project keyed name removes that, at the cost of cross-project deduplication, which client isolation forbids anyway (inferred).
- **ML-KEM.** Not present in the workspace (no `ml-kem` dependency found in the search above; inferred from the absence). Adding it is a dependency change and goes through the diff gate with owner approval.
- **KMS.** Not enabled in the project (ADR-130 §8). KMS holds symmetric key-encryption keys, which is all this design asks of it; it does not need to sign ML-DSA (ADR-130 §8).
- **The SC's Local SSD** cannot use customer-managed keys (ADR-130 §10, Local SSD table), so plaintext of a client project exists on the SC while it integrates. ADR-131 makes that an explicit per-client trust decision.

## 6. Migration notes

- **Import by git object store, not working tree,** because the git tree hash is the verification the spikes already trust (gate 3, gate 5: `d070bb36…`, `fc52a711…`), and because reading `.git` objects does not depend on the working tree being clean. Gate 5 also showed that one batched process per operation matters: per-blob `git hash-object` took 25.8 s, batched 1.97 s (ADR-130 gate 5, verified); the importer uses one `git cat-file --batch` stream for the same reason (inferred).
- **PG paths.** `/data1/weavelogic-internal/weftos` is the live install root, not a source tree (mock, "What differed"); the import plan has to classify each directory under `/data1/weavelogic-internal/*` and `/data1/weavelogic-clients/*` as git source, data, runtime state or service install before importing. This design did not list or read those directories.
- **Rollback costs nothing extra** only while PG materialises every head into the original path (phase 2); that is why phase 2 comes before PG gives up the path for good (inferred).

## 7. Not established

- WinFsp behaviour and cost.
- The real cost of git's built-in fsmonitor plus the drive's own FSEvents stream on one tree (two watchers).
- `F_FULLFSYNC` latency for the ring on this Mac.
- Kernel versions on PG, the CI host and the SC for fanotify pre-content.
- Whether a File Provider domain can hold a git working tree with acceptable `git status` time.
