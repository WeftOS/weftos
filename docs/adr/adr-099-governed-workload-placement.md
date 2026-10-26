# ADR-099: Governed workload placement across the mesh

- **Status**: Accepted (2026-09-29; decisions settled 2026-09-29, implementation tracked on cards mesh-placement-01..23)
- **Date**: 2026-09-28 (rewritten same day; supersedes the earlier draft titled "Cog workloads and mesh placement", which is now ADR-100 plus this ADR)
- **Deciders**: Platform / ops. Open questions settled 2026-09-29 (defaults accepted by user); status stays Proposed until implemented, but the decisions below are settled pending implementation.
- **Depends-On**: ADR-022 (mandatory ExoChain audit), ADR-024 (Noise), ADR-025 (Ed25519 node identity), ADR-031 (rvf-wire mesh format), ADR-033 (three-branch governance), ADR-092 (governance rule distribution), ADR-094 (spawn permission)
- **Relates-To**: ADR-100 (cog workload kind), ADR-101 (inference workload kind), ADR-060 / ADR-018 (local model serving, which ADR-101 migrates), ADR-044 (wasip2), `~/llm` ADR-0004 (open capability vocabulary), 0016 (local serving), 0022 (storage tiers)
- **Amends**: none. Fills the reserved `SpawnBackend::Remote / Container / Wasm` slots (`crates/clawft-kernel/src/supervisor.rs:298-322`). No earlier ADR covers workload placement or remote backends (searched `docs/adr`, `docs/architecture`, `.planning`; `docs/architecture/swarm-topology.md` covers claude-flow prompt roles and the flat `SwarmCoordinator`, not node placement).

## Context

Three requirements arrived together:

1. Run Cognitum "cogs" (native ARM binaries) as governed, loadable workloads placed on the right node: a Mac for development, preferably a Cognitum Seed/Zero, Pi 5, or ARM server added to the mesh.
2. "The same for NPU workloads, or TPU, or even a TSU if we had one": accelerators are first-class placement targets.
3. "This really should be the same way local inference is hosted long term": model servers (today `llama-server` on :8090, Ollama on :11434, controlled from `~/llm`) become placed workloads.

Placement therefore cannot be a cog feature. It is a general layer; cogs and inference servers are two **workload kinds** on it, and future accelerator jobs are more. This ADR defines the layer. Kind-specific decisions are in ADR-100 (cogs) and ADR-101 (inference).

### What the WeftOS mesh gives us today (verified in source)

| Need | What exists | Gap |
|---|---|---|
| Node identity | `NodeIdentity`: Ed25519, `node_id = hex(SHA-256(pubkey)[..16])` (`cluster.rs:305-352`, ADR-025). `NodeRegistry` maps node id to pubkey and holds derived write grants (`node_registry.rs:96-131`). | none |
| Join / trust | Noise-encrypted mesh; `WeftHandshake` carries `node_id`, `governance_genesis_hash`, `governance_version`, a `u32` capability bitmap, `chain_seq` (`mesh.rs:124-146`). Operator pairing window / `PairingGate` (`cluster.rs:1094`, `:1168`). `RevocationList` is a persisted host ban list checked in `add_peer_checked` (`cluster.rs:864-878`, `revocation.rs`). | Revocation bans hosts only: no package, signer, or artifact revocation. |
| Discovery | `DiscoverySource` SeedPeer / Mdns / Kademlia / PeerExchange / Manual (`mesh_discovery.rs:37-50`); `MeshPeerEvent` feeds `ClusterMembership` (`mesh_discovery.rs:58`, `cluster.rs:896`). | none |
| Capability advertisement | `PeerNode { platform, state, address, capabilities: Vec<String>, labels }` (`cluster.rs:87-117`). `NodePlatform` is `CloudNative / Edge / Browser / Wasi / Custom` (`cluster.rs:29-40`): no CPU arch, OS, runtime, accelerator, memory, or feed. Capability tokens are a **closed** allow-list, Ed25519-signed (`capability_claim.rs:23-38`, `:109-198`). `labels` say "for scheduling" but are unsigned and unused. `NodeEccCapability` (`cluster.rs:281`) is tick timing only. Only one place reads host arch (`tools_extended.rs:716`). A repo grep for Metal/CoreML/CUDA/NPU/Hailo/Coral in `clawft-kernel` and `clawft-types` finds nothing. | No hardware or accelerator advertisement; vocabulary is closed. |
| Remote RPC | `MeshIpcEnvelope` carries a `KernelMessage` (hop count, dedup id, JSON default) (`mesh_ipc.rs:40-100`); `MessageTarget::RemoteNode` routing (`mesh_runtime.rs:326-345`, `:458-477`); correlated `MeshRequest` (`mesh_ipc.rs:224`). | Message plane exists; no workload-control message set. |
| Remote spawn | `SpawnBackend::Remote { node_id }` returns `BackendNotAvailable` "requires K6"; `Container` and `Wasm` likewise (`supervisor.rs:661-682`). | Unimplemented. |
| Health | SWIM-style `HeartbeatTracker` Alive / Suspect / Dead (`mesh_heartbeat.rs:216-227`, `:299-380`). | Node-level only; no workload health, restart, or reschedule. |
| Artifact transfer | `ArtifactStore` (BLAKE3, `artifact_store.rs`); frame types 0x0B / 0x0C (`mesh_framing.rs:51-54`); `ArtifactExchange` request / response / announcement types (`mesh_artifact.rs:17-63`). | `ArtifactExchange` is types plus a test catalog; its doc says real integration is not done (`mesh_artifact.rs:58-62`); no other crate uses it. One `Vec<u8>` per response: unusable for multi-GB model weights. |
| Placement / scheduling | `ConsistentHashRing`, `DistributedProcessTable`, `ProcessAdvertisement`, `ResourceSummary`, gossip / consensus scaffolding (`mesh_process.rs:22-345`); `ClusterServiceRegistry` (`mesh_service_adv.rs:16-60`). | Defined and re-exported (`lib.rs`, `weftos/src/lib.rs`) but referenced by no other crate: no placement decision exists. `ResourceSummary` is usage, not capacity. |
| App lifecycle | `AppManifest` (`app.rs:41-79`); install gated with action `app.install` (`app.rs:627`); chain kinds `app.install` / `app.start` (`chain.rs:459-465`); `weaver app` CLI calls `app.install/start/stop/remove/list/inspect` (`clawft-weave/src/commands/app_cmd.rs:54-120`). | Daemon `dispatch` (`clawft-weave/src/daemon.rs:5863-6380`; has `cluster.*`, `node.*`, `agent.register`) contains no `app.*` arm; repo-wide grep finds `"app.install"` only in the CLI, `app.rs`, `chain.rs`. Needs confirmation by running a daemon. |
| Container backend | `ContainerManager::start_container` is simulated (`container.rs:383-405`). | No real runtime. |
| Governance | `GateBackend::check(agent_id, action, context)`; `CapabilityGate` permits unknown actions (`gate.rs:111-135`); `GovernanceGate` over `EffectVector` (`gate.rs:297-420`); rule distribution per ADR-092. | New `workload.*` actions would be **permitted** by default; need explicit default-deny. |
| Signed installs | none: install checks the gate only. | Add signature and hash verification. |
| Existing service model | The local LLM is already a kernel service `llm` with lifecycle flag, health check, and a chain-anchored contract (`clawft-weave/src/llm_service.rs`, header). | Single-node, points at a hardcoded URL. |

Summary: identity, encrypted join, discovery, node health, message routing, content-addressed storage, governance and audit exist. **Hardware and accelerator advertisement, an open vocabulary, a placement decision, remote workload control, real container and native runtimes, signature verification, large-artifact transfer, and (probably) the daemon `app.*` handlers do not.**

## Decision

Introduce a **governed workload placement layer** with five parts: a generic workload model, an open capability vocabulary with node advertisement, a placement engine, a runtime-adapter trait, and a mesh lifecycle. Every placement, install, load, start, unload, migration, and refusal is gated by governance and written to ExoChain.

### 1. Workload model

A **workload** is `(kind, spec, package/manifest, config, requirements, policy)`:

- `kind` is an open string (`cog`, `inference`, later `accelerator-job`, `wasm-module`). Each kind is defined in its own ADR and supplies: how a spec becomes requirements, which adapters may run it, what "healthy" means, and its stable-address behavior.
- `requirements` are expressed only in the capability vocabulary (section 2). The placement engine never contains kind-specific logic.
- The payload is content-addressed in `ArtifactStore`: a **manifest artifact** (small, signed) that lists content hashes of the actual payload files or shards. Small payloads (cog binaries) are fetched whole; large ones (model weights) are addressed by shard and may be **adopted in place** (section 6).
- Instance identity: `instance_id = (manifest hash, config hash, node id)`. Secrets are delivered over Noise at place time and never written to chain payloads (hash only).

A workload is not an `AppManifest` agent. An optional wrapper lets `weaver app list` show workloads, but the manifest artifact is the source of truth.

### 2. Open capability vocabulary and node advertisement

**Capabilities are not an enum.** A capability is a dotted, lowercase id plus attributes:

```
Capability { id: "accel.gpu.metal", attrs: {vendor, device, sdk, sdk_version, mem_bytes, mem_free_bytes,
                                            unified, formats[], precisions[], ...},
             provenance: claimed | probed | measured,
             state: available | busy | reserved | degraded,
             exclusive: bool }
Requirement { id | id_prefix, where: [eq|gte|lte|in|has on attrs], count, exclusive }
```

Rules that make it extensible without a kernel change:

1. A node advertises whatever ids its probes and adapters produce. A requirement matches a node iff the node advertises that exact id (or an id under the requested prefix) and all predicates pass. Unknown ids are carried, stored, and matched; the placer does not need to understand them.
2. A **vocabulary file** (`config/capabilities.toml`) lists well-known ids and their attribute schemas. It drives validation warnings, `weaver workload explain` output, and docs. It **does not gate**: an id not in the file is accepted. Experimental ids use the `x.` prefix.
3. The signed allow-list in `capability_claim.rs:23-38` stays for the coarse existing tokens. Node facts are a new signed block in the same format. The closed list is not extended per accelerator.
4. `provenance` is honest: `probed` means a local probe saw it; `measured` means a conformance or benchmark run produced it; `claimed` is operator- or adapter-asserted. This follows `~/llm` ADR-0004, which says a claimed capability must be measured before it is trusted. Placement may require `probed` or better.

Initial well-known ids (all data, none code):

| Family | Ids |
|---|---|
| CPU / OS | `cpu.arch.aarch64`, `cpu.arch.armv7`, `cpu.arch.x86_64`, `os.linux`, `os.macos` |
| Runtimes | `runtime.native`, `runtime.container.apple`, `runtime.container.docker` (attr `variant`: orbstack, engine, desktop), `runtime.container.podman`, `runtime.wasm.wasmtime`, `runtime.infer.llamacpp`, `runtime.infer.mlx-lm`, `runtime.infer.ollama`; each with attrs `arches_native[]`, `arches_emulated[]` |
| Accelerators | `accel.gpu.metal`, `accel.gpu.cuda`, `accel.gpu.rocm`, `accel.gpu.vulkan`, `accel.npu.ane`, `accel.npu.hailo`, `accel.npu.rknn`, `accel.npu.qualcomm`, `accel.tpu.coral`, `accel.tpu.cloud`, `accel.tsu.<vendor>`, `accel.other.<name>` |
| Model / data formats | `format.gguf`, `format.mlx`, `format.safetensors`, `format.onnx`, `format.tflite`, `format.coreml`, `format.hef` (as attrs on accelerators and runtimes, and as ids so a requirement can name them) |
| Memory | `mem.system` (attrs total, free), `mem.unified` (marks that GPU and NPU share the system pool, so a placer must not double-count), `mem.vram` |
| Feeds / data | `feed.esp32-csi-udp` (attr `lan_id`, `bind`), `feed.http-sensor`, `store.tier.internal`, `store.tier.external` (attr `mounted`), `model.present` (list of shard hashes held) |
| Trust / class | `trust.tier.{discovered,paired,pinned}`, `node.class.{dev-mac,cognitum-seed,pi5,arm-server,other}` (informational) |

Probing is a `node_facts` module run at boot and on change. It reads `std::env::consts`, `sysctl`/`/proc`, and shells to `docker info`, `container --version`, `podman info`, `ollama`/`llama-server` presence, binfmt/qemu for emulation, and vendor tools where present (e.g. `nvidia-smi`, `hailortcli`). It never assumes: an accelerator is advertised only if a probe confirms it. On this Mac (M5 Max, 128 GB, Metal 4, 40 GPU cores, verified with `sysctl` and `system_profiler` on 2026-09-28) the probe would emit `accel.gpu.metal` with `unified = true` and `mem.unified`. The Apple Neural Engine has no public direct-query API and is reachable only through CoreML; the probe can infer its presence from the chip and CoreML availability, but **busy state and throughput cannot be read**, so ANE is advertised as `claimed` or `probed` at most and marked `state: available` without utilisation until measured.

**Measured throughput, not only architecture.** NodeFacts also carries `perf.*` capabilities with `provenance: measured`: for cogs, `perf.cog.cycle_ms{cog_id}` (wall time of one `--once`/`--interval 1` cycle on a reference feed); for inference, `perf.infer.tok_s{model}` and prefill rate; for accelerators, per-format throughput. The conformance harness (card 08) and admission probes populate them. The motivating measurement (real hardware, 2026-09-29): the same cog at `--interval 1` takes about 6 s per cycle on a Pi Zero 2 W and about 1 s on a Pi 5, a 6x difference between two nodes that both advertise `cpu.arch.aarch64`/`armv7` and pass every hard constraint. Architecture alone cannot rank them. On identical synthetic input, fall-detect on a Cognitum Seed (armhf) and on the Pi 5 (aarch64 native) produced the same `z_impact` to the last digit printed (0.7071067811865475 versus ...476), so measured cycle time is a valid ranking signal without correctness caveats between those arches.

Advertisements are signed by the node key (proves who said it, not that it is true), carry a TTL, and are refreshed on change. A node's state (`busy`, free memory) is a small delta message on top of the signed base.

### 3. Placement

A placement request is `(workload, config, pins/affinity, allow_emulated?)`. The engine is a **pure function** `place(request, node_facts[], cluster_state) -> Decision` so it is table-testable; I/O lives in the control plane.

**Phase A, hard constraints** (a node failing any is filtered, with the failing constraint recorded):
1. Requirements from the workload kind are satisfied by advertised capabilities (arch, runtime, accelerator class and format, memory / VRAM including model size plus KV budget, exclusive access).
2. `hardware_requirement` and resource asks from the manifest.
3. Data locality constraints the kind declares (sensor LAN `lan_id`; for inference, only a **preference**, see ADR-101).
4. Trust tier at least what the package policy requires; provenance at least what the requirement requires.
5. Node `Alive`, not revoked, facts within TTL.
6. **Co-residency**: the workload's `excludes` list (for example two large model servers that must not share a memory pool, as in `~/llm` docs) against instances already on the node.
7. **Governance**: `gate.check(requester, "workload.place", ctx)` permits.

**Phase B, preference scoring** among survivors: native on real target hardware, then emulated, then the dev-mac fallback, as strict tiers (example weights 100 / 40 / 20, tunable; a lower tier never outranks a higher one). This was decided by the user on 2026-09-29: ARM workloads must run on the real ARM node, such as the Pi, and a native run on the dev Mac is only a last-resort fallback, never preferred over target hardware. The earlier example weights (100 / 20 / 40) contradicted the stated order and were wrong. Verification follows the same rule: anything that must prove it runs on ARM is tested on the Pi through `scripts/build.sh test-pi`; data locality (feed on the same LAN, weights already present, consumer proximity); accelerator fit (prefer the smallest sufficient accelerator, never steal an `exclusive` one); **measured performance** (for workloads declaring `latency_class = interactive` or a cycle-time budget, a node whose measured `perf.*` misses the budget is filtered, and among passers a lower measured cycle time scores higher; nodes with no measurement are scored conservatively and flagged); load; stickiness to the current node. Weights are placeholders in governance config, not fixed by this ADR.

**Overrides and fallback.** Operator pins and affinity override scoring, not constraints; a pin to an ineligible node is a hard error naming the failed constraint. **Emulated placement is allowed only as an explicit fallback** (`allow_emulated`), recorded as `emulated: true` in the chain event. A workload with no eligible node is reported `Unplaceable` with per-node reasons, never silently degraded.

**Verification of claims.** The target node runs an admission self-check (binary arch, runtime dry-run, free memory, accelerator device open) and refuses on disagreement; the refusal is chained and the placer tries the next candidate. Conformance runs can upgrade a capability's provenance to `measured`.

**Validated on hardware (2026-09-29).** The native ARM path is proven on a Pi 5 that already runs weaver v0.8.1 as a mesh member: released aarch64 cogs `anomaly-detect`, `fall-detect`, `baby-cry` and `sleep-apnea` ingest natively there. A Cognitum Seed was driven successfully through its own HTTP API (ADR-100 section 5). Placement itself is still unimplemented; these results validate the target nodes and the native adapter's premise, not the layer.

### 4. Governance

Actions: `workload.install`, `workload.place`, `workload.load`, `workload.start`, `workload.unload`, `workload.migrate`, all with `kind` in context and an `EffectVector` (package trust, node trust tier, network policy, secrets present, emulated, accelerator use, resource cost). Because `CapabilityGate` permits unknown actions (`gate.rs:111-135`, test `capability_gate_unknown_action_permits`), the governance rule set must ship an explicit default-deny for the `workload.*` prefix (distributed per ADR-092). Decisions, permitted or denied, are chained.

### 5. Runtime adapters

```rust
trait WorkloadRuntime {
    fn id(&self) -> &str;                              // "native", "container.apple", "container.docker", "infer.llamacpp", ...
    fn provides(&self) -> Vec<Capability>;             // what a node advertises when this adapter works
    async fn admit(&self, w: &VerifiedWorkload) -> Result<Admission>;   // self-check, reserves resources
    async fn load(&self, w: &VerifiedWorkload, cfg: &Config) -> Result<InstanceHandle>;
    async fn start(&self, h: &InstanceHandle) -> Result<()>;
    async fn stop(&self, h: &InstanceHandle, grace: Duration) -> Result<Evidence>;
    async fn unload(&self, h: InstanceHandle) -> Result<()>;
    async fn status(&self, h: &InstanceHandle) -> InstanceStatus;   // includes kind-specific health
    fn control_mode(&self) -> ControlMode;             // Managed | Adopted (observe-only)
}
```

Adapters: `native` process (cog-runner-style limits, later landlock / seccomp), `container.*` (apple-container, docker/OrbStack, podman), `wasm` (later; **decided 2026-09-29: unify on `clawft-wasm-host`** and retire or wrap the kernel `wasm_runner` module for cogs, so there is one WASM sandbox and one permission store), a **`remote.api`** family for nodes that expose their own management API instead of running WeftOS (the Cognitum Seed, ADR-100), and **inference-server** adapters (`infer.llamacpp`, `infer.mlx-lm`, `infer.ollama`) that wrap a server process or an already-running server. Accelerator-specific adapters (Hailo HEF runner, Coral TFLite delegate, RKNN, Qualcomm, TSU) implement the same trait and simply add their capability ids. **`ControlMode::Adopted`** lets the layer register and health-check a server the operator started by hand (as today) without controlling it, which is the first migration step for inference.

Accelerator job kinds and the TPU / TSU / NPU adapters are **deferred until the hardware exists**; the vocabulary and trait ship now.

### 6. Artifacts and large payloads

- The manifest lists `{path, size, blake3, shard_index}`. Large files are split into fixed-size pieces (proposal 64 MiB) with a piece-hash list.
- **Swarm distribution (decided 2026-09-29).** Artifacts spread across the mesh the way a torrent does, though it doesn't have to be BitTorrent itself. The design has three parts:
  - **Identity.** An artifact is identified by its content (the root hash over the piece list), not by the node it came from. Any node that holds verified pieces can serve them.
  - **Exchange.** Nodes announce what they hold with a `have` bitfield, request individual pieces, and verify each piece's hash before accepting it. A node that finishes a download becomes a seeder.
  - **Swarm behaviour.** Nodes fetch different pieces from several peers at once, prefer the rarest pieces, and choose peers by locality and measured link speed (NodeFacts). A seeder dropping out mid-transfer only means re-requesting its pieces from another peer.
- **The work is split across two cards.** Card 11 builds the simple node-to-node version, on the swarm-ready protocol above. It is content-addressed and piece-based, resumes from the `have` bitfield, and can fetch from any holder, not only the node the artifact came from. Card 25 extends it:
  - multi-source parallel fetch and rarest-first;
  - seeding, and reseeding after a fetch;
  - a cache with pinning and eviction policy, advertising held artifacts as capabilities;
  - bandwidth limits;
  - reliability when seeders drop out.

  `ArtifactExchange` is finished on this basis (today it is scaffolding).
- **Governance of distribution.** Only artifacts whose signed manifest verifies may be seeded or cached. Revocation of a package id, signer or artifact hash stops seeding it and evicts it from caches everywhere, and every seed, evict and revoke action is chained. Transfer outcomes are chained as well: each completed or failed fetch (`artifact.fetch`), each rejected piece and the peer that sent it (`artifact.piece_rejected`), and the first time an artifact is served to each peer (`artifact.serve`). Individual piece transfers are not chained, because they would flood the chain. Sharing is opt-in: a cog package is seeded, advertised and served only when its signed manifest carries `redistributable = true` (default false) and it has no Cognitum provenance; see `docs/research/mesh-placement/swarm-throughput.md`.
- **Adopt in place**: a node may hash existing local files (for example an HF cache or Ollama blob store) and register them as artifacts without copying, so 20 GB of weights already on a node is never re-shipped. Adoption is recorded and the file is re-hashed lazily on use.
- **Locality-aware fetch**: nodes advertise `model.present` (shard hashes held) and `store.tier.*` (with `mounted`). The placer prefers a node that already holds the payload; if none does, it chooses between fetching from a peer and placing where the bytes are, using size versus link estimate, and records the choice.
- Signatures cover the manifest (hashes of everything), not the bytes; verification checks each shard hash on arrival or on adoption.

### 7. Mesh lifecycle

State machine per instance: `Requested -> Placed -> Fetching -> Verified -> Loaded -> Running -> (Stopping -> Unloaded | Failed | Lost)`.

- **Control plane**: a `workload.ctl` message set (place, load, start, stop, unload, status, logs) over `MeshIpcEnvelope` / `MeshRequest`, addressed to the node's `workload-host` service (advertised through `ServiceAdvertisement`). Messages are signed and carry the governance decision id and a nonce with expiry.
- **Health**: node liveness from `HeartbeatTracker`; instance health from a periodic status heartbeat whose meaning is kind-defined (cog: last ingest; inference: `/health` and `/v1/models`).
- **Restart**: node-local supervisor with bounded backoff, each restart chained.
- **Reschedule on node loss**: instances of a `Dead` node become `Lost`; the placer re-runs placement excluding the dead node (stickiness off) subject to governance, unless the instance is `pinned` or the kind declares itself non-migratable (inference with warm KV, see ADR-101), in which case it raises an alert.
- **Delivery is at-least-once, with no fencing.** A node that is cut off keeps running what it holds. The controller declares it `Dead` only after it has been unreachable for `dead_after` (and membership, when it knows the peer, agrees it is not healthy), places a replacement, and unloads the old copy when the node returns. For that whole span the workload may run twice: a sensor cog on the cut-off node and its replacement both produce data (duplicates, not loss), and a workload with side effects may act twice. The mitigations are partial. The opt-in node lease (`workload-host.json` `lease_secs`) makes a node stop its continuous instances once no remote controller has been heard for the lease, so set it at or below `dead_after` and the old copy stops before the replacement starts, at the cost of stopping everything another node placed on a node that merely lost its controller (instances the node placed itself are exempt, and the minimum is 30 s, three controller ticks). The controller restarts a lease-stopped instance on its next pass; an operator stop is final. A workload that needs attached hardware (a requirement in `sensor`, `device` or `feed`; `accel`, `trust`, `store` and `model` do not pin, since any node with the same accelerator is as good a home), a pin, an operator `--no-migrate`, or a non-migratable kind is never moved: it raises an alert and stays `Lost`. A node that returns without an instance it never reported as revoked or unloaded (a reboot) has it placed again, on the same node when it is pinned or non-migratable, subject to the same gate and engine. Failed reschedules back off (capped at five minutes between attempts) and keep retrying; an alert is chained once at `max_reschedules` attempts. Real exactly-once would need a lease the controller grants and the node must renew before the controller may replace anything; that is deferred.
- **Revocation**: extend `RevocationList` to package ids, signer keys, and artifact hashes. A revocation is a chained, gossiped event; nodes check at fetch and on receipt, stop and unload affected instances, and reconcile on rejoin via `chain_seq`.
- **Audit**: every transition, placement decision (candidates, scores, failed constraints), governance decision, refusal, and revocation is an ExoChain event (ADR-022): new kinds `workload.install / place / load / start / stop / unload / migrate / refuse / revoke` next to `chain.rs:459-465`.

### 8. Trust (Decided 2026-09-29, defaults accepted by user)

1. Package and manifest signatures are Ed25519 (ADR-025) from a **pinned WeftOS signer set plus operator-pinned keys** in governance config (ADR-092). Rationale: one anchor we control, rotated through the existing governance distribution. At least one valid signature from a pinned signer is required to install or place. ML-DSA-65 dual signing (ADR-028) is a follow-up.
2. Kind-specific external verifiers (Cognitum release records for cogs) are optional additional signatures (ADR-100). **Model trust is operator attestation over a hash manifest** (ADR-101), because upstream publishers of weights rarely sign.
3. Node trust tiers gate what may run there: `discovered` nothing by default; `paired` operator-signed workloads; `pinned` workloads that carry secrets.
4. Commercial terms (Cognitum's 30% per cog) are **contractual, not enforced in code**. Rationale: enforcement code is not a substitute for the agreement.
5. Our cogs fork is the source of truth and is ahead of upstream until our PRs merge (ADR-100).

### 9. Security and deferred

Threat model: malicious package, lying node advertisement, node that ignores unload, replayed control message, LAN attackers (the ESP32 UDP feed is unauthenticated). Mitigations in v1: signature and hash checks before execution, governance on every action, unprivileged and container isolation, signed control messages, per-instance tokens, secrets over Noise only, revocation with forced unload. Known limits: node facts are self-reported (admission self-check and conformance runs reduce, not remove, this); a compromised node key can lie; native isolation is weak until landlock / seccomp.

Deferred: WASM lane; landlock / seccomp; cross-LAN sensor relay; armv7 on Apple `container` (unsupported by that runtime); ML-DSA dual signing; hardware attestation; TEE backend; NPU / TPU / TSU hardware adapters (until the devices exist); cost-based bin-packing (this is constraints plus scoring, not a general scheduler).

## Consequences

Positive: one layer serves cogs, model servers, and future accelerator jobs; new accelerator classes need a vocabulary entry and an adapter, not a kernel change; governance and audit are uniform; local inference gains failover and locality; most mesh plumbing is reused.

Cost: finishing scaffolding (`ArtifactExchange`, placement state, container runtime), adding signature verification, a new signed advertisement, a control message set, probably the daemon RPC handlers.

Risks: scope creep into a general scheduler (mitigation: pure constraint plus score function, no preemption or bin-packing); advertisement drift (TTL, admission self-check); vocabulary sprawl (mitigation: the vocabulary file is advisory and reviewed like any config); wrong unified-memory accounting (mitigation: `mem.unified` marks a shared pool).

## Decisions (settled 2026-09-29, defaults accepted by user)

1. **Trust anchor**: pinned WeftOS Ed25519 signer set plus operator-pinned keys; Cognitum release records optional (section 8). Rationale: single controlled anchor.
2. **RPC family**: a new `workload.*` RPC family, not reuse of `app.*`. Rationale: workloads are not kernel-spawned agents. Card 06 still confirms the suspected missing `app.*` daemon handlers.
   - **Confirmed 2026-09-29 (card mesh-placement-06)**: against a running daemon built from `dc9c7ea2`, every `weaver app list|inspect|start|stop|remove|install` returned `unknown method: app.*`; the gap was real. The daemon now routes `app.*` to the kernel `AppManager` behind the governance gate (`clawft-weave/src/app_rpc.rs`) and serves `workload.list|inspect|install|unload` from a node-local catalog (`clawft-weave/src/workload_rpc.rs`). `workload.*` mutations fail closed without a gate and chain `workload.install` / `workload.unload` / `workload.refuse`; `workload.revoke` and `workload.node.bind` are Admin, the other lifecycle verbs Write, and an unclassified `workload.*` verb defaults to Write (`capability.rs`). `place`, `load`, `start`, `stop` and `migrate` answer "not available on this node" until cards 09 and 12 land. `weaver app list` still shows apps only; the wrapper that lists workloads there (section 1) has not been built.
   - **Implemented 2026-09-29 (card mesh-placement-12)**: the control plane is `clawft-kernel/src/workload_ctl/` (signed `workload.ctl` requests with nonce, expiry and decision id over `MeshIpcEnvelope` / `MeshRequest`; the `workload-host` service with fetch-before-load and admission; retry to the next candidate; chained decisions and refusals). The daemon serves `workload.place|explain|status|stop|logs` and `workload.unload {instance_id}` (`clawft-weave/src/workload_place_rpc.rs`), and `weaver workload place|explain|status|stop|logs|unload` call them. An unknown `workload.*` method is refused and the refusal chained, both at the daemon RPC and at a node's `workload-host`. Seed nodes are addressed through the card 09 Seed adapter on their operator-assigned node id (`plane_seed.rs`). A daemon serves its own `workload-host` to other controllers when the operator writes `<runtime>/workload-host.json` (listen address, Noise XX, the controller keys it answers; off when absent) (`clawft-weave/src/workload_host_serve.rs`). The daemon advertises the host through the `ServiceAdvertisement` in every signed `describe`, and re-probes the signed facts it returns. The host runs on a dedicated port, not on the kernel mesh listener (`:9489`). Trust is the operator's call: a peer named in a request is only `discovered`, and `workload-peers.json` assigns tiers. `workload.explain` and `workload.logs` need Write. A start that fails after load is unloaded. A `place` whose answer is lost is reconciled against the target's instances and in-flight decisions before another candidate is tried; if it cannot be reconciled, placement stops. Proven on real hardware by `scripts/build.sh test-pi --placement`: the Mac controller places onto an isolated `weaver` daemon on the Pi 5. Evidence: `docs/cogs/evidence/placement-mac-pi5-weaver-2026-09-30.json`.
   - **Review round 3 (2026-09-29, card mesh-placement-12)**: operator trust is bound to a node key, not an address. A `workload-peers.json` entry may pin `"key"`; without one the tier goes to the key first learned at the address, and `refresh` re-describes every target requiring its stored key, so another key answering at a listed address is never learned under that tier. A controller offers a node only the runtimes its `workload-host` advertises a route for; the daemon serves a Docker / Podman / Apple container adapter when the operator writes `workload-container.json`. Known targets and placements are persisted (`workload-placements.json`, `0600`, atomic) and survive a daemon restart. Seeds are configured in `workload-seeds.json` and placed with `workload.place {store_pin}` (`weaver workload place-seed`). The daemon's default-deny refusal of an unknown `workload.*` method is chained within a budget and cuts the method name to 64 characters. Live evidence: `docs/cogs/evidence/placement-mac-pi5-weaver-container-2026-09-29.json` and `placement-mac-pi5-weaver-native-only-2026-09-29.json`.
3. **Emulation**: operator opt-in only, never automatic. Rationale: emulated results and timing are misleading, and the choice must be visible and chained.
4. **WASM stack**: unify on `clawft-wasm-host`; retire or wrap the kernel `wasm_runner` for cogs. Rationale: one sandbox, one permission store.
5. **Adopt-in-place and weight transfer**: no hard transfer-size ceiling in v1; adopt-in-place is allowed on removable drives, and a detached drive marks the workload Degraded (never silently broken). Chunk size 64 MiB remains a tunable placeholder.
6. **Vocabulary file** (`config/capabilities.toml`): changed only through the governance path, not freely editable. It remains advisory (does not gate matching).
7. Other decisions on inference (proxy, `~/llm`, exposure, first slice) are recorded in ADR-101; cog scope and Seed strategy in ADR-100.

## Amendment 2026-10-05: node-admin methods on the `workload.ctl` wire

An operator needs to reach a specific peer to run an admin action on it (first use: rotate that node's WeftOS dashboard token, `weaver dashboard rotate-token --node <id>`). The mesh had no general remote-RPC path, but `workload.ctl` already is a signed, expiring, replay-guarded request to one named node, answered by a controller-key check and Noise XX, so the amendment reuses it rather than adding a transport.

- **Methods.** `dashboard.status` and `dashboard.token.rotate` join the message set as *node-admin* methods (`method::NODE_ADMIN`). They are not in `method::ALL` (no adapter serves them) and are answered by a `NodeAdmin` hook the daemon installs on its `workload-host` (`WorkloadHostService::set_node_admin`). With no hook the host refuses them as `unknown_method`. `dashboard.token.rotate` is a mutation: it carries a decision id and a lost answer is `Indeterminate`.
- **Trust, in order.** The local caller needs Admin (`dashboard.token.rotate`; any `node` that is another peer, for status too). The controller sends a mutating node-admin call only to a target the operator pinned in `workload-peers.json` (`PlacementControlPlane::node_admin`, tier `pinned`; `paired` may read status). The target serves it only when the signing key is in its `workload-host.json` `controllers`. The hook may refuse further (`[dashboard] allow_remote_rotate = false`).
- **Chain.** The controller chains `node_admin.sent` and `node_admin.outcome`; the target chains `node_admin.request` and `node_admin.result`. Neither carries the body or any secret; the method result is `{rotated, rotated_at, token_file}` and never a token.
- **Code.** `clawft-kernel/src/workload_ctl/{host_node_admin,plane_node_admin}.rs`; daemon side `clawft-weave/src/dashboard_rpc.rs`. Guide: `docs/guides/dashboard-reporter.md`.

## Amendment 2026-10-26: placed instances survive a daemon restart

A `workload-host` kept its instances in memory, so a daemon restart emptied the node. It now journals each placed instance and restores them at startup (`workload_ctl/host_journal.rs`, `workload-host-journal.json` in the runtime dir).

- **What is journaled.** The `place` body, the requester, the decision id and whether the instance was meant to run. No token, key or secret. A restore places each one again through the same path and governance as the original `place` (new cogbus token and ingest lease, same instance id), starting only what was meant to run.
- **Instance ids and `data/`.** Native instance ids carry the project and the controller (`HostContract::owner`), so the same package placed for another project is another instance with its own directory. Staging keeps a real `data/` and replaces everything else in the instance directory, never following a symlink; an explicit `unload` removes the whole directory. The id carries 128 bits of that hash (32 hex characters; an earlier draft had 32 bits, which a controller could grind offline), and each instance directory also holds an `.owner` marker with the full hash: staging reuses a directory only when the marker matches, and refuses (never empties) one that names another owner, or that holds data and has no marker (a directory from before markers must be removed by hand). The marker is written first and atomically (temp file, fsync, rename, directory fsync) and a matching one is never deleted, so a crash at any point leaves a directory that still names its owner; an empty or torn marker counts as no marker, never as another owner's. `data/` is mode 0700 and, when the host runs cogs as another uid, owned by the instance's uid (kept files are brought to it). Per-project file separation on disk therefore holds only where each cog has its own uid: while cogs share the daemon's uid the project separation is by directory and id, not by file permissions, and per-instance uids (gap-uid) are what make it a permission boundary. Instance ids changed twice during development (the owner key, then the 128-bit width); only dev branches ran those commits and no node holds such an instance, so there is nothing to migrate.
- **What is checked at restore.** The file is signed with the node key over the node id, a generation and the entries; an edited or foreign file is refused whole. Each save writes the file and then records the generation on the chain (`workload.journal`, a reserved kind: not replicable, not appendable through the `chain.append` RPC or the tracing bridge). A file whose generation is below the chain's is an older copy and is refused. A file the chain has no record of, or no chain at all, restores instances loaded and starts none. A journal that is missing or unreadable while the chain says one existed restores nothing and reports `journal missing`. The journaled requester must still be a controller under the live policy.
- **Failures.** A permanent refusal (verification, governance, authorisation, a bad request) drops the entry; anything else, including a governance denial caused by an unreadable revocation list, keeps it and retries at the next start, giving up after five. The restore report, the log and the host's `status` listing (`include_departed`) carry the outcome and `journal_status`.
- **Limit of the replay check.** The generation is tamper-evidence only up to the chain's durability. The chain is written out at a clean shutdown, so after a crash it can be older than the journal, and an attacker who can roll the chain back as well as the file defeats the check. Owner card a93f7abe (a durable chain) closes that; until it lands, treat the generation as protection against a copied or stale file, not against a rollback of the whole node.
