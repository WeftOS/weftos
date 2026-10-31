# ADR-130: GCS seed store: integrated master layers published one way to a bucket, one elected writer on a Compute Engine VM, PG kept as seed and standby

- **Status**: Proposed (2026-10-30; owner direction, see Context). Adoption is gated on the spike in "Spike plan".
- **Date**: 2026-10-30
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
- There is **one elected writer**, a **Compute Engine VM** with a fast disk that "acts as PG", next to the bucket.
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

1. Integrate the incoming layers on the writer's state disk; seal; dual-sign (Ed25519 + ML-DSA-65, ADR-129).
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

### 10. The writer VM

| Item | Choice | Reason |
|---|---|---|
| Machine | `n2-standard-4`, standard (not spot), us-east1-b | e2 does not support local SSD; n2 does. A spot VM could be preempted mid-publish and lose the lease |
| Durable state | 200 GB PD-SSD (`hyperdisk-balanced` selectable) | Survives VM stop and rebuild; this is the integrated state the writer works on |
| Scratch | 1 local NVMe SSD (375 GB) | Fast sync target for the ring and pack assembly; lost on stop, so nothing durable lives there |
| Image | Ubuntu 24.04 LTS | Matches the CI host's family and the WeftOS release targets |
| Network | Own VPC, private subnet, **no external IP**, egress through Cloud NAT, a deny-all ingress rule, no allow rules | No public inbound ports. Tailscale needs outbound only |
| Access | Joins Tailscale at boot with the key from Secret Manager; OS Login on | Reach it over the tailnet; no SSH from the internet |
| Hardening | Shielded VM (secure boot, vTPM, integrity monitoring) | Cheap and standard |

Everything carries labels `project=weftos`, `purpose=seed-spike`, `adr=adr-130`.

### 11. Bucket location

The Terraform defaults to **single-region us-east1**: cheapest, lowest latency to Orlando and Miami, and the spike's data is rebuildable from PG and git. `dual_region = true` switches to a custom dual-region with `US-EAST1` and `US-EAST4` (location `US` with a custom placement config). Choose dual-region before any data that cannot be rebuilt goes in; changing a bucket's location later means a new bucket and a copy.

## Cost estimate (us-east1, list prices, monthly, estimates)

These are estimates from public list prices as known on 2026-10-30. They were not re-read from the live pricing pages during this work; check each before relying on them. Sources: Compute Engine pricing (`cloud.google.com/compute/all-pricing`), Cloud Storage pricing (`cloud.google.com/storage/pricing`), Cloud NAT pricing (`cloud.google.com/nat/pricing`), Pub/Sub pricing (`cloud.google.com/pubsub/pricing`), Secret Manager pricing (`cloud.google.com/secret-manager/pricing`). Sustained-use discounts on n2 are ignored (they lower the VM line).

| Item | Basis | Estimate |
|---|---|---|
| `n2-standard-4` VM, 730 h | about $0.19 per hour | about $142 |
| 200 GB PD-SSD | about $0.17 per GB-month | about $34 |
| 1 local SSD, 375 GB | about $0.08 per GB-month | about $30 |
| 20 GB boot disk (pd-balanced) | about $0.10 per GB-month | about $2 |
| Cloud NAT gateway | per-VM hourly plus about $0.045 per GB processed | about $1 to $5 |
| GCS Standard, single region | about $0.02 per GB-month; 100 GB assumed | about $2 |
| GCS Standard, dual-region (option) | roughly twice single-region, plus inter-region replication | about $4 plus replication |
| GCS operations | Class A about $0.005 per 1,000 | under $1 for the spike |
| Internet egress from GCS | about $0.12 per GB after the free tier | the large unknown: about $12 per 100 GB pulled |
| Pub/Sub, Secret Manager | within free tiers or cents | under $1 |
| **Total, fixed** | | **about $210 to $215 per month** |

The VM dominates. A `n2-standard-2` would roughly halve that line if the spike shows the writer is not CPU-bound. The spike records actual cost (gate 8).

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
| 3 | fsync on PD-SSD against local NVMe | Median and p99 small-write fsync, reported side by side (PG's HDD is the 26 to 33 ms baseline) |
| 4 | Round-trip time from the Mac, the CI host and PG to the bucket and to the VM | Median and p99 for a head fetch and a 4 KB range read |
| 5 | Publish throughput | Seal-and-publish of the same tree as the RustFS baseline (CI host 19 to 45 s) |
| 6 | Cold and warm joins against the RustFS and git baselines | Cold fetch of the base against 18.8 s (Mac) and 14.4 s (CI host) from RustFS; warm join against 2 to 4 s |
| 7 | Kill the writer; PG takes the lease | PG holds epoch + 1 within the lease expiry; the old writer's late head update fails with 412 |
| 8 | Tamper with an object; verification fails | A flipped byte in a pack, a manifest and a head are each rejected by a reader, and the head does not advance |
| 9 | Record the actual cost | Billing for the spike period against the estimate table |

Kill criteria: gate 7 or 8 failing is a design failure, not a tuning problem. Gates 3 to 6 failing against the baselines means the tier adds cost without a speed benefit and the spike stops.

## Consequences

- A new external dependency (GCP) exists only at L2; L0 and L1 keep working with no cloud.
- Reads leave the home network for cold joins. Egress cost is the unknown to measure.
- Trust moves entirely to signatures: a compromised bucket can withhold or delay, not forge.
- PG's role narrows to seed, standby writer and replica, which also removes its HDD from the publish path.
- Open: reader auth (signed URLs first, then WIF), KMS custody, per-client encryption policy, garbage collection of unreachable objects, and Bucket Lock.
