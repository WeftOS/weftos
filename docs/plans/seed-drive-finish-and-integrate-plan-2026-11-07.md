# Seed store and seed drive: status, Tapestry needs, gaps, and a finish-and-integrate plan (2026-11-07)

Status: plan, not committed. Nothing was built, deployed or changed in GCP, on PG or on the CI host for this document. Written from the ADRs, spike records and research below; the live SC and bucket state could not be re-read today (`gcloud` wants a fresh login, see §7).

Labels used throughout: **proven** (measured and recorded), **partial** (built or measured in part), **not done** (design only), **never tested** (built or planned, no run recorded).

Sources: ADR-128 (`docs/adr/adr-128-rvf-layered-trees.md`), ADR-130 (`docs/adr/adr-130-gcs-seed-store.md`), ADR-131 (`docs/adr/adr-131-seed-drive.md`), `docs/research/seed-drive-design-2026-11-01.md`, `docs/research/adr128-mesh-mock-2026-10-30.md`, `docs/grok-build-fork/rustfs-computer-spike-plan-2026-11-02.md`, `docs/grok-build-fork/spike-results/{host-survey,gates-20261104,rustfs-macos-wsl}.md`, `docs/grok-build-fork/plan.md`, `docs/handoff-tapestry-phase1.md`, and the gate logs in `/Volumes/ai-models/research/tapestry-computer-spike/gate-results/`.

## 1. Where the three ADRs stand

| ADR | Status line | Reality | Label |
|---|---|---|---|
| ADR-128 RVF layered trees | Proposed | T0, T2, T3, T4, T5 pass; T1 passes for direction only (in-VM), fails for a Mac-side mount; T6 not run. G1 ("adopt the direction": T0, T1, T2, T5) is met on the numbers but never declared. **G2 (mount route)** is answered on paper by ADR-131 with a third option the gate did not list: a materialised native tree plus a watcher, FUSE only for Linux server views. B1 to B6 are not started | partial |
| ADR-130 GCS seed store | Accepted 2026-11-01 | The production build is live: Spot SC, writer unit, dual-signed heads, Pub/Sub trigger, idle-stop, waker, budget. Gates 1 to 9 all recorded | proven |
| ADR-131 seed drive | Proposed 2026-11-01, "nothing built" | Still true. D0 to D6 not run. The nine owner questions are unanswered | not done |

**ADR drift to reconcile** (findings, not edits; this document does not touch the ADRs):

- ADR-130 "Production build, cost controls" still lists the budget alert as **Pending**; the Terraform worktree has it applied (`gcp/seed-store` commit `6c7d73d`, and the project memory records it live on 2026-11-01). ADR-130 needs an Updated line.
- ADR-128 G2 lists two options (JuiceFS with an RVF metadata adapter, or a WeftOS FUSE filesystem). ADR-131 chose neither for clients. ADR-128 carries an Updated pointer but G2 itself still reads as open; it should say "answered by ADR-131 for client nodes, pending acceptance".

## 2. The seed store (ADR-130): what is proven

Everything in this table has a measurement in ADR-130's spike sections.

| Area | Result | Where | Label |
|---|---|---|---|
| Stateless writer on Titanium Local SSD | fsync 0.14 ms p50 at 4 KB (PG HDD 26 to 33 ms); RustFS-style objects 1,095 PUT/s at 8 writers (PG 48) | gates 1, 1b | proven |
| Publish and compare-and-swap head | 2.25 GB, 692 PUTs, 104 s (source-bound); CAS 60 to 100 ms; stale generation refused with 412 | gate 3 | proven |
| Cold and warm joins | CI host cold full tree 22.5 s (RustFS 90 s); Mac base-only 14.0 s (RustFS 18.8, git clone 9); warm 0.45 to 0.54 s, 3 to 5 GETs | gates 3, 5 | proven |
| SC owns the write path | ingest via create-only URLs, flat `(depth, name)` merge in `git merge-tree` 26 ms for 6 layers (plain git 372 ms), seal, sign, publish; last upload to new head 1.1 s | gate 5 | proven |
| Conflicts are records | `conflicts/<layer>/record.json` plus the refused layer and pack; the same layer refused as plain git | gate 5 | proven |
| Signed head, rollback and fork refused | 15 cases, state file untouched on refusal | "Pub/Sub, signed head" | proven |
| Failover to PG and back | PG took the lease 30.7 s after the last renewal, published 43.9 s after; SC rehydrated from blank in 3.3 s; no two writers | gate 7 | proven |
| ML-DSA-65 dual signing | head v2 and `.sig2`; readers refuse downgrades; cutover at seq 13 | "ML-DSA-65 dual signing" | proven |
| Tamper suite | 26 cases, Mac and CI host, state unchanged on every refusal | gate 8 | proven |
| Cost | spike about $11.65; Spot SC $0.167 per hour running, about $8.40 a month fixed; 8 h weekdays about $38 | gate 9, "Cost re-estimate" | proven |
| Production wake path | Pub/Sub integrate 1.41 s with the SC awake; `/wake` to head 72.2 s; preemption recovery 71.6 s; idle-stop 51 s after release | "Verification (2026-11-01)" | proven |
| Pub/Sub delivery to a device | publish 71 ms; Mac unary pull 0.69 s (upper bound, Python loop over Wi-Fi) | "Pub/Sub latency" | partial: never measured on the CI host, streaming pull not tried |
| Reader access without GCP identity | SC-minted V4 GET URLs (688 in 1.4 s); Mac used the owner's ADC as an interim | gate 3 "Reader access" | partial: WIF never built |

**Still open in ADR-130 itself** ("Still open" and "Consequences"): waker image pinned by tag not digest; no automatic fallback when the zone has no Spot capacity; A+ deferred; reader auth beyond signed URLs; KMS custody; per-client encryption; garbage collection of unreachable objects; Bucket Lock; the mkfs of the Local SSD (about 20 s) and the tailnet join sit on the 72 s wake path.

**Last known live state** (ADR-130, 2026-11-01): head `master` at **seq 16**, dual-signed, SC Spot VM `weftos-seed-cone`, tailnet name `weftos-seed-cone-1`. Not re-verified today.

## 3. The seed drive (ADR-131): design, phases, G2

### 3.1 The design in one paragraph

A project is a real directory on the local disk. A per-user drive daemon materialises files from a local content-addressed cache by copy-on-write clone (APFS `clonefile`, Linux reflink or an unprivileged overlayfs upper), watches for edits (FSEvents, inotify or fanotify), seals a device-signed layer after 2 s of quiet, uploads it create-only to an inbox, and refreshes placed paths when a new dual-signed head arrives. Cold data is sparse by path (v1) or placeholder-backed (fanotify pre-content on Linux 6.14+, ProjFS on Windows, File Provider on macOS later). The SC merges, records conflicts, and publishes. PG becomes standby, replica and backup (ADR-131 §1 to §3, §6).

### 3.2 The G2 mount choice

ADR-128 G2 asked for a mount route from T1 and T2. The T1 numbers decided it: warm `git status` on WeftOS is 0.09 to 0.22 s on local disk, 0.11 s on FUSE inside a Linux VM, and 2.1 to 2.5 s on native macFUSE (the Linux kernel tree: 0.31 s against 31 to 34 s), with a 0.16 ms metadata round trip, so the cost is the per-`lstat` kernel-to-daemon hop, not the store (ADR-131 "Context"; `seed-drive-design` §1). FSKit lacks readdirplus, xattrs, locks and an fsync callback and shows 2 to 4 times the CPU (`seed-drive-design` §2.1). So:

- **Clients (Mac, Linux lanes, Windows):** materialise plus watch. Not a mount. Label: **not done** (design).
- **Linux server views (many branches on one host, read-mostly):** FUSE3 with long caches and passthrough is allowed. Label: **not done**; in-VM numbers are **proven**.
- **Rejected:** FUSE everywhere, FSKit now, File Provider as primary, NFS from a VM (26.5 s), rclone mount, JuiceFS (needs its own metadata engine) (ADR-131 "Alternatives considered").

What G2 still needs from the owner: acceptance of ADR-131, which turns G2 from "answered" into "decided".

### 3.3 The phases (ADR-131 "Gates for the prototype" and §6.2)

| Gate | What | Depends on | Runs where | Label |
|---|---|---|---|---|
| D0 | Entry attributes v2: mtime outside the layer name, explicit directories, modes, symlink rules, chunk lists; property tests; a restored `target/` gives `cargo check` with 0 units rebuilt | nothing | any host (pure code) | not done |
| D1 (first, kill criterion) | Mac drive for WeftOS from the seed head: clone, FSEvents, seal, ring; warm `git status` at most 2× local; cold materialise at most 15 s; refresh at most 1 s; four `kill -9` points lose nothing; `F_FULLFSYNC` measured | D0 | the Mac, serial | not done |
| D2 | Linux drive on the CI host (overlayfs) and the Mac-to-CI round trip: 5 s with the SC awake, 90 s asleep | D1's daemon | CI host (aepod-xpc) plus the Mac | not done |
| D3 | Conflicts across two devices: record within one head cycle, clean paths re-sealed, `weft drive resolve` publishes | D2 | Mac plus CI host | not done |
| D4 | Tenant separation and encryption: keyed names, per-project DEK, revoked device refused, gate 8 suite on an encrypted chain | owner questions 2 and 3, KMS enabled, ML-KEM through the diff gate | SC plus a test tenant bucket pair | not done |
| D5 | Migration drill: import a copy of an internal project into the test chain, PG drive client materialises into a scratch path, rollback by tree hash | D0 to D2, PG access as the owning user | PG | not done |
| D6 | Lazy cold data on Linux via fanotify pre-content (kernel 6.14+) | kernel check | PG (7.0) or the CI host (6.18); the SC's kernel is unchecked | never tested |
| Cutover 0 to 3 | shadow two weeks, freeze and flip, PG as a client, steady state; per project, WeftOS first | D5 and owner windows | PG | not done |

Pieces that already exist and that the drive reuses (`seed-drive-design` §3, all **proven** in the mock or ADR-130): CoW place from a CAS, `(size, mtime)` change index, adoption (12 s, 18 KB), tree-hash verification, the ring with `F_FULLFSYNC` support (cost unmeasured), the dependency-tree skip, create-only upload URLs (no size bound yet), the client allowlist, conflict records, chain binding in head v2, the lease, the wake request, dual signatures.

Pieces that do not exist anywhere (`seed-drive-design` §3 and §7): a file watcher for trees (WeftOS has `notify` 8 for skill files only), any FUSE, FSKit, ProjFS or fanotify code, a device registry, per-project chains or leases on the SC, client-side encryption, `weft drive` commands, a head relay over the mesh.

## 4. What Tapestry specifically needs

Tapestry's own plan puts the store on its critical path only at **Phase 3** (`docs/grok-build-fork/plan.md` §4, "Phase 3" and "WeftOS-side work this depends on"): a "computer" workload kind, a scoped-view interface on our store for the bwrap/Landlock sandbox, and a hub server. Phases 1 and 2 need nothing from the seed store. Phase 1 today: gates (f) and (h) pass; (a), (c), (d), (e), (g) open (`docs/handoff-tapestry-phase1.md`; fork branch `tapestry/phase1-r17` pushed at `38015966`, with `76dd3728` on top locally).

The owner's target design for a computer (`docs/grok-build-fork/decisions-2026-11-02.md`, Q6): a folder holding a root image plus a working filesystem on a seed node, spun up locally under bwrap/Landlock on Linux and WSL or Seatbelt on macOS, network-transparent, with Debian 13 trixie as the base image to match xAI's sandbox.

The computer spike (`tapestry-computer-spike`, 2,269 lines, its own git repo) proved most of it **against a throwaway RustFS or a `file:` store, never against GCS or the SC** (by design: `rustfs-computer-spike-plan` §3 "Out of scope"):

| Gate | Result | Label |
|---|---|---|
| C1 image round trip | rootfs seals, publishes, materialises with an equal tree hash on PG, the ARM VM, the Mac and cross-host (PG publishes, `weftos-ci` boots Debian 13 in bwrap: cold 118 s over the tailnet at about 27 MB/s, warm 12.3 s with 0 GETs) | proven |
| C2 isolation | Mac Seatbelt: network denied, write outside `/work` denied (4 checks). **Native bwrap route: the C2 checks were skipped in every recorded run** ("needs Linux + --rootfs"); the Landlock launcher is not in the gate log | partial; native **never tested** |
| C3 moves between hosts | Live path (writer plus read-only follower) 0.5 to 0.6 s per sealed edit cross-host. **Handoff path (`down` on A, `up` on B) fails the 5 s bar on PG: 9.1 s on one host, 12.8 s cross-host; 131 ms on the VM.** The cost is A's seal-and-publish on PG's USB disks | partial: fails on PG for the ADR-130 reason |
| C4 speed | warm `git status` 1.33 to 1.67× local on PG, 0.86 to 0.89× on the VM, 0.88× on the Mac | proven |
| C5 crash | `kill -9` mid-edit, rescan finds the edit, on Mac (direct and Seatbelt) and Linux; overlay whiteouts captured | proven |
| C6 access | skipped in every run (needs RustFS IAM admin operations and a second principal) | never tested |
| C7 lease | second `up` refused while held, allowed after release and after expiry | proven |

So Tapestry needs, in order:

1. **A fast seal-and-publish target.** C3's handoff miss is PG's HDD, which ADR-130 already removed from the WeftOS publish path. Computers need the same: publish to the SC's inbox (or an SSD-backed L1), not to PG's RustFS.
2. **Per-computer chains and leases on the SC.** Head v2 carries a `chain` field (**proven**), but the SC unit serves one chain (`master`) and one lease. ADR-131 §5.2 designs a prefix per project; nothing serves it. **Not done.**
3. **A root-image layer kind** (board card `bf59a0a7`). ADR-131 §4's attributes drop owner and group, split hard links and refuse absolute symlinks, which a Linux rootfs breaks. The spike has its own `image.rs` with "stripped" entries; it must become one format with D0. **Partial.**
4. **Follower mode over GCS.** The spike's follower (`follow.rs`) reads a RustFS or file store; the drive's refresh (ADR-131 §3.5) is the same operation over signed heads. **Not done.**
5. **The native sandbox checks (C2) and access checks (C6)** run and recorded on `weftos-ci`, with Landlock applied inside the bwrap child (host survey note 1: PG blocks a bare `unshare`). **Never tested.**
6. **The bwrap scoped view** = a materialised image bound read-only as `/` plus the overlay upper as `/work` (spike plan §1 point 3). The "scoped rustfs view" wording in Q9 should be corrected to this. **Proven mechanically, not integrated.**
7. **A "computer" workload kind** in ADR-099 placement and the WeftOS daemon. **Not done.**

Not needed by Tapestry: client-side encryption (D4) until a client project runs in a computer; Windows ProjFS (WSL is the Windows route for computers); WIF reader identities.

## 5. The leftover gaps

| Gap | What exists | What is missing | Label |
|---|---|---|---|
| **Metadata cache** | Full manifest is gzip JSON, 8 MB (30 MB parsed), 1.4 to 5.2 s to fetch and parse on the Mac; deltas are 0.4 KB; ADR-130 gate 3 names a per-path or binary manifest as "the next cost". ADR-128 T2 (keyed tree metadata in RVF, 1.2M lookups/s) exists as a spike crate but is not wired to the GCS path. The mock's `(size, mtime, hash)` index is in `client.rs` | **Closed to tested code 2026-11-08** on mesh-mock `feat/metadata-index` (9d390bc..bcfabc4, merged into `feat/manifest-v2`; tests 36 to 45): manifest v2 and the range-readable `WMR2` form cover the cold join (range join fetches 7.9% of v1); `MESH_DRIVE_INDEX=1` adds one checksummed versioned file per drive at `<out>/.mesh/drive.idx` (manifest id, partial flag, skip list, per-file size, mtime, hash; partial trees point at the range-cache entry). Refresh diffs the index against the new head and stats, hashing nothing; capture hashes only files whose size or mtime changed and rebuilds the index when missing or damaged. Numbers (87k-file loopback tree, loaded machine, compare within a run): capture of 3 edits hashes 3 files instead of 5,873 (222 ms against 2,202 full tree; 1,603 against 7,787 partial); partial-tree refresh with a small delta does 30 ops instead of 5,931 (2.6 s against 12.7 s) and now deletes files the head removed; a full-tree refresh is not faster with the index (1.2 s against 0.47 s: a stat of every file plus one fsync'd write) and buys edit detection and independence from the tree cache. Owner decision: the index is a plain file replaced whole, so the ADR-128 T2 keyed container buys nothing until per-entry updates are needed. Open: sealing capture's put/remove ops into a layer on the SC path is not wired; a same-size same-mtime content change is not detected; a refresh still overwrites a local edit when the head changes the same path; real GCS and CI-host numbers | **built**, loopback numbers |
| **Windows bucket load** | `D:\weftos` on xpc is drvfs/9p: RustFS data there does 130 small PUT/s against 525 on the distro ext4, list and copy 10 to 30× slower (`rustfs-macos-wsl.md`); the host survey rules 9p out as an overlay upper or lower. The planned "cache on `/mnt/d`" cost run is not in any result file | The cost run; a rule that caches and uppers live on ext4 and only bulk data on `D:`; native Windows (ProjFS, ADR-131 §2) was never prototyped and xpc is the only Windows host | partial; native Windows **never tested** |
| **Notifications** | SC side **proven**: Pub/Sub on `head/` (71 ms) and on `incoming/` (the SC's pull, 0.79 s). Device side: one Mac unary pull measured at 0.69 s | The site relay (PG daemon subscribes once, rebroadcasts over the mesh, ADR-131 §3.4, owner question 8); the 30 to 60 s head-poll fallback; the staleness bound that closes the freeze gap; desktop and board notifications for conflicts (§3.6); any CI-host consumer | partial |
| **Everyday deploy** | The SC binary `gcsmesh` is built on the CI host and copied; the unit is installed from instance metadata and the binary from `bin/` in the bucket by pinned SHA-256 (**proven**). Terraform state is local in `/Volumes/ai-models/weftos-worktrees/tf-gcp-seed` | The drive daemon and `weft drive` do not exist, so there is no daily path from an edit to a published head; the mock crate is outside the workspace, outside `scripts/build.sh`, outside signed releases and **outside git** (see §7); no release channel for the SC binary; Terraform state not on a remote backend; no `target-*` CI trigger | not done |
| **Worktree sharing** | The mock ran lanes as APFS clones plus layers (fan-out 16 **proven**); the computer spike has a single-writer lease and a read-only follower (0.5 s per edit, **proven**); ADR-126 §4's worktree overlay is re-expressed as layers; board card `f40828fb` asks for shared worktrees so unmerged docs stay visible. **Correction 2026-11-08:** the L1 tier is built on `feat/adr126-s2` (aepod tip be64b601): tree sharing into the worktree bucket, a registry, and the §4a follower view (`follow`, `search`, `sync-docs`, `leftovers`, `overlaps`), tested on `FsBackend` only | Two writers on one tree are by design conflict records, not a live shared POSIX tree; the L1 follower view has no real-RustFS, two-machine or Windows evidence yet (ADR-126 §4a gate, being produced on the Mac; `docs/plans/mesh-store-next-steps-2026-11-08.md`) | partial, **built** |

## 6. Finish-and-integrate plan

Rules carried in from the owner: targeted tests only; the Mac is for edits and quick checks, CI on aepod-xpc is the authority; Terraform applies and anything under `~/.weftos`, `/data1`, `head/master` or GCP IAM are owner steps; agents never add diff-gate approvals; no commit to `target-0.8.5` of fork docs; stage by path in the shared checkout.

### 6.1 Sequence

| Step | Work | Needs first | Where | Can run alongside |
|---|---|---|---|---|
| **S0** | ~~Put the mesh mock under git~~ **Done 2026-11-08:** `/Volumes/ai-models/research/weftos-mesh-mock` is a git repo (commit `0a05fe7`, "version the mesh-mock as found (2026-11-07), unchanged source"), no remote yet. Still to do: mirror it to a remote and tag the binary the SC runs by SHA-256 | nothing | Mac, minutes | everything |
| **S1** | Owner decisions: accept ADR-131 (closes G2); answer questions 1 (A+), 2 (client data and KMS), 3 (bucket per client), 5 (sparse Mac v1), 6 (Windows now or later), 8 (relay), 9 (pre-warm); re-login `gcloud` | nothing | owner | S2, S3, S4 |
| **S2** | **D0** attributes v2 in the mock crate: `Ent` gains an attribute table outside `canonical()`, directories, chunk lists; the image layer kind (`bf59a0a7`) as the same format with an `image` flag that keeps owner, group and absolute links; property tests; the `target/` rebuild check | S0 | aepod-xpc (`weftos-ci`, build and test) | S3, S4 |
| **S3** | **Metadata cache**: ~~a binary or per-path manifest with the delta chain, and a persistent local index~~ **Built 2026-11-08** on the mesh-mock worktree branch `feat/manifest-v2` (1044589..bc46e88, 18 tests, no new deps, flags `MESH_MANIFEST_V2` and `MESH_INDEX_V2` default off, v1 bytes pinned by test). **Finding:** v1 gunzip plus parse of the 87k-file manifest is about 85 ms on the Mac, so the 1.4 to 5.2 s was the 8 MB fetch, not parsing; v2 is 4.4 MB (47% smaller), one-path lookup 17 ms, subtree 16 ms, delta 370 B; a warm full rewrite is slower (43 vs 26 ms) until the overlay is used; the binary index is 44% smaller, not faster. **Follow-up the same day** (779cc46..f5c80c0, 25 tests): `sc.rs` reads either format (its own delta is still written v1 because the bytes are signed); a range-readable stored form `WMR2` (header, chunk table, top chunk, one compressed chunk per directory subtree, SHA-256 per chunk) at 5.16 MB, 16% bigger than the v2 container, lets a base-only join fetch 707 KB in 3 requests (13.7% of the object, 12× fewer bytes than v1) and parse it in 6 ms; warm `load_tree` lays the delta over the cached parent (13 ms, was 43 ms v2 and 26 ms v1; chain cut at 8); `MESH_GCS_ENDPOINT` and `MESH_MANIFEST_CHUNK` knobs; one end-to-end `gcsmesh` publish and join ran against a loopback GCS stand-in, byte-identical placement, flag on and off. **Decision needed (owner, ADR-128/130):** the head signs the whole stored object's hash, so a partial reader cannot verify; signing the chunk-table prefix digest would let a range-reading join verify. **Range-reading join built later the same day** (dbff123..d26e825, lib tests 25 to 29, one integration test on an in-process loopback store) behind `MESH_RANGE_JOIN` (default off; flag-off pinned to send no Range request): a cold, non-verifying join on a `WMR2` head reads the header and chunk table by Range, then only the chunks of directories it places, each checked against its SHA-256; it steps aside for `--verify-pin`, a warm cache, and v1, v2-container or delta heads. The result type carries `ChunkTable::UnverifiedAgainstSignedHead` and the tree is reachable only through `into_unverified_parts`, since the head still signs the whole object. Loopback numbers on the 87k-file tree: range join 3 requests and 675 KB (7.9% of the 8.52 MB v1 object, 13.2% of `WMR2`, 5 of 46 chunks), manifest phase 10 ms against 114 to 119 ms whole; total cold join about 1.5 s either way because placing 5,931 files dominates on loopback; a projection at 50 ms RTT and 3 MB/s (not measured) puts the manifest phase at 2.9 s v1, 1.8 s `WMR2` whole, 0.4 s range. **Partial-tree cache added** (a5bb783, 323af44; tests 30 to 36): range-join entries live under `<cache>/range/` keyed by manifest id plus sorted skip list, marked unverified, never visible to the verified `trees/` cache, checksummed (a checksum, not a signature: a writer of the cache dir can forge one, as with blobs today); a warm range join now makes 0 manifest requests and 1 GET (`head/master`, 310 bytes), 21 ms against 611 ms uncached and 61 ms warm whole-object; placement on a hit is by missing-or-wrong-size, so a same-size edit is not repaired (same as the existing warm path). Owed: the signing decision (the chunk-table prefix digest is computed and logged, not signed), a test and fallback for a cached entry whose blobs are gone, delta heads over a range-read base (fall back to whole reads), the SC publishing v2 deltas, real GCS and CI-host numbers, oversized single directories | S0 | done on the Mac; CI-host and GCS measurements owed | S2, S4 |
| **S4** | **Computer spike C2 (native) and C6** on `weftos-ci` with a throwaway RustFS: bwrap isolation, Landlock inside the child, disabled credential refused, second principal refused; record in `docs/grok-build-fork/spike-results/` | nothing | aepod-xpc | S2, S3 |
| **S5** | **D1 Mac drive** for WeftOS: daemon with FSEvents plus index scan, clone-place, seal after quiet, ring with `F_FULLFSYNC`, verified refresh; the kill criterion run (`git status` at most 2× local, four `kill -9` points) | S2; S1's question 5 | the Mac, one slot, serial | S4, S6 |
| **S6** | **Per-chain SC**: the unit serves `p/<project>/{head,manifest,packs,lease}` and `computers/<id>/…`; one lease per chain; `clients.json` becomes the signed device registry (ADR-131 §5.1); size-bound the PUT URLs (`x-goog-content-length-range`) | S0; a Terraform metadata update for the unit (in place, owner-approved) | code on aepod-xpc; deploy is an owner step | S5 |
| **S7** | **D2** Linux drive on the CI host (overlayfs upper as the change set), the Mac-to-CI round trip, and **D6** kernel check on PG and the CI host | S5, S6 | aepod-xpc plus the Mac | S8 |
| **S8** | **Notifications**: PG daemon as the one Pub/Sub subscriber (needs a subscriber role on one subscription, owner IAM step), mesh rebroadcast, 30 to 60 s poll fallback, staleness bound; conflict surfaces (`.weft/conflicts`, board card) | S1 question 8 | PG daemon code on aepod-xpc; PG deploy owner-gated | S7 |
| **S9** | **D3** conflicts across the Mac and the CI host | S7, S8 | both | S10 |
| **S10** | **Tapestry computers on the seed store**: `tcomp up/down` against the SC inbox and a `computers/<id>` chain; follower over signed heads; rerun C3's handoff path (expect well under 5 s with the SC awake, since PG's disk leaves the path) | S6, Tapestry Phase 1 exit and Phase 2 for the harness side | aepod-xpc (`weftos-ci`) and PG as the second host | S9 |
| **S11** | **D4** encryption and tenant separation: KMS enabled, a test bucket pair, ML-KEM dependency through the diff gate, keyed names, the 26-case tamper suite on the encrypted chain | S1 questions 2 and 3; owner Terraform apply | SC plus the Mac | S9, S10 |
| **S12** | **Integrate into WeftOS**: move the drive, SC and computer code from the research volume into workspace crates (one for the store client and formats, one for the drive daemon, the SC loop as a weaver role), `scripts/build.sh` targets, diff gate for the new deps (`reflink_copy`, `ml-kem`, fanotify bindings), the ADR-103 per-user daemon hosts the drive, `weft drive status|seal|fetch|resolve|enrol`, signed release of the SC binary, Terraform state to a remote backend | S5 to S9 green | aepod-xpc builds; the Mac edits | S10, S11 |
| **S13** | **D5** migration drill and cutover 0 (shadow) for WeftOS itself on PG, as the `weftos` user | S12; owner window | PG | S14 |
| **S14** | **Windows**: the `/mnt/d` cache cost run; then ProjFS only if S1 question 6 says now (xpc's Windows side is also the CI host, so it needs its own window) | S1 question 6 | aepod-xpc Windows side | after S13 |

### 6.2 What can run in parallel on aepod-xpc now, with no owner gate

- S2 (D0 format and image layer kind, property tests), S3 (metadata cache) and S4 (C2 native and C6) are independent code-and-measure tasks on `weftos-ci` with throwaway stores. They touch no bucket, no `head/master`, no `/data1`. Give each a Sonnet lane; one at a time per host for the measurements so the numbers are not load-skewed.
- S6's code (per-chain SC) can be written and unit-tested on `weftos-ci`; its deploy waits (§6.3).
- Builds of `gcsmesh` and `tcomp` for x86-64 stay on the CI host, as they have been.

Not on aepod-xpc: D1 (S5) must run on the Mac, serially, because the kill criterion is Mac `stat` behaviour; keep it to one slot and no parallel suites.

### 6.3 What must wait

| Waits for | Items |
|---|---|
| The owner's decisions (S1) | A+ inbox URLs (S6's waker change), KMS and bucket-per-client (S11), the relay model (S8), Windows (S14), the first project to cut over (S13), ADR-131 acceptance (declares G2) |
| An owner Terraform apply or IAM step | S6 deploy (unit metadata update), S8's subscriber role, S11's KMS key and bucket pair, Terraform remote state, the PG RustFS upgrade to a release with #9067 and #9073 (card `3f30aaf9`), the digest pin of the waker image |
| A fresh `gcloud` login on the Mac | any re-read of the live head, VM state or billing |
| D1 passing | D2, D3, D5 and the integration (S12); D1 is the kill criterion |
| Tapestry Phase 1 exit (a) to (h) and Phase 2 | S10's harness side (computer picker, hub); the store side of S10 can be built first |
| A PG window as the `weftos` user | S13 import and shadow |

### 6.4 Exit for "finished and integrated"

- D0 to D3 and D5 pass with their ADR-131 bars; D4 passes for the first client the owner names; D6 is recorded (pass or "not on this kernel").
- The drive, store client and SC live in the workspace, build through `scripts/build.sh`, pass the diff gate, and ship as signed releases; the research-volume crates are retired to history.
- WeftOS itself is in cutover phase 2 (PG materialises the head into its own path) with one week of green service checks.
- Tapestry's computer `up`/`down` runs against the SC with C1 to C7 all recorded as pass on `weftos-ci` and PG.
- ADR-128, ADR-130 and ADR-131 carry Updated lines that match the code.

## 7. Risks and things found while writing this

1. **The production SC's source was not under version control** (fixed 2026-11-08: local git repo at `0a05fe7`; a bare mirror with both branches is at `~/weftos-backups/weftos-mesh-mock-mirror.git`, refreshed by hand; a remote under weave-logic-ai is still an owner call since the repo holds the production SC source; the repo plan, mirror and reproducible-build scripts are in the mesh-mock worktree `docs/repo-plan.md`, see the next-steps plan row 3e). `/Volumes/ai-models/research/weftos-mesh-mock` (18 source files, about 6,500 lines, including `sc.rs`, `lease.rs`, `head.rs`, `pq.rs`, `wake.rs`, `bin/gcsmesh.rs`, `bin/seedwaker.rs`) has no `.git`. The CI host holds a copy said to be "in sync". The binary on the SC is pinned by SHA-256, so what runs is known, but how it was built is not reproducible. S0 fixes this and should happen before anything else.
2. **The live state could not be verified today.** `gcloud` on this Mac needs `gcloud auth login` (the token refresh fails non-interactively). Everything in §2 about the live head and VM is as of 2026-11-01.
3. **The computer spike never touched GCS**, so "computers on the seed store" is unproven end to end; C3's handoff miss on PG is the strongest reason to move it there.
4. **Two ADRs describe a world slightly ahead of or behind the code** (§1): the budget line in ADR-130 and G2 in ADR-128.
5. **D1 is a real kill criterion.** If a Mac drive with FSEvents plus an index scan cannot hold `git status` under 2× local, the design returns to the per-stat question that killed macFUSE, and there is no measured fallback for the Mac.
6. **Spot capacity.** A wake can fail with no Spot capacity in us-east1-b; the fallback to a standard VM is manual (ADR-130 "Still open").
7. **Terraform state is a local file** in a worktree on the external drive; losing it means importing 20-odd resources by hand.

## 8. Owner summary

The cloud side is done and paid for: the seed store works, is signed with both signature schemes, fails over to PG and back, survives tampering tests, stops itself when idle and wakes in about 72 seconds, and costs a few dollars a week at the current duty cycle. What does not exist yet is the thing people would use: the drive. Nothing of ADR-131 has been built. The one decision it needs from you first is to accept the design, which settles the mount question by not mounting at all: files are real files on the laptop, kept in step by a small daemon.

Tapestry does not need any of this until its Phase 3. Its computer spike already proved images, moves between machines, crash recovery and the lease, but it ran against PG's slow disks and a throwaway store, so the one failing number (a 9 to 13 second handoff) is the same disk problem the seed store already solved. Pointing computers at the seed store fixes it, once the SC can hold more than one chain.

Three things to do this week, in order: push the seed cone's source to a remote (it is under local git since 2026-11-08); log in to gcloud so we can read the live state again; answer the nine ADR-131 questions, or at least the first three. With those, the format work, the metadata cache and the remaining sandbox checks can run on aepod-xpc in parallel with no further approvals, and the Mac drive prototype can start after the format lands.
