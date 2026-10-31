# ADR-130: GCS seed store: integrated master layers published one way to a bucket, one elected writer on a Compute Engine VM, PG kept as seed and standby

- **Status**: Proposed (2026-10-30; owner direction, see Context). Adoption is gated on the spike in "Spike plan".
- **Date**: 2026-10-30
- **Updated**: 2026-10-30 (owner replan: the writer is stateless; GCS is the only durable store; Titanium Local SSD replaces the PD-SSD state disk)
- **Updated**: 2026-10-31 (owner naming: the elected writer node is the **seed cone**, **SC** for short; the GCP node is `weftos-seed-cone`)
- **Deciders**: owner
- **Prior decisions**: the decision store, consulted on 2026-10-30, holds nothing on cloud-hosted seed storage.
- **Builds on**:
  - ADR-126: the mesh object store on RustFS (becomes the optional L1 staging tier);
  - ADR-128: RVF layered trees (layers, packs, one writer per tip, ring write-behind);
  - ADR-129: upstream rvf-crypto and real ML-DSA-65 (the dual signature that makes untrusted storage safe);
  - ADR-028: SHAKE-256 hashing with Ed25519 and ML-DSA-65 dual signing;
  - ADR-099: placement.
- **Changes**: nothing is removed. ADR-126 and ADR-128 gain a tier above them; both carry an Updated note pointing here.

## Context

The owner's decisions on 2026-10-30:

- Integrated master layers (sealed, content-addressed, dual-signed per ADR-129) go **one way** into a **GCS bucket**. Any node pulls packs from it.
- There is **one elected writer**, a **Compute Engine VM** with a fast disk that "acts as PG", next to the bucket. The owner named this role the **seed cone (SC)**; the GCP node is `weftos-seed-cone`. Whichever node holds the writer lease is acting as the SC, so PG becomes the SC when it takes over.
- **PG stays the seed, the standby writer and a full replica.**
- GCP project `weavelogic-internal`, region as close to Orlando and Miami as possible: **us-east1** (South Carolina). The bucket may be a configurable dual-region, us-east1 plus us-east4.
- An existing bucket in us-central1 is unrelated. This work never references it.

Why now. The mesh mock (`docs/research/adr128-mesh-mock-2026-10-30.md`) measured the limits of PG as the only store:

- PG's disks are USB hard disks. RustFS issues about four syncs per PUT at 26 to 29 ms each, so small PUTs cap at about 40 per second. The cap is fsync, not the audit sink or the network.
- A client ring that packs small writes into segments moves the cost to bulk uploads, which are limited by the link and the HDD.
- Cold joins from the layer store lose to a public clone plus install (CI host 90 s against 32 s; Mac 19 s against 9 s). Warm joins (2 to 4 s, no bytes) and adoption (nothing moves) are where the layer store pays.

A bucket close to the nodes, written by a VM with a fast disk, removes the HDD from the publish path and gives every node, including ones that cannot reach PG, one place to pull sealed state from.

## Decision

### 1. Three tiers

| Tier | What | Holds | Writes |
|---|---|---|---|
| L0 | Local: the ring plus CoW branches (ADR-128) | A node's working layers, unsealed | Always local |
| L1 | Optional LAN or PG staging (RustFS, ADR-126) | Unmerged layers that other nodes should see before integration | Optional; skipped on a single node |
| L2 | GCS seed bucket | Integrated master only: sealed, dual-signed, content-addressed | One elected writer |

L2 is the only tier a cold node needs. L1 is a latency and sharing aid and can be absent.

### 2. Bucket layout

All object names under a content hash are immutable and written once.

```
packs/<hash>        pack segments (the ring's segment objects, ADR-128)
packidx/<hash>      pack indexes
layers/<hash>       sealed layer objects (signed delta + lineage record)
manifests/<hash>    a manifest naming the layers and packs for one master state
head/head           the one mutable pointer: manifest hash, sequence number, dual signature
lease/writer        the writer lease (section 4)
```

Only `head/head` and `lease/writer` are overwritten. Versioning is on, so every overwrite of those keeps its predecessor.

### 3. Publish protocol (writer only)

1. Integrate the incoming layers in the writer's Local SSD workspace; seal; dual-sign (Ed25519 + ML-DSA-65, ADR-129).
2. Upload packs, pack indexes, layers and the manifest under their content-hash names with `ifGenerationMatch=0` (create only; a 412 means the object is already there and counts as success). Uploads are concurrent.
3. Update `head/head` with `ifGenerationMatch=<generation read at the start of integration>`. A 412 means another writer won: stop, re-read the head, re-integrate. This compare-and-swap is the only commit point; objects uploaded in step 2 but not reachable from `head` are garbage and are collected later.
4. The head write emits an `OBJECT_FINALIZE` notification (section 6).

The writer needs no ordering guarantee beyond this: a reader that sees a head always finds every object it names, because step 2 completed before step 3 began.

### 4. Writer lease and election

`lease/writer` holds `{holder, node key id, epoch, expires_at}` and is updated with `ifGenerationMatch`. The Compute Engine VM holds it by default and renews on an interval well inside the expiry. PG takes over when the lease has expired and the VM is unreachable: it writes a new lease with epoch + 1 using the generation it read. The signed head carries the epoch, and readers refuse a head whose epoch is lower than one they have seen. A returning old writer finds its generation stale, fails the compare-and-swap and stands down.

The lease protects against split writers through GCS's own atomicity; it is not a clock-synchronised lock. Expiry uses the object's server time (`updated`), not the writer's clock.

### 5. Readers

GCS is **untrusted storage**. A reader:

1. Fetches `head/head` and verifies the dual signature against the pinned node keys (ADR-129), and checks the epoch and sequence number never go backwards.
2. Fetches the manifest and checks its hash against the head.
3. Range-reads packs into a local content-addressed cache, checking each blob's hash on read. An object that fails verification is discarded and reported; the head is not advanced.

A tampered object therefore fails verification in step 1, 2 or 3 and is never used.

### 6. Change notification

A bucket notification on `OBJECT_FINALIZE` with prefix `head/` publishes to a Pub/Sub topic. Nodes subscribe (pull, one subscription each) and fetch the head on a message. No polling. A missed message is repaired by a slow background head check (minutes), not by a tight loop.

### 7. Durability

- Object versioning on. Noncurrent versions are deleted after a configurable number of days (default 30).
- No retention policy and no Bucket Lock in the spike. Bucket Lock is irreversible and comes later, once the object naming is stable.
- Autoclass comes later, once the access pattern is known.
- Public access prevention enforced; uniform bucket-level access.

### 8. Authentication and keys

- **No long-lived keys on nodes.**
- **Writer:** the VM's attached service account holds `roles/storage.objectAdmin` on this bucket only (plus bucket metadata read). The spike uses that service account and nothing else.
- **Readers (designed, not built in the spike):** read-only access by Workload Identity Federation from each node's identity provider, or by short-lived signed URLs minted by the writer for a manifest's packs. Signed URLs are the simpler first step because they need no per-node GCP identity; WIF is the target for nodes that read a lot.
- **Node signing keys:** in Secret Manager now (the spike's only secret is the Tailscale key container). Cloud KMS is **not enabled** in the project; signing in KMS needs the API turned on and a decision on key custody (ML-DSA-65 is not a KMS algorithm, so KMS can hold the Ed25519 half or wrap the key at rest, not sign the post-quantum half).
- The Tailscale auth key is a secret the owner provides. Terraform creates the secret container only. The value is never in Terraform, state or this repository.

### 9. Client data

WeftOS itself is public. Client data is the owner's decision per client, taken before a client's layers are published:

- **Encrypted on the writer before upload**, with a key per project (the key stays off GCS), or
- **kept out of GCS** (L0 and L1 only).

The spike publishes only the mesh mock's WeftOS master head, which is public source.

### 10. The writer VM: stateless, on Titanium Local SSD

**GCS is the only durable store.** The writer's working state (the git merge workspace, caches and the ring WAL) lives on Local SSD and is rebuilt from GCS if lost. There is no PD-SSD or Hyperdisk state disk.

**Why Local SSD.** Persistent Disk and Hyperdisk are network-attached, and their performance scales with queue depth and I/O size (see Google's guidance on optimizing persistent disk performance). Serial small fsyncs, which is what a ring group-commit and a git merge workspace issue, are their worst case. Local NVMe is directly attached and fastest for that pattern.

**The WAL's durable copy is GCS.** The writer group-commits to Local SSD (fast ack), and the ring streams closed segments to GCS as content-hash objects (section 3). A write is durable when its segment is in GCS and the layer put has waited for it, exactly as in the mock's ring. Between the ack and the upload, the Local SSD is the only copy, so the writer treats an acked-but-not-uploaded write as in flight, not as published: nothing is signed into a head until its objects are in GCS.

**Rehydration on loss.** If the Local SSD comes back blank (default stop, unrecovered host error, replacement VM), the writer takes no action until it has re-read `head/head`, verified it, and refilled the cache and workspace from GCS by range reads. In-flight writes that never reached GCS are lost and are re-driven by their senders (the ring's recovery path, ADR-128); this is the same case as a crash today. While rehydrating, the writer does not renew the lease, so PG takes over if the rebuild is slower than the lease expiry.

**Lease failover to PG** is unchanged (section 4). PG holds a full replica and needs no rehydration.

| Item | Choice | Reason |
|---|---|---|
| Machine | `c4-standard-4-lssd`, standard (not spot), us-east1-b | The smallest bundled-Titanium-Local-SSD shape: 4 vCPU, 15 GB, one 375 GB Titanium SSD. Offered in us-east1-b, -c and -d. C4D and C3 have `-lssd` shapes too; C4 is preferred for Titanium SSD and the lowest small shape |
| Boot | 40 GB Hyperdisk Balanced | C4 requires Hyperdisk for boot. No durable data lives here |
| State | None. Titanium Local SSD, rebuildable scratch | See above |
| Image | Ubuntu 24.04 LTS | Matches the CI host's family and the WeftOS release targets |
| Network | Own VPC, private subnet, **no external IP**, egress through Cloud NAT, a deny-all ingress rule, no allow rules | No public inbound ports. Tailscale needs outbound only |
| Access | Joins Tailscale at boot with the key from Secret Manager; OS Login on | Reach it over the tailnet; no SSH from the internet |
| Hardening | Shielded VM | Cheap and standard |

Everything carries labels `project=weftos`, `purpose=seed-spike`, `adr=adr-130`.

**Local SSD behaviour (Compute Engine Local SSD documentation, "data persistence", read 2026-10-30).**

| Event | Local SSD data |
|---|---|
| Guest OS reboot | Persists |
| Live migration during host maintenance | Persists |
| Maintenance on a terminate-and-restart-only instance | Instance restarts in place, data intact |
| Default stop or suspend | **Discarded**; disks are blank on resume |
| Forced stop after guest shutdown | Not preserved |
| Host error | Recovery only if `automaticRestart` is set and within the Local SSD recovery timeout; otherwise **blank, unformatted disks** |
| Stop-on-maintenance without live migration | Does not persist |
| Spot or preemptible preemption | Does not persist |
| Instance deletion | Permanently lost |
| Stop or suspend with preservation (Preview, opt-in) | Copied to durable storage and restored; **not available on machine types that use Titanium SSD** |
| Power loss on C4, C4A, C4D, C4N, H4D | Disks **might not recover all writes completed before the loss** |

Consequences. The writer must treat the Local SSD as lost on any stop and as possibly missing recent acked writes after a power event, so durability is carried by GCS and never by the disk. That is the reason the writer is stateless. Local SSDs cannot be snapshotted or added after creation, and customer-managed keys cannot be used with them (data is encrypted by Compute Engine). Device names can change across restarts, so mounts use `/dev/disk/by-id/`. The startup script formats the device only when it has no filesystem (an existing one is fsck-checked and mounted), and writes a fresh-format marker so the writer knows whether to rehydrate.

**I/O-shaping rules** (from the PD/Hyperdisk guidance, applied to avoid the network-disk worst case and keep the Local SSD fast):

- Group small writes into segments of 256 KB or larger before they hit the disk or the network; never one fsync per small file (the ring already does this).
- Keep queue depth high: many segments in flight to GCS, and parallel workspace operations, rather than serial round trips.
- ext4 with lazy init off (`lazy_itable_init=0,lazy_journal_init=0`) and `discard` at mkfs and mount, I/O scheduler `none`.
- Reference disks by `/dev/disk/by-id/`, never by `/dev/nvme*`.

**Spot as a later option.** The Terraform has a `provisioning_model` variable (default `STANDARD`). Spot is cheaper but the Local SSD does not survive preemption and the lease must fail over to PG. It becomes reasonable only after gate 7 and rehydration are proven, and only for a second, non-default writer.

### 11. Bucket location

The Terraform defaults to **single-region us-east1**: cheapest, lowest latency to Orlando and Miami, and the spike's data is rebuildable from PG and git. `dual_region = true` switches to a custom dual-region with `US-EAST1` and `US-EAST4` (location `US` with a custom placement config). Choose dual-region before any data that cannot be rebuilt goes in; changing a bucket's location later means a new bucket and a copy.

## Cost estimate (us-east1, list prices, monthly, estimates)

These are estimates from public list prices as known on 2026-10-30. They were not re-read from the live pricing pages during this work; check each before relying on them. Sources: Compute Engine pricing (`cloud.google.com/compute/all-pricing`), Cloud Storage pricing (`cloud.google.com/storage/pricing`), Cloud NAT pricing (`cloud.google.com/nat/pricing`), Pub/Sub pricing (`cloud.google.com/pubsub/pricing`), Secret Manager pricing (`cloud.google.com/secret-manager/pricing`). Sustained-use discounts on n2 are ignored (they lower the VM line).

| Item | Basis | Estimate |
|---|---|---|
| `c4-standard-4-lssd` VM, 730 h, including its 375 GB Titanium Local SSD | C4 compute is about $0.19 per hour for 4 vCPU and 15 GB; the bundled SSD is priced into the `-lssd` shape (a separate Local SSD would be about $0.08 per GB-month). Not confirmed against the live SKU | about $150 to $185 |
| 40 GB Hyperdisk Balanced boot | about $0.08 to $0.10 per GB-month plus baseline IOPS and throughput | about $4 |
| Cloud NAT gateway | per-VM hourly plus about $0.045 per GB processed | about $1 to $5 |
| GCS Standard, single region | about $0.02 per GB-month; 100 GB assumed | about $2 |
| GCS Standard, dual-region (option) | roughly twice single-region, plus inter-region replication | about $4 plus replication |
| GCS operations | Class A about $0.005 per 1,000 | under $1 for the spike |
| Internet egress from GCS | about $0.12 per GB after the free tier | the large unknown: about $12 per 100 GB pulled |
| Pub/Sub, Secret Manager | within free tiers or cents | under $1 |
| **Total, fixed** | | **about $160 to $200 per month**, against about $210 for the earlier n2 plus PD-SSD design |

The VM dominates. The smallest `-lssd` shape is already 4 vCPU, so there is no cheaper bundled-SSD shape; Spot (above) is the lever, later. The spike records actual cost (gate 8).

## Alternatives considered

- **Filestore (managed NFS).** No: it is a mutable POSIX share, which is the model ADR-128 moves away from, and its minimum size and price are high for a store of immutable objects.
- **Cloud Run.** No persistent fast disk and no stable long-lived process to hold the lease and the working state.
- **GKE.** Overkill for one stateful process and a bucket.
- **The CI host as the writer.** Possible later as a second standby; it sits behind a residential link and is a build runner first. Not the default.
- **S3-compatible RustFS in the cloud instead of GCS.** Keeps one protocol, but we would run and patch the store, and lose GCS durability, `ifGenerationMatch` and notifications. GCS also exposes S3-style interoperability if a client needs it, but the spike uses the native JSON/gRPC API for the compare-and-swap.

## Spike plan

All gates are recorded in this ADR on completion. The Terraform is a separate root, `gcp/seed-store/` in the private environment repository, planned but not applied until the owner approves the plan.

| # | Gate | Pass condition |
|---|---|---|
| 1 | Publish the mesh mock's current master head from the VM to the bucket | `head/head` verifies from a clean checkout of the keys |
| 2 | The CI host and the Mac pull from it | Both reach the same verified head and read back every blob |
| 3 | **First gate.** `fio` sync-write latency, queue depth 1 and 32, 4 KB and 256 KB writes, on Titanium Local SSD against Hyperdisk Balanced against PD-SSD (attach the latter two temporarily for the spike) | Median and p99 latency and IOPS side by side, with PG's HDD (26 to 33 ms fsync) as the baseline. If Local SSD is not clearly faster at queue depth 1, the stateless-writer premise fails and the design returns to a state disk |
| 4 | Round-trip time from the Mac, the CI host and PG to the bucket and to the VM | Median and p99 for a head fetch and a 4 KB range read |
| 5 | Publish throughput | Seal-and-publish of the same tree as the RustFS baseline (CI host 19 to 45 s) |
| 6 | Cold and warm joins against the RustFS and git baselines | Cold fetch of the base against 18.8 s (Mac) and 14.4 s (CI host) from RustFS; warm join against 2 to 4 s |
| 7 | Kill the writer; PG takes the lease. Also stop the VM (Local SSD comes back blank) and confirm the restarted writer rehydrates from GCS before renewing the lease | PG holds epoch + 1 within the lease expiry; the old writer's late head update fails with 412 |
| 8 | Tamper with an object; verification fails | A flipped byte in a pack, a manifest and a head are each rejected by a reader, and the head does not advance |
| 9 | Record the actual cost | Billing for the spike period against the estimate table |

Kill criteria: gate 7 or 8 failing is a design failure, not a tuning problem. Gate 3 is run first. Gates 4 to 6 failing against the baselines means the tier adds cost without a speed benefit and the spike stops.

## Spike results: gates 1 and 2 (2026-10-31)

Run on the SC (`c4-standard-4-lssd`, us-east1-b, Ubuntu 24.04, fio 3.36, RustFS 1.0.4 official x86_64 gnu release, checksum verified). In this section "gate 1" is the disk test (spike-plan gate 3) and "gate 2" the round-trip test (spike-plan gate 4, partly: the bucket and the SC, not yet the 4 KB range read from each host). Temporary disks were created with gcloud outside Terraform and deleted afterwards; `terraform plan` shows no changes.

### Gate 1: disks

Disks under test: Titanium Local SSD (`/mnt/scratch`, ext4, `discard`, scheduler none); a 100 GB Hyperdisk Balanced at its defaults (3,600 provisioned IOPS, 290 MB/s); a 100 GB PD-SSD. **PD-SSD cannot be used on this machine type**: attaching it fails with "pd-ssd disk type cannot be used by c4-standard-4-lssd machine type", so the third column of the original plan does not exist on C4. Both formatted ext4 with lazy init off and `discard`. 30 s per fio run, buffered `--ioengine=sync --fsync=1` at queue depth 1, then `libaio --direct=1 --iodepth=32 --rw=randwrite`.

**fio, queue depth 1 (write then fsync after every write)**

| Disk | bs | IOPS | MB/s | write p50 | fsync p50 | fsync p99 | fsync p99.9 |
|---|---|---|---|---|---|---|---|
| Local SSD | 4k | 6,904 | 28 | 3 us | 136 us | 281 us | 465 us |
| Local SSD | 256k | 1,296 | 340 | 41 us | 823 us | 1.45 ms | 1.48 ms |
| Hyperdisk Balanced | 4k | 1,205 | 4.9 | 4 us | 791 us | 1.38 ms | 1.81 ms |
| Hyperdisk Balanced | 256k | 536 | 140 | 44 us | 1.79 ms | 2.54 ms | 3.06 ms |
| PG USB HDD (baseline, earlier test) | 4k | | | | 26 to 33 ms | | |

**fio, queue depth 32 (libaio, direct, random write; completion latency)**

| Disk | bs | fdatasync | IOPS | MB/s | clat p50 | p99 | p99.9 |
|---|---|---|---|---|---|---|---|
| Local SSD | 4k | no | 85,752 | 351 | 477 us | 498 us | 502 us |
| Local SSD | 4k | every write | 43,486 | 178 | 473 us | 3.0 ms | 5.8 ms |
| Local SSD | 256k | no | 1,340 | 351 | 23.5 ms | 25.0 ms | 43.8 ms |
| Local SSD | 256k | every write | 1,332 | 349 | 23.5 ms | 33.4 ms | 46.9 ms |
| Hyperdisk Balanced | 4k | no | 3,619 | 15 | 8.6 ms | 10.7 ms | 18.0 ms |
| Hyperdisk Balanced | 4k | every write | 1,306 | 5.3 | 24.8 ms | 33.8 ms | 39.1 ms |
| Hyperdisk Balanced | 256k | no | 1,168 | 306 | 27.7 ms | 41.2 ms | 53.7 ms |
| Hyperdisk Balanced | 256k | every write | 1,162 | 305 | 27.7 ms | 50.1 ms | 61.6 ms |

Both disks sit on their limits: Hyperdisk at its provisioned 3,600 IOPS (4k) and about 300 MB/s (256k); Local SSD at about 350 MB/s for large blocks. Large writes are bandwidth bound on either disk, so the 256 KB segment rule is about the sync count, not about escaping a latency floor.

**RustFS-style object, four syncs each** (create temp file, write 4 KB, fsync, rename, fsync directory; Python, 20 s per cell)

| Writers | Local SSD obj/s | p50 | p99 | Hyperdisk obj/s | p50 | p99 |
|---|---|---|---|---|---|---|
| 1 | 3,692 | 0.26 ms | 0.40 ms | 669 | 1.38 ms | 2.66 ms |
| 4 | 7,296 | 0.51 ms | 0.91 ms | 1,138 | 3.46 ms | 4.70 ms |
| 8 | 9,277 | 0.81 ms | 2.58 ms | 1,618 | 4.94 ms | 6.58 ms |
| 32 | 12,318 | 2.14 ms | 9.04 ms | 2,689 | 11.82 ms | 16.50 ms |
| 128 | 12,423 | 7.80 ms | 17.56 ms | 3,010 | 40.89 ms | 67.58 ms |

### Gate 1b: RustFS 1.0.4 PUT burst, same probe as the PG test

A throwaway RustFS on loopback with its data directory on each disk, audit off (PG mode (a)), own temporary credentials in a 0600 file, 2,000 PUTs per cell using the same `probe.py` as the PG test (`probe.py` creates up to 8 client processes; the probe client runs on the same four vCPUs as RustFS). PUT/s with p50 and p99 in ms.

| Writers | 4 KB Local SSD | 4 KB Hyperdisk | 4 KB PG HDD | 64 KB Local SSD | 64 KB Hyperdisk | 64 KB PG HDD |
|---|---|---|---|---|---|---|
| 1 | 353 (2.8 / 3.2) | 235 (4.2 / 5.0) | n/a | 308 (3.2 / 3.7) | 195 (5.0 / 6.1) | n/a |
| 4 | 939 (4.0 / 6.3) | 571 (6.8 / 8.9) | 21.3 (173 p50) | 838 (4.6 / 6.2) | 526 (7.4 / 9.9) | 13.2 (272 p50) |
| 8 | 1,095 (6.7 / 10.1) | 698 (10.8 / 15.9) | n/a | 977 (7.6 / 11.0) | 714 (10.6 / 14.5) | n/a |
| 32 | 959 (24.8 / 101.9) | 745 (33.6 / 89.5) | 48.1 (623 p50) | 873 (28.0 / 110.5) | 710 (36.1 / 95.5) | 36.5 |
| 128 | 564 (100.3 / 412.2) | 494 (130.4 / 428.5) | 48.1 (2,353 p50) | 537 (109.9 / 382.7) | 472 (139.2 / 477.3) | 35.7 |

Syncs per PUT, `strace -f -c -e trace=fsync,fdatasync -p <rustfs pid>`, 32 writers, 640 4 KB PUTs: 1,926 `fsync` plus 646 `fdatasync` = 2,572, which is **4.0 syncs per PUT**, the same as on PG. Mean time per sync under strace (which inflates it): Local SSD 126 us (`fsync`) and 223 us (`fdatasync`); Hyperdisk 568 us and 1,232 us; PG 26 to 29 ms.

### Gate 2: round-trip times

Ping to the SC's tailnet address, 30 echoes each, from the host named. Tailscale SSH to the CI host and PG is not permitted, so the tests ran as the hosts' ordinary users with plain `ping`; the path was direct at the end of every run (the first pongs of each `tailscale ping` went via the Ashburn DERP relay at 50 to 150 ms before the direct path came up).

| From | min | median | p95 | max | Path |
|---|---|---|---|---|---|
| Mac | 28.2 ms | 29.3 ms | 88.7 ms | 94.4 ms | direct (the p95 is a few slow Wi-Fi samples) |
| CI host | 25.4 ms | 25.8 ms | 27.1 ms | 29.0 ms | direct |
| PG | 34.6 ms | 39.2 ms | 46.2 ms | 52.2 ms | direct |

From the SC to Cloud Storage (`gs://weavelogic-weftos-seed`, JSON API, VM service account, each call a fresh `curl` so each includes DNS lookup, TCP and TLS, about 30 ms of the figures; ten calls each, objects under `spike/` only and deleted afterwards):

| Operation | min | median | p95 |
|---|---|---|---|
| Object metadata GET (small) | 29.8 ms | 37.8 ms | 43.8 ms |
| 1 KB PUT | 66.1 ms | 74.6 ms | 80.3 ms |
| 1 KB PUT, `ifGenerationMatch=0` (the head compare-and-swap shape) | 67.9 ms | 74.0 ms | 83.3 ms |
| 1 KB GET | 44.4 ms | 45.9 ms | 54.0 ms |
| 8 MiB PUT | 126 ms | 161 ms | 212 ms |
| 8 MiB GET | 86.4 ms | 114 ms | 122 ms |

### What this means for the design

- **The stateless-writer premise holds.** At queue depth 1 an fsync on the Local SSD takes 0.14 ms at 4 KB, about 190 times faster than PG's HDD and 5.8 times faster than Hyperdisk at its defaults. The "Local SSD clearly faster at queue depth 1" condition is met.
- **RustFS-style objects are fast on the Local SSD, and the cap on PG is the disk.** The same four-sync PUT that tops out at 48/s on PG's HDD does 1,095/s (4 KB, 8 writers) on the SC's Local SSD, about 23 times more, with p50 under 7 ms. Raw four-sync objects reach 12,400/s. Past 8 writers the RustFS figures fall, and Hyperdisk and Local SSD converge; with the load generator on the same four vCPUs the RustFS test is CPU bound above that point, so read the 32 and 128 writer cells as a CPU limit of this VM shape, not a disk limit. The mean sync is 0.15 ms against 26 to 29 ms on PG, so the ring's segmenting is no longer needed for the disk's sake on the SC, only for GCS request count.
- **Hyperdisk Balanced as a durable fallback is usable, not equal.** At defaults it manages 0.8 ms fsync, 3,600 IOPS and about 300 MB/s, and 745 RustFS PUT/s. It is 4 to 6 times slower than Local SSD on raw syncs and 1.2 to 1.6 times slower through RustFS. If the Local SSD is lost and the writer needs a state disk, this is good enough for the publish path; provisioned IOPS can be raised. PD-SSD is not an option on C4.
- **GCS op latency against the planned cadence.** A 1 KB write or a head compare-and-swap costs about 75 ms, a read about 46 ms, an 8 MiB segment about 160 ms including connection setup (less on a reused connection). The planned 250 ms segment timer with 4 to 8 segments in flight fits: one segment PUT finishes well inside the timer, and a head advance (one segment wait plus one conditional PUT) is about 250 to 350 ms of added latency.
- **Network to the SC:** 26 to 39 ms median from the CI host, the Mac and PG, all direct. Fetching a head is therefore a one-RTT call from any node on the tailnet, and the SC adds no more than about 40 ms to a writer round trip.
- **Not measured here:** parallel segment throughput to GCS (my 8-way test did not validate its own results and is excluded), Mac, CI host and PG round trips to the bucket itself, and the 4 KB range read.

## Consequences

- The writer holds no durable state: losing the VM, its disk or its zone costs rehydration time, not data.
- A new external dependency (GCP) exists only at L2; L0 and L1 keep working with no cloud.
- Reads leave the home network for cold joins. Egress cost is the unknown to measure.
- Trust moves entirely to signatures: a compromised bucket can withhold or delay, not forge.
- PG's role narrows to seed, standby writer and replica, which also removes its HDD from the publish path.
- Open: reader auth (signed URLs first, then WIF), KMS custody, per-client encryption policy, garbage collection of unreachable objects, and Bucket Lock.
