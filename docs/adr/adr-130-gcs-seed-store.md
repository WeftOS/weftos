# ADR-130: GCS seed store: integrated master layers published one way to a bucket, one elected writer on a Compute Engine VM, PG kept as seed and standby

- **Status**: Accepted (2026-11-01, owner). Production decisions taken with the acceptance: the SC runs as a **Spot** instance (`instance_termination_action = STOP`), and every gate 9 cost control is built (idle-stop with a waker, a Pub/Sub trigger on `incoming/` in place of the 10 s LIST poll, the boot disk at baseline IOPS and throughput, dependency trees out of default cold joins, a budget alert on the `adr=adr-130` label). See "Production build".
- **Date**: 2026-10-30
- **Updated**: 2026-10-30 (owner replan: the writer is stateless; GCS is the only durable store; Titanium Local SSD replaces the PD-SSD state disk)
- **Updated**: 2026-10-31 (owner naming: the elected writer node is the **seed cone**, **SC** for short; the GCP node is `weftos-seed-cone`)
- **Updated**: 2026-11-01 (the SC writer unit; ML-DSA-65 dual signing of heads and manifests; gate 8 tamper tests; gate 9 cost; spike cleanup)
- **Updated**: 2026-11-01 (accepted by the owner: Spot SC, idle-stop and waker, and the gate 9 cost controls for production)
- **Updated**: 2026-11-01 (ADR-131, Proposed: the seed drive over this store; per-project chains, a seed and inbox bucket per client, client-side encryption for §9, waker-minted inbox URLs (A+) and the plan to make the SC the primary file holder with PG as standby, replica and backup)
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
| Machine | `c4-standard-4-lssd`, **Spot** since the production build (was standard during the spike), us-east1-b | The smallest bundled-Titanium-Local-SSD shape: 4 vCPU, 15 GB, one 375 GB Titanium SSD. Offered in us-east1-b, -c and -d. C4D and C3 have `-lssd` shapes too; C4 is preferred for Titanium SSD and the lowest small shape |
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

**Spot as a later option.** The Terraform has a `provisioning_model` variable (default `STANDARD`). Spot is cheaper but the Local SSD does not survive preemption and the lease must fail over to PG. It becomes reasonable only after gate 7 and rehydration are proven, and only for a second, non-default writer. **Superseded on acceptance (2026-11-01):** the owner chose Spot for the SC itself; see "Production build". A preemption stops the SC, and the waker starts it again.

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

## Spike results: gate 3 (2026-10-31)

Spike-plan gates 1, 5 and 6 (publish, publish throughput, cold and warm joins), the 4 KB range read and bucket round trips of gate 4, and the reader-access question. The SC published the mesh mock's merged master head (the w4 integrator layer `085bfe31`, 87,158 files, 2.2 GB) into the seed bucket, and the CI host and the Mac joined from it. Tool: `gcsmesh` in the mesh-mock crate (`src/gcs.rs`, `src/gcsflow.rs`, `src/bin/gcsmesh.rs`): GCS XML API with the VM's metadata-server token, plus V4 signed URLs for readers. Objects written: `packs/<hash>`, `layers/<id>`, `manifest/<id>` (all with `x-goog-if-generation-match: 0`, a 412 counts as success) and `head/master` (generation compare-and-swap only). The manifest is gzip JSON (30 MB raw, 8.0 MB gzipped) holding the full tree and a blob-to-pack location map; later heads are delta manifests over a parent.

### Publish

| Item | Result |
|---|---|
| Source | PG's RustFS and the mock service over the tailnet, read from the SC |
| Objects written | 686 pack or blob objects (151 existing packs copied as is, 10 new 8 MiB packs repacked from 5,878 small loose blobs, 525 large loose blobs), 4 lineage layers, 1 manifest, 1 head = 692 PUTs |
| Bytes up | 2,245,718,377 (2.25 GB) |
| Wall time | 104 s: 7 s reading layers and tree, 6 s locating blobs, 90 s copying, 1 s layers, manifest and head |
| Throughput | 21.6 MB/s. The source is the limit (RustFS on PG's disk over the tailnet at about 24 MB/s, as for the CI host in the RustFS baseline), not GCS or the SC |

**Compare-and-swap.** The first `head/master` write used `ifGenerationMatch=0` and got generation 1793455059458548. A synthetic two-file layer (a 4 KB and a 1 KB file, signed with a throwaway key, delta manifest of 408 bytes) moved the head with `ifGenerationMatch=1793455059458548` to generation 1793455271223423. A write with the old generation was refused with 412 and the head stayed. The real head was restored with the current generation (1793455290269087). The warm-join test for the CI host repeated this (1793455290269087 to 1793456400405888, then back to 1793456410435237). Each head write took 60 to 100 ms.

**Pub/Sub.** A temporary pull subscription on the head topic received one `OBJECT_FINALIZE` message for each successful `head/master` write, with the object generation in its attributes, and none for the refused 412. The subscription was deleted afterwards. No `spike/` objects were written.

### Joins

A join reads `head/master`, then the manifest (and parents until a cached tree), fetches only blobs missing from a local content-addressed cache (whole pack when two or more wanted blobs share it, otherwise a range), hash-checks every blob, and materialises the tree. 32 fetch threads.

| Client | Case | Time | Bytes | GCS requests | RustFS baseline |
|---|---|---|---|---|---|
| CI host | Cold, full tree incl. dependency prep (77,100 blobs, 87,158 files) | **22.5 s** (head 0.15, manifest 0.5, fetch 21.4, materialise 0.4) | 2.23 GB | 688 | 90.3 s, 2,120 MB |
| CI host | Warm, one 2-file layer | **0.45 s** | 5.9 KB | 3 | 2 to 4 s, no bytes |
| Mac | Cold, base only (dependency trees skipped, 5,878 blobs) | **14.0 s** (13.8, 14.0, 19.1 over 3 runs; head 0.24, manifest 1.4 to 5.2, fetch 11.7, materialise 0.45) | 107 MB | 66 | 18.8 s, 99 MB, 5,878 requests; git clone 9 s |
| Mac | Warm, one 2-file layer | **0.49 s** | 5.9 KB | 3 | adopt 12 s (18 KB) |
| Mac | Cold, full tree (one run, for completeness) | 300 s (fetch 290) | 2.23 GB | 688 | no baseline |

The Mac figure for the full tree is its Wi-Fi link (about 7.4 MB/s), not the store. The CI host moved the same bytes at about 100 MB/s. The cold join is 4 times faster on the CI host than from RustFS on PG, and 1.3 times faster on the Mac. It stays slower than a git clone on the Mac (9 s) for the base alone: a 1.4 to 5 s manifest fetch (8 MB gzipped, 30 MB parsed) is a fixed cost that a per-path or binary manifest would cut. A warm join costs three requests.

### Round trips to the bucket (30 samples, GCS XML API)

| From | TCP connect (median) | head/master GET, kept-alive (median / p95) | head/master GET, new connection | 4 KB range read on a 100 MB pack, kept-alive (median / p95) | 4 KB range read, new connection |
|---|---|---|---|---|---|
| The SC (same region) | 0.3 ms | 28 / 42 ms | 30 / 43 ms | 37 / 61 ms | 32 / 46 ms |
| CI host | 13.9 ms | 51 / 61 ms | 137 / 155 ms | 58 / 64 ms | 138 / 163 ms |
| Mac | 38 ms | 80 / 123 ms | 191 / 246 ms | 102 / 145 ms | 197 / 265 ms |

A new connection costs three round trips (TCP and TLS 1.3 and the request); keep-alive makes a head check or a range read one round trip plus about 25 ms of service time. The 4 KB range read costs about the same as a head fetch.

### Reader access

Neither the CI host nor the Mac has a bucket permission, and the design does not give them one. The SC signs short-lived V4 GET URLs with IAM `signBlob` as its own service account, for the head, the head's manifest and every pack the manifest names (688 URLs in 1.4 s, one hour). That needs `roles/iam.serviceAccountTokenCreator` for the SC account on itself, one Terraform resource (`google_service_account_iam_member.writer_signs_for_itself`, plan 1 add, 0 change, 0 destroy), applied on approval. After the binding propagated (the first signBlob calls were denied for under two minutes), the CI host ran both joins above using only the URL file (mode 0600, deleted afterwards), with no credentials of its own. The Mac joins used the owner's own gcloud application default credentials as an interim and needed no IAM change. Limits seen: a URL names one object, so a join that reads a newer manifest needs the SC to mint again (the warm join did); the head URL reads the live head but expires with the rest.

### Verification

The Mac's materialised tree (files read back from disk) hashes to git tree `d070bb36d22d94d0eb7a89458f3606626a726170`, and so does the CI host's. That is `refs/mesh/integrated/w4^{tree}` in the integrator on PG, the git tree of the master head. After each warm join the hash was recomputed over a tree that includes that run's two synthetic files (the Mac and the CI host used different synthetic layers, so those two hashes differ); it is not comparable to git.

### What this means for the design

- **The store works as a seed.** One writer publishes immutable content-named objects and moves the head by generation compare-and-swap; readers need nothing but signed URLs. The cold join beat the RustFS baseline on both clients and the warm join needs three requests.
- **Publish is bounded by its source here.** Copying from PG's disk ran at 21.6 MB/s; the same 2.2 GB from a local source would be limited by GCS write rates, which this run did not reach. Gate 5 (seal-and-publish from the SC's own disk) is still open.
- **Repack on publish pays.** The 5,878 small loose base blobs became 10 packs, so the Mac's base join read 66 objects instead of 5,878 requests.
- **Manifest size is the next cost.** Gzipped JSON of the full tree is 8 MB; deltas are 0.4 KB. Heads should be published as deltas over a cached parent, with the full manifest only for cold joins.
- **Not done in this run:** gates 7 and 8 (writer failover, tamper checks), billing (gate 9), and signature verification of the lineage on the reader side (the readers check blob hashes and the tree hash, not layer signatures).

## Spike results: gate 5 (2026-10-31)

The seed cone (SC) did the whole write path itself, with no PG in the loop: it took in six client layers, merged them, sealed a master layer, signed its manifest, published, and moved the head. **Ed25519 only, by the owner's choice for this gate.** Tool: `gcsmesh` in the mesh-mock crate (new `src/sc.rs`; changes in `src/gcs.rs`, `src/gcsflow.rs`, `src/bin/gcsmesh.rs`, `src/lib.rs`; scripts `scripts/gate5-*.{sh,py}`). The CI host copy of the crate was synced to match.

### Setup and rehydrate

The SC is x86-64 with git 2.43. The binary was built on the CI host and copied over (rustup was not installed on the SC; nothing there needs a compiler). Rehydrate is a cold join of the head's tracked files (dependency trees skipped) onto the Local SSD scratch, then a git base commit built from the materialised tree.

| Step | Time |
|---|---|
| Cold join, 5,914 files, 5,878 blobs, 107 MB, 66 GETs | **0.61 s** (0.87 s on the first run) |
| Git base commit | **1.97 s** (25.8 s on the first run, one `git hash-object` process per blob; now one batched `hash-object --stdin-paths` and one `update-index`) |
| Tree hash of the files on disk | 0.08 s |
| Total | **2.85 s** |

The integrator's git tree is `d070bb36d22d94d0eb7a89458f3606626a726170`, the tree the Mac and the CI host reached in gate 3, and equals the tree hash of the files on disk. The SC then signed the inherited w4 manifest (`manifest/085bfe31….sig`), because a reader that requires signatures needs one on every manifest in the chain, and the SC had just checked that manifest's tree against git.

### Signing key

An Ed25519 key was generated on the SC for the spike: `/mnt/scratch/gate5/sc-ed25519.key`, mode 0600, never printed or uploaded. The public key is at `keys/sc-ed25519.pub` in the bucket. **Fingerprint (SHA-256 of the raw public key): `2a59ea15017045b9dff2ab5902c6faf7d6eb24eba77559f2d7762cf764a24cf1`.** The production key belongs in Secret Manager or KMS (KMS can hold the Ed25519 half), not on the Local SSD, and its fingerprint is pinned in each reader's configuration. A reader that fetched the public key from the bucket and trusted it unpinned would verify nothing; the tool takes `--verify-pin <fingerprint>` and refuses a key that does not match.

A signature is a detached document at `manifest/<id>.sig`: `{alg, key_id, manifest, manifest_sha, sig}`, where the signed message is `weft-seed-manifest-v1`, the manifest id and the SHAKE-256 of the stored manifest bytes.

### The wave: six layers, two clients

Both clients joined the w4 head with the signature check on (Mac 6.8 s over Wi-Fi, CI host 3.3 s, base only), made real edits, and sealed one layer per task against `head/master`. Each layer is a signed `layer.json` (the layer plus its blob locations) and one pack of its new blobs. Edits (the Rust test and the doc comment were not compiled in this run):

| Client | Layer | Change |
|---|---|---|
| Mac | `4cd76bb1` | docs: new "Reading a rejection" section in `diff-gate.md` |
| Mac | `6773bdae` | test: same version record under two kinds stays apart (`clawft-versions/src/tests.rs`) |
| Mac | `0e77457b` | docs: `diff-gate.md`, the first-push sentence (contested line) |
| CI host | `b127398b` | docs: a doc comment on `embed` in `clawft-versions/src/mirror.rs` |
| CI host | `168092ef` | docs: `contributing.md` points at the local diff-gate run |
| CI host | `78600597` | docs: `diff-gate.md`, the fail-closed sentence (the same line as `0e77457b`) |

### Ingest

The SC minted 12 V4 PUT URLs (15 minutes) under `incoming/<client>/<layer>/{pack,layer.json}`, into 0600 files handed over ssh and shredded afterwards. The URLs sign `x-goog-if-generation-match: 0` as a header, so a holder can create the object once and can never overwrite it; the clients held no other bucket access. A URL does not bound the object's size (a signed `x-goog-content-length-range` header would); that is a hardening item.

| Client | Layers | Objects | Bytes | Wall time (3 layers in parallel) |
|---|---|---|---|---|
| Mac | 3 | 6 | 19,953 | 0.62 s (0.30 to 0.37 s each) |
| CI host | 3 | 6 | 35,684 | 0.38 s (0.33 to 0.37 s each) |

### Integrate, seal, sign, publish (on the SC)

The SC waited for six complete bundles (it polled `incoming/` every 250 ms; production would use a Pub/Sub event on `incoming/`), then:

| Phase | Result |
|---|---|
| Fetch 12 incoming objects | 97 ms, 55,637 B |
| Verify (layer signature against a client allowlist, parent, pack and blob hashes) | under 1 ms |
| Build six git commits | 170 ms |
| Merge, flat `(depth, name)` order, `git merge-tree --write-tree` | **26 ms**, 6 merge operations |
| Same six commits with plain `git merge` in a worktree | **372 ms** (plus 538 ms to create the worktree), same final tree |
| Conflict records (3 objects) | 195 ms |
| Seal (git diff, master layer, delta manifest) | 11 ms |
| Sign (Ed25519 over the manifest) | under 1 ms |
| Repack | 4 new blobs into 1 pack, under 1 ms |
| Publish 9 objects create-only (1 pack, 5 client layers, the master layer, manifest, signature) | 224 ms, 47,885 B |
| Head compare-and-swap (`ifGenerationMatch` = the generation read at the start) | 60 to 87 ms |
| Clean up `incoming/` | 12 deletes, 164 ms |
| Fetch to head moved (excluding the wait) | **906 ms** |

**Last client upload to new head visible: 1,097 ms**, from the object generations on both sides, which are GCS server timestamps in microseconds (no clock skew), and it includes the SC's 250 ms poll. **The temporary Pub/Sub subscription was not measured:** the Mac's gcloud session had expired (reauthentication needs a person), the CI host has a different account, and the SC's service account has no Pub/Sub permission (403; no IAM change was made). Gate 3 already showed one `OBJECT_FINALIZE` per head write.

**Conflict.** One layer was refused and recorded, not dropped. In name order `0e77457b` merged first; `78600597` changed the same line and `git merge-tree` reported a content conflict in `docs/development/diff-gate.md`. The SC wrote `conflicts/78600597…/record.json` (kind, client, message, both sides' hunk with conflict markers, git's messages, and the merged layers that touched the same file), plus the layer and its pack, so the owner can rebase it. `4cd76bb1`, which edits a different region of the same file, merged cleanly. Five layers merged. The plain `git merge` run also merged five and refused one, and produced the identical git tree `fc52a711ff4f070c0c63da87a6c7a1470cba8e2d`. The master layer has 4 operations (the four edited files), a 574-byte manifest and a 396-byte signature.

### Readers verify

The SC minted read URLs for the new head, its manifest, its signature and the public key. The CI host and the Mac warm-joined with `--verify-pin`:

| Client | Warm join of the new head | GETs | Bytes | Signature check |
|---|---|---|---|---|
| CI host | **0.50 s** | 5 | 43,513 | 63 ms |
| Mac | **0.54 s** | 5 | 43,513 | 86 ms |

**Tree hash.** Both clients' materialised trees (read back from disk) hash to git tree `fc52a711ff4f070c0c63da87a6c7a1470cba8e2d`, which is the SC integrator's merged tree.

**Negative tests (both clients).** A copy of the new manifest with one byte flipped was uploaded to `spike/gate5-neg/manifest/<id>`. Each reader fetched it with the real signature and rejected it ("manifest bytes do not match the signed hash"; exit code 3). On the real manifest each reader also ran four tamperings in memory, and all were rejected: a flipped manifest byte; a flipped manifest with the hash field in the signature rewritten to match (the Ed25519 check fails); a flipped signature byte; a signature by another key. The untouched pair verified. A wrong pin was refused before any manifest was read.

### Cost signals

Client side: 12 PUTs. SC: about 90 LIST calls while waiting at 250 ms (an estimate; this goes away with a Pub/Sub trigger), 2 LISTs for the fetch and cleanup, 12 GETs for the layers, 13 PUTs (3 conflict objects, 9 publish objects, the head) plus 12 DELETEs. Each reader: 5 GETs. At the published list prices that is far below a cent for the whole wave.

### What this means for the design

- **The SC can own the write path alone.** Seal, sign and publish from the Local SSD took under a second for a small wave, and the compare-and-swap is still the only commit point.
- **Rehydrate is cheap once the working set is batched:** 2.9 s from blank. Per-blob git processes are the cost to avoid (25.8 s).
- **Flat merge in `git merge-tree` beats plain git by about 14 times** on six layers, and refuses the same layer.
- **Ed25519-only caveats.** (1) The head document itself is not signed here, only the manifest. A hostile bucket could point `head/master` back at an older, validly signed manifest (a rollback) and a reader would accept it; the dual-signed head with sequence and epoch (this ADR, section 4) is what closes that, and the reader should check the head's `manifest_sha` against the manifest. (2) A single classical signature offers no post-quantum protection for long-lived history. **ML-DSA-65 dual signing, to come:** ML-DSA-65 is added next to the Ed25519 signature in the same `.sig` document and on the head, a reader requires both, and the key custody problem (KMS cannot hold ML-DSA-65) is solved by a wrapped key, not by KMS signing. The measured cost is two signatures of a few kilobytes per head, so size and time stay small.
- **Hardening still open:** size-bound the signed PUT URLs; readers do not yet verify the lineage's layer signatures (the SC verifies the clients' layers, readers verify manifests); the clients' public keys are an allowlist file on the SC, not yet in a registry.

### State left behind

`head/master` was restored to the w4 head (`085bfe31…`, sequence 7) after the tests; the wave's head `84d967fd…` and its manifest, signature and layers remain in the bucket and can be re-selected with `head-set`. The bucket also holds `keys/sc-ed25519.pub`, `manifest/085bfe31….sig`, `conflicts/78600597…/` and `spike/gate5-neg/`. On the SC, `/mnt/scratch/gate5/` (about 200 MB: binary, blob cache, tree, integrator repo, keys, client allowlist) is unchanged and is at the wave's head, so rehydrate again before the next run. No `incoming/` objects remain; every URL file was deleted.

## Spike results: Pub/Sub, signed head (2026-10-31)

Two gaps left by gate 5: the change-notification latency was never measured, and the head was unsigned, so a hostile bucket could roll it back to an older validly signed manifest. Code: `head.rs` (new), changes in `gcs.rs`, `gcsflow.rs`, `sc.rs`, `bin/gcsmesh.rs`, `lib.rs` in the mesh-mock crate; the CI host copy is in sync and the x86-64 binary on the SC was replaced (the gate 5 binary is kept beside it as `gcsmesh.g5`).

### Pub/Sub latency

A temporary pull subscription (`spike-pubsub-20261031`) on `weftos-seed-head` was read from the Mac with the owner's login, then deleted (the topic has no subscriptions again). Six events: one real integrate-and-publish cycle on the SC (a one-line docs layer from the Mac client, SC rehydrate 2.3 s, head CAS 84 ms) and five bare head compare-and-swap writes (the same bytes rewritten, a new generation each time). Times are from the object generation, which is the GCS server's microsecond timestamp.

| Interval | Median | Range |
|---|---|---|
| Head object finalize to message `publishTime` | **71 ms** | 62 to 93 ms |
| `publishTime` to received on the Mac | **688 ms** | 622 to 763 ms |
| Finalize to received on the Mac | **766 ms** | 687 to 826 ms |

- **Publishing is fast; delivery depends on the subscriber.** The 0.7 s on the Mac is a unary REST `pull` in a Python loop over Wi-Fi, which is the slow way to consume a subscription. A streaming pull (client library or `gcloud`) was not tried, so treat 0.77 s as an upper bound for a Mac and not as the floor.
- **Clock.** The Mac's clock and GCS agree to within about 35 ms (four of the five PUT responses arrived 33 to 36 ms after the generation stamp it returned), small next to the 0.7 s.
- **Not measured on the CI host.** It cannot use the owner's login, and the other option (a subscriber on the SC) needs a Pub/Sub subscriber role the SC's service account does not have; none was granted.
- **Compared with polling.** Gate 5's 250 ms `LIST` poll saw the last client upload 1.1 s before the new head was visible, end to end; a notification costs about 0.07 s to publish plus the subscriber's pull. For a reader that only needs "the head moved", a plain `HEAD`/`GET` of `head/master` every second is within the same range and needs no subscription.

### Signed head

`head/master` is still one object, still written by one generation compare-and-swap, now carrying `{manifest_id (the existing `head` field), manifest_sha, seq, prev_manifest_id, signer_key_id, issued_at}` and a detached Ed25519 signature over those fields (message: `weft-seed-head-v1`, then the fields NUL-separated). The signature is in the same object, so there is no window where the head and its signature disagree. `seq` is the old head's `seq` plus one, read in the same step as the CAS generation, so a lost race fails with 412 and never reuses a number.

A reader accepts a head only if all of these hold:

1. it is signed, by a key whose fingerprint the reader pinned locally (`--verify-pin`, comma-separated for more than one key); keys found under `keys/` in the bucket but not pinned are ignored, so writing a key into the bucket buys an attacker nothing;
2. the signature verifies;
3. `seq` is not lower than the last `seq` the reader accepted, which is kept in a per-reader state file (`--head-state`, written by rename); an equal `seq` is accepted only for the same manifest, so a fork at one number is refused too.

Refusals never touch the state file. The manifest signature check from gate 5 is unchanged, except that it now accepts any pinned key (the standby writer in gate 7 has its own key).

| Case (run on the Mac against the bucket and offline, and on the CI host offline) | Result |
|---|---|
| Valid head, seq 1, then seq 2 | accepted |
| **Rollback:** the seq 1 head, validly signed, published again over seq 2 | **refused** (seq 1 is lower than the last accepted 2) |
| Fork: seq 2 again with a different manifest, validly signed | refused |
| **Tampered**, one field changed after signing: manifest id, seq, prev, `issued_at`, `manifest_sha`, or one signature byte (six cases) | **refused** (signature does not verify) |
| **Unknown key:** a validly signed head from a key outside the trusted list | **refused** |
| Same key B claiming the trusted key's id | refused (signature does not verify) |
| Unsigned head (the old format) | refused |
| Valid seq 3 after all of the above, and the same head read twice | accepted (refusals left the state alone) |

All 15 cases came out as expected on each of the three runs. The Mac run against the bucket wrote its heads to `spike/head-test/master` by real compare-and-swap and deleted the object afterwards. Each case builds a new reader from the state file on disk, so the rollback refusal holds across process restarts.

**The real head.** `head/master` was re-published as a signed head at the current integrated head (`001acc49…`, seq 9, signed by the SC key, `prev` the same manifest because it was a `head-set`). The Mac read it with `head-verify` and with a verified `join` (440 ms, warm cache, manifest signature verified); the CI host read it with signed read URLs minted on the SC. Both accepted it. With a state file already at seq 10, both refused it as a rollback, which tests the persisted state against a real head. Pinning an unrelated fingerprint fails with "pinned key not found".

- **Still open.** The reader trusts what the bucket serves for the *latest* head, so an attacker who can only replay can hide new heads from a reader (a freeze): readers should also reject a head whose `issued_at` is older than a bound they choose. Not built. Readers also still do not check the head's `manifest_sha` against the manifest bytes in `join`; the manifest's own signature covers the same bytes, so a mismatch is caught there. A reader that has never seen the bucket (state file absent) accepts any signed head; the first seen `seq` should be pinned out of band when that matters.

## Spike results: gate 7 (2026-10-31)

Failover and rehydrate: the SC (the elected writer) was stopped with its Local SSD discarded, the standby on PG took the writer lease, rehydrated from GCS and published a signed wave, then the SC was started again, read its key back from Secret Manager, rehydrated onto a blank scratch and published the next wave. Code: `lease.rs` (new) and a `--verify-pin`/`--head-state` option on `rehydrate`, in the mesh-mock crate (both copies in sync). The failover ran on 2026-11-01 UTC.

### Setup

| Part | What |
|---|---|
| Lease | `lease/writer` = `{holder, expires_at, seq}`; taken and renewed only with a generation compare-and-swap; TTL 30 s, renewal every 10 s; `seq` is the term (it rises by one when the holder changes). Contenders poll once a second. |
| SC signing key | In Secret Manager (`weftos-seed-cone-signing-key`, accessor for the SC's service account only). Piped from the SC by the owner, never printed. |
| Standby identity | A service account with `objectAdmin` on the bucket and nothing else. **A service-account key, not Workload Identity Federation:** PG is not a GCP host and has no OIDC or x.509 identity provider, so federation would need a pool, a provider, a binding and a PG-side token source. The key was created with gcloud (not as a Terraform resource, so it never entered the state), held as a 0600 file under the weftos user on PG, exchanged for tokens with a JWT grant, and deleted at close. |
| Standby signing key | Its own Ed25519 key, generated on PG; the public key was published to `keys/`; readers pin both fingerprints (`--verify-pin SC,PG`). |
| Wave input | One real client layer per wave (a one-line docs edit sealed by the Mac), uploaded to `incoming/` through SC-minted create-only URLs before the SC was stopped; the second wave's layer was sealed against the standby's head. |

### Timeline (server timestamps of the lease and head objects; ms)

| Event | Time | Since the SC's last renewal |
|---|---|---|
| `gcloud compute instances stop` issued (first try failed: a VM with Local SSD needs `--discard-local-ssd=true`; the time here is the second try) | 660.9 s (Mac clock) | |
| SC's last lease renewal (the guest kept running during the stop) | 690.5 s | 0 |
| SC's lease expires | 720.5 s | 30.0 s |
| **Standby takes the lease (CAS, seq 2)** | 721.3 s | **30.7 s** |
| Standby has rehydrated | | 41.7 s |
| **Signed head seq 10 published by the standby** | 734.4 s | **43.9 s** |

- **Time to detect:** the lease has to run out, so detection is the TTL (30 s after the last renewal) plus up to one poll (1 s). The standby's own log was lost, so detection is inferred from the expiry time and the take time: the take landed 0.8 s after expiry, with the CAS itself taking about 0.1 s.
- **Time to take over:** 0.8 s from expiry to holding the lease (one poll plus one CAS). **From the stop command to a new head: 73.5 s**; from the last renewal, 43.9 s.
- **Standby rehydrate on PG, blank scratch:** **11.0 s** (cold join 3.7 s for 5,878 blobs and 107 MB in 73 GETs, with the head and both manifests verified; git base commit 6.9 s, slower than the SC's 2.1 s (not investigated); tree hash 0.3 s). Integrate, seal, sign, publish: 1.5 s; head CAS 121 ms. Trees matched.
- **SC restart:** VM started and reachable over the tailnet in **34 s**. The startup script then formatted the Local SSD (`mke2fs` with lazy initialisation off, about 23 s for 375 GB). **My run started before the SSD was mounted, so the first rehydrate ran on the boot disk: 3.5 s** (join 1.3 s, git base 2.1 s). **Re-run on the mounted Local SSD from blank: 3.3 s** (join 1.1 s, git base 2.1 s). The key was read from Secret Manager in 0.1 s. The lease had already expired (the standby had stopped), so the SC took it back at once (seq 3) rather than waiting; the signed head seq 11 was published 50.8 s after the start command. **Lesson for the production unit:** the writer must wait for the scratch mount marker before it starts; nothing in the startup script gates that today.
- **Readers.** The Mac accepted seq 10 (signed by the standby's key) and seq 11 (the SC's key) with both fingerprints pinned, and a verified join of seq 10 took 1.2 s. With only the SC's fingerprint pinned, seq 10 is refused ("signed by unknown key"), so adding a standby key is a deliberate reader-side step. Not repeated on the CI host, which can only read through SC-minted URLs and the SC was down at the time.

### No two writers

| Check | Result |
|---|---|
| Lease history (every version of `lease/writer` is kept) | seed-cone seq 1 (renewals every 10.0 s, each lasting 30 s), then pg-standby seq 2, then seed-cone seq 3. **No overlap:** seq 1 expired at 720.5 s, seq 2 started at 721.3 s; seq 2 expired at 761.3 s, seq 3 started at 863.6 s. |
| Head history | seq 8 and the five bare rewrites, 9 (SC), 10 (standby), 11 (SC), each with a distinct generation and a strictly rising `seq`, `prev` the one before. |
| Stale head write (a zombie holding the seq 10 generation, after the SC wrote seq 11) | **412 precondition failed**, head unchanged |
| Stale lease renewal (the standby's last generation, after the SC took the lease) | **412** |
| Lease create-only on an existing lease | **412** |
| Natural 412s during the run | none (the counters of both integrates read 0): the lease serialised the writers so they never raced |

The lease is what makes a handover orderly, and the generation compare-and-swap on `head/` is what makes it safe if the lease logic is wrong: a stale writer cannot move the head even while it believes it holds the lease. A writer that stalls past its TTL can still publish one wave that is stale relative to the new holder only if it reads the head and writes inside the same generation, which the CAS refuses.

### Things that went wrong, and what they cost

- The first `instances stop` was refused (no `--discard-local-ssd` flag). It cost 2.5 minutes with the SC still renewing; the timings above are from the second try.
- My standby script cleared its own log directory, so the standby's lease events (detect and take) were lost. They are rebuilt from the object versions, which is why the detect time is an inference. The lease code now prints these events; the script needs a log path outside the directory it clears.
- Putting the private key into Secret Manager and creating the service-account key were both stopped by the permission check and run by the owner.
- The stale-lease and stale-head tests were run from the Mac with the owner's login, as a stand-in for a zombie process.

### Close

The head was set back to the Part A head (`001acc49…`) by the SC with a signed seq 12 (the Mac verified it, accepting seq 12 after 11). The service-account key was disabled and deleted in GCP, PG's key file and all spike files there were shredded and removed, and the objects the gate created were deleted with all their versions. The Terraform block is removed by reverting the gate-7 commit; the destroy plan goes to main for the owner to approve and apply (it needs a fresh login; the saved one had expired when the plan was first tried).

## Spike results: the SC writer unit (2026-11-01)

Gate 7 found that the SC's first rehydrate after a stop ran before the startup script had mounted the Local SSD, so it wrote to the boot disk. The writer is now a systemd unit, `weftos-seed-sc.service`, installed on the SC's boot disk (`/etc/systemd/system`, scripts and the `gcsmesh` binary in `/opt/weftos-seed/bin`, configuration without secrets in `/etc/weftos-seed/sc.env`). The boot disk survives a stop and start; the Terraform startup script is unchanged and still formats and mounts the Local SSD.

| Step | What it does |
|---|---|
| Ordering | `After=google-startup-scripts.service network-online.target tailscaled.service`, `RequiresMountsFor=/mnt/scratch`. The startup script is a oneshot, so the unit starts only after the format, the mount and the fresh-format marker. |
| `ExecStartPre` 1: scratch check | `/mnt/scratch` must be a mount point, its source must resolve to `/dev/disk/by-id/google-local-nvme-ssd-0`, it must be ext4 and on a different device from `/`. Otherwise it exits 1 and systemd retries every 15 s (`Restart=on-failure`, no start limit). It never falls back to the boot disk. |
| `ExecStartPre` 2: keys | `gcloud secrets versions access latest` for `weftos-seed-cone-signing-key` into the unit's `RuntimeDirectory` (tmpfs, 0700), split by `gcsmesh key-unbundle`, the payload shredded. With `WEFT_REQUIRE_PQ=1` the unit refuses to start without the ML-DSA-65 key. |
| `ExecStartPre` 3: rehydrate | Only when the fresh-format marker exists or the SC state is missing: a verified cold join (pinned keys; `--require-pq` after the cutover), the git base, the tree check; then the marker is removed. The reader state (rollback, cutover) lives on the boot disk (`/var/lib/weftos-seed`), so a blank scratch does not reset it. |
| `ExecStart` | `gcsmesh lease-run --holder seed-cone --wait -- sc-loop`: take or renew `lease/writer`, then the integrate loop under it (LIST `incoming/` every 10 s, exit 0 after 600 s idle, rehydrate on any failure). Losing the lease kills the loop (exit 7) and systemd restarts the unit. |

Tests on the running SC (no VM stop or restart):

| Case | Result |
|---|---|
| First start on the scratch the gate 7 restart left blank (marker present) | running in **3.8 s**: rehydrate 3.0 s (join 0.84 s, 5,878 blobs in 65 packs, git base 1.98 s), trees match, lease taken |
| `systemctl restart` (no marker, state present) | **0.5 s** to running, no rehydrate, the same holder retakes the lease at once |
| Local SSD unmounted, then `systemctl start` | refused three times, 15 s apart ("not mounted; not running on the boot disk"); nothing written under the mount point; after the same `mount` as the startup script the next retry started the unit by itself |
| A tmpfs stacked on `/mnt/scratch` | refused ("is tmpfs, not the Local SSD"); started once the tmpfs was removed |
| Marker written by hand, then `systemctl restart` | rehydrated (3.1 s) and removed the marker |

**Found on the boot disk.** Hidden under the mount point, the gate 7 run left `/mnt/scratch/gate7/` on the boot disk: 223 MB, including `sc.key`, a 0600 copy of the SC's Ed25519 private key. The same key is in Secret Manager. With the owner's approval it was removed through a bind mount of `/`, without touching the Local SSD; `sc.key` was shredded.

**Terraform (plan only).** Changing `metadata_startup_script` **forces replacement** of the instance in the google provider: the plan that installs the unit from the startup script shows `must be replaced` (1 to add, 1 to destroy). Instance metadata keys alone are an in-place update. The change on the Terraform branch therefore carries the unit and its scripts as metadata keys and adds a short install block to the startup script (install the files, `daemon-reload`, `enable`, and a queued `start --no-block`, because a start from inside the script would wait on a job ordered after it), both behind `sc_unit_via_metadata`, default `false`. With the default the plan shows no changes. Turning it on is for an owner-approved replacement window; after that one replacement, later unit edits are in-place metadata updates. It was not applied.

## Spike results: ML-DSA-65 dual signing (2026-11-01)

Heads and manifest signatures now use weftos-pq-sign's `Ed25519MlDsa65V2` suite (ADR-129: RustCrypto `ml-dsa` 0.1.1, key file WPQK v2). The mesh mock uses a verbatim source copy of the crate with standalone versions in its `Cargo.toml`; nothing ML-DSA is hand-rolled.

**Keys.** The SC generated an ML-DSA-65 key (WPQK v2) with weftos-pq-sign; it was never printed. Its public key is at `keys/sc-mldsa65.pub` (hex, 3,905 bytes).

| Key | Fingerprint (SHA-256 of the raw public key) |
|---|---|
| SC Ed25519 (unchanged) | `2a59ea15017045b9dff2ab5902c6faf7d6eb24eba77559f2d7762cf764a24cf1` |
| SC ML-DSA-65 | `3ef7b6d17acffa0084d4f6b12a8da0ad9e5d5428e35eb91b7e2a099d895fbdf3` |

Both private keys go into one payload, a new version of the existing secret `weftos-seed-cone-signing-key` (no Terraform change). The bundle is three LF-terminated lines: `weft-sc-keys/v1`, `ed25519 <the 32-byte seed, hex>`, `ml-dsa-65 <the 1,992-byte WPQK v2 file, base64>` (2,756 bytes). `gcsmesh key-bundle` writes it 0600 and never over an existing file; `key-unbundle` also accepts the bare-hex version 1 payload. The owner wrote the bundle as **version 2** of the secret, piped from the SC as in gate 7. It was checked hash-equal to the pending bundle, and the pending copies on the SC's boot disk were then shredded. On restart the unit's key step loaded `weft-sc-keys/v1` with both fingerprints from the secret, took the lease and is renewing it. Until that point the unit had taken the ML-DSA-65 key from a 0600 copy on the boot disk.

**Format (version bump).**

- **Head v2.** The head document gains `chain` (the logical chain, `master`), `suite` (`ed25519+ml-dsa-65/v2`), `mldsa_key_id`, and `dsig`, the base64 of weftos-pq-sign's binary dual signature (`WDS`, suite tag byte, Ed25519 half, ML-DSA-65 half). The signed message is `weft-seed-head-v2`, then the suite, the chain, the manifest id, `manifest_sha`, `seq`, `prev_manifest_id`, both key ids and `issued_at`, NUL-separated. The suite name is in the signed bytes and the suite tag is in the signature, so neither can be changed without breaking a half. The chain binds a head to the store it moves: a test chain's heads can never be accepted as `master`. ML-DSA-65 context: `weftos/seed-head/v2`. Head object: 5.2 KB (v1: 0.6 KB).
- **Manifest signature v2.** `manifest/<id>.sig2` (the v1 `.sig` objects are create-only and stay): `{v: 2, suite, key_id, mldsa_key_id, manifest, manifest_sha, dsig}`, message `weft-seed-manifest-v2`, then the suite, id, hash and both key ids; context `weftos/seed-manifest/v2`. 4.9 KB each. The integrate path writes both `.sig` and `.sig2` for new manifests during the transition.

**Readers.** A reader pins both fingerprints, verifies both halves and accepts only `PqStatus::Verified` (an Ed25519-valid signature with the ML-DSA-65 half absent, unchecked or legacy is refused). It keeps the rollback and fork rules on `seq`. After the first v2 head it accepts, it records `pq_cutover` in its state file (as ADR-129 does for nodes), and from then on refuses a v1 head, a v2 head with a legacy suite name or tag or a missing half, and a v1-only manifest signature. `--require-pq` sets the cutover out of band. Two reader fixes came with this:

- the reader state is now written only after the whole join has verified (head, every manifest, every blob). Before, `check_head` recorded the head before the manifest was fetched, so a head whose manifest was missing or tampered still advanced the reader;
- the join checks that the head's signed `manifest_sha` matches the manifest bytes (open since the signed-head section).

**Cutover and the real head.** Under the writer lease, the SC ran a fresh verified rehydrate (both manifests of the chain checked against their v1 signatures, git tree equal to the tree on disk), then dual-signed both: `manifest/001acc49….sig2` and `manifest/085bfe31….sig2`. `head-set` then published **seq 13**, v2, at `001acc49…` (the same integrated head as seq 12).

| Reader | Result |
|---|---|
| The SC's unit, restarted with `WEFT_REQUIRE_PQ=1` and a forced rehydrate | accepted seq 13; both manifests `ed25519+ml-dsa-65/v2`; state: seq 13, cut over |
| Mac, `head-verify` with both pins, fresh state | accepted, version 2, `pq_cutover: true` |
| Mac, verified cold join, dependency trees skipped, fresh cache | **13.8 s** over Wi-Fi (fetch 11.7 s), 73 GETs; signature checks 128 and 165 ms; tree `95ee1f59…` = the SC integrator's git tree |
| Mac, only the Ed25519 key pinned | refused ("signed by unknown ML-DSA-65 key") |
| CI host, through 698 SC-minted read URLs (the chain walk now includes `.sig`, `.sig2`, layers and keys), fresh cache | accepted seq 13; cold join **3.8 s** (fetch 2.5 s); signature checks 184 and 256 ms; same tree. The URL file was shredded afterwards |

## Spike results: gate 8, tamper tests (2026-11-01)

Gate 8 ran on copies only. The SC built a parallel test chain under `spike/g8/`, never touching the real `head/`, `manifest/`, `layers/` or `packs/`: throwaway test keys (a dual key pair for the test writer and a second, unpinned pair for the forger), the chain name `g8`, and every case as a complete store under its own prefix (`spike/g8/<case>/`, through a key prefix in the tool). The chain: a full manifest (ten small files in one pack, a 600 KB blob as its own pack, a symlink) and two deltas, each with its lineage layer, `.sig` and `.sig2`; seq 1 Ed25519-only, seq 2 the first dual-signed head (the reader cuts over), seq 3 the valid next head. Each tamper case is the seq 3 store with one change. The reader ran every case with a fresh cache and a copy of the state the case needs (S2: after accepting seq 2; S3: after seq 3), with `--verify-lineage` on (each manifest's `layers/<id>` must hash to its id, carry the manifest's parent and ops, and be signed by a pinned key). A case passes only if the outcome, the reason and the state file all match: after every refusal the state file was byte-for-byte unchanged, so the head did not advance.

| Case | Expected | Mac | CI host (signed URLs only) |
|---|---|---|---|
| Control: valid chain at seq 2, reader cuts over | accepted | accepted | accepted |
| Control: a v1 head to a reader that has not cut over | accepted | accepted | accepted |
| Control: valid seq 3 | accepted | accepted | accepted |
| Pack byte flipped | refused: blob hash at materialise | "blob … fails its hash" | same |
| Pack index entry altered (a blob offset in the manifest's `locs`, the pack index of this layout; signatures kept) | refused: manifest hash | "manifest bytes do not match the signed hash" | same |
| Layer object altered (message rewritten, name kept) | refused: lineage | "content does not match its name" | same |
| Layer object swapped (the seq 2 layer, validly signed) | refused: lineage | "names a different layer (swapped)" | same |
| Manifest altered (a path), signature document kept | refused: manifest hash | "manifest bytes do not match the signed hash" | same |
| Manifest altered and the hash in `.sig2` rewritten to match | refused: signature | "dual signature: Ed25519 half does not verify" | same |
| Manifest signatures stripped (`.sig` and `.sig2` deleted) | refused | "has no ed25519+ml-dsa-65/v2 signature and this reader has cut over" | same |
| Head signature stripped | refused | "unsigned" | same |
| Downgrade: a validly Ed25519-signed v1 head after the cutover | refused | "downgrade, an Ed25519-only (v1) head after this reader cut over" | same |
| Downgrade: the head's suite field set to the legacy suite | refused | "suite … is not ed25519+ml-dsa-65/v2" | same |
| Downgrade: the suite tag inside the binary signature set to legacy | refused | "suite tag downgraded … inside the signature" | same |
| ML-DSA-65 half removed (Ed25519 half valid) | refused | "ML-DSA-65 half missing" | same |
| Downgrade: `.sig2` replaced by the manifest's valid v1 signature | refused | "downgrade: Ed25519-only manifest signature after the cutover" | same |
| ML-DSA-65 half taken from the seq 2 head (a valid signature by the same key over another message) | refused | "ML-DSA-65 half does not verify" | same |
| Key substituted: head and manifest signed by another dual key pair, its public keys in `keys/` but not pinned | refused | "signed by unknown key" | same |
| Key substituted in place: `keys/sc-*.pub` replaced by the other pair | refused | "pinned key … not found under keys/" | same |
| Replay: the validly signed seq 2 head served to a reader at seq 3 | refused | "rollback, seq 2 is lower than the last accepted 3" | same |
| Same seq, different manifest (validly signed, valid manifest) | refused | "seq 3 was already accepted for a different manifest" | same |
| Truncated pack | refused | "pack shorter than its location" | same |
| Truncated manifest | refused | "manifest bytes do not match the signed hash" | same |
| Truncated head | refused | "malformed head object" | same |
| Head pointing at a manifest that does not exist (validly signed) | refused, state not advanced | "manifest … not found" | same |
| Forged lease: `lease/writer` overwritten with no generation precondition, then the forger's head (its own dual key) written the same way | refused | "signed by unknown key" | same |

All 26 as expected on both. Per case 0.3 to 3.4 s on the Mac and 0.2 to 1.4 s on the CI host.

**Writer side of the forged lease (on the SC, under the case's prefix).** A holder renewing every 3 s (TTL 15 s); the forger's unconditional PUT of `lease/writer` succeeded, as it must for any principal with write access to the bucket. The holder's next renewal failed with **412**, it reported `lost` and exited 7 (the unit would restart and wait). Its stale compare-and-swap with its last generation was also refused with 412. The lease is not a security boundary: the forger's heads fail on signatures, and a stale writer cannot move the head (gate 7).

**A client's incoming layer trying to overwrite (both clients, SC-minted create-only PUT URLs).** First PUT 200; a second PUT with different bytes **412** `PreconditionFailed`; the same PUT without the signed `x-goog-if-generation-match` header **400** `MalformedSecurityHeader`. Each object kept a single version holding the original bytes, read back on the SC.

**Close.** Every `spike/g8/` object was deleted with all its versions (495, then 0). The test keys existed only in the builder's memory.

## Spike results: gate 9, cost (2026-11-01)

**Source.** The billing account has a BigQuery export (standard and resource-level tables). Actual spend comes from the resource-level table, filtered to the spike: the `adr=adr-130` label, the spike's resource names (the SC under both its names, the gate 1 test disks, the bucket), and the Cloud NAT rows in us-east1 (the NAT carries no labels). The project runs other services; they are excluded. The export lags: compute rows run to 05:00 UTC on 2026-11-01 and Cloud Storage rows to 17:00 UTC on 2026-10-31. Prices are from the Cloud Billing Catalog API (USD, read on 2026-11-01) and match the export's cost per unit; the Class B operation price is the published list price, not read from the catalog.

**Actual spend to date.**

| Item | Actual (export) |
|---|---|
| SC compute: C4 cores and RAM (about 41 h, under both names) | $5.22 |
| Titanium Local SSD | $2.13 |
| Boot disk, Hyperdisk Balanced 40 GB (capacity, IOPS, throughput) | $0.28 |
| Gate 1 temporary disks (Hyperdisk 100 GB, PD-SSD 100 GB, about 3 h) | $0.14 |
| Cloud NAT (gateway, NAT IP, 2.9 GiB processed) | $0.30 |
| Network Intelligence Center (console views) | $0.02 |
| Cloud Storage (374 Class A, 444 Class B, 1.03 GiB internet egress, storage) | $0.002 (egress and Class B inside the free tier) |
| Secret Manager, Pub/Sub, network transfer, Ubuntu licence | $0.002 |
| **Total in the export** | **$8.09** |
| Not yet exported: the SC from 05:00 to 17:00 UTC on 2026-11-01, at the run rate below ($0.295 per hour) | about $3.56 (estimate) |
| **Spend to date** | **about $11.65** |

**Monthly projection for the production layout (730 h).**

| Item | Basis (list price) | Standard | Spot |
|---|---|---|---|
| SC cores | 4 vCPU at $0.03465 (Spot $0.02074) per hour | $101.18 | $60.56 |
| SC RAM | 15 GiB at $0.003938 (Spot $0.002357) per GiB-hour | $43.12 | $25.81 |
| Titanium Local SSD | billed separately: 368 GiB at $0.16 (Spot $0.0958) per GiB-month | $58.87 | $35.25 |
| Boot disk | 40 GiB at $0.09, plus IOPS and throughput provisioned above the baseline | $7.66 | $7.66 |
| Cloud NAT | gateway $0.0014 per hour, NAT IP $0.005 per hour, $0.045 per GiB processed | $4.67 plus traffic | $4.67 |
| Writer while idle | LIST of `incoming/` and a lease renewal, each every 10 s: 518,400 Class A at $0.005 per 1,000 | $2.59 | $2.59 |
| Secret Manager | two active versions | $0.12 | $0.12 |
| Bucket storage | Standard, us-east1, $0.02 per GiB-month (the first 5 GiB of the billing account are free) | now 2.1 GiB live: $0.04 | |
| **Fixed total** | | **about $218** | **about $137** |

Against the estimate table: the VM is about $203, not $150 to $185, because the `-lssd` shape bills its Local SSD as its own SKU ($58.87, 29% of the VM). Spot saves 40% but loses the Local SSD on preemption and forces a failover to PG; it stays the option for a second, non-default writer.

**Growth of the bucket.** 10 GiB $0.20, 100 GiB $2.00, 1 TiB $20.48 per month, plus noncurrent versions for 30 days (now 0.30 GB). Storage stays small next to the VM at any size this store is likely to reach.

**Operations per integrate cycle** (counted in gate 5; L client layers): about 3L + 8 Class A (two create-only PUTs per client layer, the LIST that finds them, one PUT per pack, per merged layer, master layer, manifest and both signature documents, the head compare-and-swap, and three per conflict record) and 2L + 2 Class B. A six-layer wave is about 26 Class A and 14 Class B: $0.0001. Even 1,000 waves a day is about $4 a month. Deletes are free. A warm reader join is 4 to 6 Class B GETs.

**Egress per join** (internet, North America; the first 100 GiB a month on the billing account are free, then $0.12 per GiB to 1 TiB, $0.11 to 10 TiB, $0.08 beyond):

| Join | Bytes | |
|---|---|---|
| Warm (one new head) | about 15 KB plus the new content (head 5.2 KB, delta manifest, `.sig2` 4.9 KB) | |
| Cold, tracked files only | 107 MB (8 MB of it the full manifest) | |
| Cold, full tree with dependency trees | 2.23 GB | |

| Clients N, each per month | N = 10 | N = 100 | N = 1,000 |
|---|---|---|---|
| One cold tracked-files join plus 1,500 warm joins | 1.3 GB: free | 13 GB: free | 130 GB: about $2.50, plus about $3 of Class B |
| One cold full-tree join | 22 GB: free | 223 GB: about $13 | 2.2 TB: about $227 |

Egress matters only for full-tree cold joins at scale. Warm joins are cheap at any N here.

**Cost control.**

1. **The SC's uptime is the cost.** The VM, Local SSD and boot disk are 98% of the fixed total. The SC is stateless and rehydrates in about 3 s, so it can sleep: stop it when `incoming/` is quiet and start it on a Pub/Sub event, with PG holding the lease meanwhile. A stopped SC bills only its boot disk and the NAT IP, so 8 hours a weekday would be roughly a quarter of $203. A committed-use discount is the other lever if it must run 24/7 (check the current C4 rates).
2. **Boot disk:** set provisioned IOPS and throughput to the Hyperdisk Balanced baseline; about $4 a month of the $7.66 is provisioning above it.
3. **Replace the 10 s LIST poll with a Pub/Sub notification on `incoming/`** ($1.30 a month and lower latency). Lengthening the lease (60 s TTL, 20 s renewal) halves the renewal cost. Versioning keeps every renewal as a noncurrent version for 30 days; a lifecycle rule on the `lease/` prefix (keep a few newer versions) keeps the listings clean.
4. **Keep dependency trees out of cold joins from GCS:** they are 95% of a full-tree join. Use package mirrors or the L1 RustFS tier on the LAN, and publish tracked files only, which already make up the default join.
5. **A billing budget with alerts on the `adr=adr-130` label** (an owner step). The labels hold on the VM, bucket, disks and secret; label the NAT and router too, so the spike's cost can be read without name filters.

## Cleanup (2026-11-01, owner-approved)

The objects left under `spike/` from gates 1 to 5, with all their versions. The owner approved the deletion, and all 77 versions were removed; `spike/` is empty.

| Prefix | Names | Versions | Bytes per version | State |
|---|---|---|---|---|
| `spike/gate2-1k-1` … `-10` | 10 | 20 | 1,024 | noncurrent only (2026-10-31 06:47-06:48) |
| `spike/gate2-cond-1` … `-10` | 10 | 20 | 1,024 | noncurrent only |
| `spike/gate2-8m-1` … `-10` | 10 | 20 | 8,388,608 | noncurrent only |
| `spike/gate2-par-1` … `-8` | 8 | 16 | 8,388,608 | noncurrent only |
| `spike/gate5-neg/manifest/84d967fd…` | 1 | 1 | 574 | live (the gate 5 tampered manifest) |
| **Total** | **39** | **77** | **302,031,422 bytes** | |

Command used (all versions): `gcloud storage rm --all-versions 'gs://weavelogic-weftos-seed/spike/gate2-**' 'gs://weavelogic-weftos-seed/spike/gate5-neg/**'`.

## Production build (2026-11-01, accepted)

The owner accepted this ADR on 2026-11-01 with three decisions: the SC runs as a **Spot** instance, and it gets every gate 9 cost control. Terraform: the `gcp/seed-store` root on the Terraform branch. Code: `wake.rs` and `bin/seedwaker.rs` (new), plus changes in `lease.rs`, `sc.rs` and `bin/gcsmesh.rs`, in the mesh-mock crate (both copies in sync).

### The wake path

A stopped SC cannot mint upload URLs, so something else has to start it. Three options were weighed:

| Option | Verdict |
|---|---|
| **A. A Cloud Run waker** whose service account can only start this one VM | **Chosen.** No credential on clients, least privilege, about $0 a month |
| B. PG holds a start-only credential and wakes the SC over the tailnet | Rejected: PG has no GCP identity, so this means a service-account key on PG (what the gate 7 close removed) or a WIF pool plus a token source on PG |
| C. A scheduler | Rejected as the primary path: wake latency in hours. It could be added later as a weekday pre-warm |

**Waker.** One image (`seedwaker`, distroless, in Artifact Registry repo `weftos-seed`) runs as two Cloud Run services (max one instance, min zero):

- `weftos-seed-waker` is public and serves `POST /wake`. The client signs `weft-seed-wake-v1 \0 <client> \0 <unix seconds>` with the Ed25519 seal key it already has. The waker accepts only keys on its allowlist (the same public-key map as the SC's `clients.json`, from the Terraform variable `waker_clients`) and refuses any clock skew over 120 s. Clients hold no GCP credential. A stolen client key can keep the SC awake, at most the Spot hourly rate; it cannot read, write or stop anything.
- `weftos-seed-waker-push` is IAM-only. It is the endpoint of a push subscription on `weftos-seed-incoming`, the bucket notification for `OBJECT_FINALIZE` under `incoming/`. Cloud Run checks the push OIDC token, which is minted as the waker's account. An upload that lands while the SC is stopped or preempted starts it (a client's minted URLs stay valid for 15 min). While the VM is still stopping, the waker answers 503 and Pub/Sub retries with a 10 to 60 s backoff.
- **The waker's service account** has a custom role holding only `compute.instances.get` and `compute.instances.start`, bound on the SC instance only. `instances.start` needed no `actAs` on the SC's account. A second start inside 10 s answers "start already requested".

**Idle-stop.** `sc-loop` exits 0 after `WEFT_IDLE_EXIT_S` (900 s) with nothing incoming. Then:

1. `lease-run --release-on-exit` releases the lease: a CAS write that sets `expires_at` to now.
2. The unit's `ExecStopPost` (`sc-idle-stop`) powers the VM off, but only after a clean idle exit (the loop's marker plus exit code 0). A manual stop, a crash or a lost lease leaves the VM running.

A guest poweroff leaves the instance TERMINATED, so the SC needs no IAM grant to stop itself.

**Spot.** The settings are `provisioning_model = SPOT`, `instance_termination_action = STOP`, `preemptible = true`, `automatic_restart = false` and `on_host_maintenance = TERMINATE`. A preemption stops the VM and keeps it: the boot disk with the unit and the reader state survives, the Local SSD does not. The SC rehydrates on the next start.

### Cost controls: status

| Control | Status |
|---|---|
| Spot SC | Built and running |
| Idle-stop plus waker | Built; measured below |
| Pub/Sub trigger in place of the 10 s LIST poll | Built. Pull subscription `weftos-seed-incoming-sc`; the SC's account has `roles/pubsub.subscriber` on that subscription only. A LIST of `incoming/` every 300 s is the fallback for a missed message |
| Boot disk at the Hyperdisk Balanced baseline | Built: 3,000 IOPS and 140 MiB/s (was 3,240 and 200) |
| Dependency trees out of cold joins | Built. `gcsmesh join` now skips git-ignored trees (`node_modules`, `.cargo-home`, `target` and so on) by default; `--with-deps` fetches them |
| Unit installed by Terraform | Built. `sc_unit_via_metadata` is on: the startup script installs the unit, its scripts, `sc.env`, `clients.json` and the `gcsmesh` binary (from the bucket, refused unless its SHA-256 matches). Later edits are in-place metadata updates |
| Budget alert on `adr=adr-130` (50/90/100% of $150 a month) | **Pending.** The resource is in Terraform, but the billing budgets API rejects user credentials with no quota project. It waits for the owner to set the ADC quota project, then a budget-only plan for approval |

### How the apply went

All four of the replacement's changes forced a new VM on their own: Spot, the startup script, and the boot disk IOPS and throughput. So they went in one window.

- **Before the apply:** the binary was uploaded create-only to `bin/` in the bucket, the Artifact Registry repo was created on its own, and the image was pushed.
- **The full apply was blocked by the agent permission check;** the owner's session ran it. It took three plans:
  - **The first** failed at the budget (no ADC quota project), after the old VM had already been destroyed.
  - **The second** (without the budget) failed creating the VM: the Terraform set `preemptible = false`, which the API refuses with `SPOT`. The fix makes `preemptible` follow the provisioning model.
  - **The third** created the Spot VM, both Cloud Run services, their invokers, the instance-scoped start binding and the push subscription.
  - **The cost:** the writer was down from the destroy until the new VM came up. The head stayed at seq 13 in GCS and readers were unaffected.
- **Tailscale.** The new VM could not join the tailnet: the auth key in Secret Manager was single-use (`invalid key ... not valid` in the serial log, repeated). The writer did not care, because it needs no tailnet. The unit started, loaded its keys, rehydrated and took the lease. The owner added a reusable key as a new version; the next start joined with it.
  - The old node, logged out but still listed, kept the hostname, so the SC is now `weftos-seed-cone-1` on the tailnet.
- **Drift.** After the apply, Cloud Run reported a service-level `scaling` block the config does not declare. It is now ignored, and the plan shows no changes.

### Verification (2026-11-01 UTC)

| Check | Result |
|---|---|
| Spot | `provisioningModel SPOT`, `instanceTerminationAction STOP`, `onHostMaintenance TERMINATE` |
| Unit and keys | `scratch ok` (Local SSD, ext4, fresh-format marker). `sc-keys` loaded the `weft-sc-keys/v1` bundle from Secret Manager with both fingerprints (Ed25519 `2a59ea15…`, ML-DSA-65 `3ef7b6d1…`). Rehydrate 3.1 s, trees match, lease taken. The installed `gcsmesh` matches the pinned SHA-256 |
| Pub/Sub trigger (SC running) | A one-line docs layer from the Mac (uploaded with the owner's login in place of SC-minted URLs; the tailnet was down). The SC's pull got the message 0.79 s after the upload finished; head seq 14 was published 0.62 s later, **1.41 s from the end of the upload**. The push waker saw the same two objects and answered "RUNNING, none". The Mac verified seq 14 (dual-signed, `--require-pq`) |
| Idle-stop | The last integrate finished at 17:50:49. The idle exit came after 900.5 s (18:05:50.5); the lease was released by CAS 0.24 s later (its `expires_at` equals the release time). The poweroff was queued at once, the guest journal stopped at 18:06:26, and the instance was **TERMINATED by 18:06:41: 51 s from the release**. The reader state on the boot disk (`last_seq` 14, `pq_cutover`) survived to the next boot. A shortened `WEFT_IDLE_EXIT_S` run was not needed, because the production 900 s ran to completion |
| `/wake`, signed, SC stopped | Timeline below: **72.2 s from the wake request to head seq 15 published** |
| Preemption (a stop as preemption, Local SSD discarded, lease not released) | Upload of a layer to the stopped SC (0.39 s), then the push waker started it 3.3 s after the upload ("TERMINATED, started"; a second delivery got "start already requested"). VM RUNNING at +14 s, unit started at +72 s (after boot, mkfs 21.7 s and the startup script), lease taken at +72 s, **head seq 16 published 71.6 s after the upload**. The old lease had expired on its own; the SC retook it with no conflict |

**Wake timeline (from the `/wake` request; GCS server time for the head):**

| Step | Time |
|---|---|
| Waker answers 202 "started" (cold Cloud Run instance, signature check, `instances.get`, `instances.start`) | 2.0 s |
| VM RUNNING | 15.9 s |
| Startup script: `mke2fs` of the 375 GB Local SSD (lazy init off) | 21 s of the boot |
| On the tailnet (the startup script's `tailscale up`; first join with the new key) | 66.8 s |
| Unit started (scratch check, keys 0.4 s, rehydrate 3.1 s) | 70.5 s |
| Lease taken | 70.6 s |
| Head seq 15 published (the layer was waiting in `incoming/`; the first LIST found it) | **72.2 s** |

The unit is ordered after the whole startup script, so the tailnet join and the Local SSD format are on the wake path. Taking the scratch format out of it (lazy init on, or the unit ordered on the mount alone) would save about 20 s. Neither was changed here.

### Cost re-estimate: Spot with idle-stop

List prices as in gate 9 (Cloud Billing Catalog, 2026-11-01). Spot while running: cores $0.0830, RAM $0.0354 and Local SSD $0.0483 per hour, so **$0.167 per hour**. The parts that bill whether the SC runs or not: the boot disk at the baseline ($3.60), Cloud NAT gateway and IP ($4.67, counted in full), Secret Manager ($0.12). Cloud Run, Pub/Sub and Artifact Registry stay within free tiers. That is **about $8.40 a month fixed**. The lease renewals and the 300 s LIST fallback cost only while the SC runs: cents.

| Duty cycle | SC hours a month | Monthly |
|---|---|---|
| Always on (Spot) | 730 | about $130 |
| 8 h every weekday | 176 | about $38 |
| 2 h a day | 61 | about $19 |
| 10 wakes a day, nothing else (each at least about 1.5 min to start plus the 15 min idle tail) | about 83 | about $22 |

Against $218 for a standard SC running 24/7 (gate 9). Each wake costs at least about $0.05: the idle tail dominates. Shortening `WEFT_IDLE_EXIT_S` trades that against more cold starts of about 72 s. Egress is unchanged from gate 9, and smaller in practice now that joins skip dependency trees by default.

### Still open

- The budget (above): owner step, then a budget-only plan.
- The waker image is referenced by tag (`seedwaker:v1`); Cloud Run resolved it to digest `sha256:91b81585…` at deploy. Pinning it by digest in the Terraform is a small in-place update.
- A wake on Spot can fail when the zone has no Spot capacity. The waker then answers 500, and Pub/Sub retries pushes. Falling back to a standard VM is an owner decision and is not automatic.
- Option A+ (the waker also mints create-only PUT URLs for `incoming/<client>/`, so a client never waits for the SC) is deferred: it would give an internet-facing service write access to the bucket.

## Consequences

- The writer holds no durable state: losing the VM, its disk or its zone costs rehydration time, not data.
- A new external dependency (GCP) exists only at L2; L0 and L1 keep working with no cloud.
- Reads leave the home network for cold joins. Egress cost is the unknown to measure.
- Trust moves entirely to signatures: a compromised bucket can withhold or delay, not forge.
- PG's role narrows to seed, standby writer and replica, which also removes its HDD from the publish path.
- Open: reader auth (signed URLs first, then WIF), KMS custody, per-client encryption policy, garbage collection of unreachable objects, and Bucket Lock.
