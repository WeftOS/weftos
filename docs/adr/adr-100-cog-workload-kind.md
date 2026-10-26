# ADR-100: The cog workload kind

- **Status**: Accepted (2026-09-29; decisions settled 2026-09-29, implementation tracked on cards mesh-placement-01..23)
- **Numbering**: This ADR was briefly filed as "COG-001" on 2026-09-29 and moved back to ADR-100 the same day. The COG-NNN series now belongs to the cogs project repo, which is private (`weave-logic-ai/cognitum-cogs`; its `docs/decisions/`), and holds cog-project decisions. This ADR stays in WeftOS because it defines how WeftOS hosts cogs.
- **Cogs project**: operational detail on Seeds, the Pi 5, fleets, tooling and upstream work now lives in the cogs repo (`docs/devices/`, `docs/testing.md`, `docs/upstream.md`). This ADR keeps only what WeftOS implements.
- **Date**: 2026-09-28
- **Deciders**: Platform / ops. Open questions settled 2026-09-29 (defaults accepted by user); status is Accepted (2026-09-29) and the decisions below are settled, with implementation tracked on the cards named in Status.
- **Depends-On**: ADR-099 (governed workload placement)
- **Relates-To**: ADR-025, ADR-092, ADR-101, ADR-105 (cog sources, per-project catalog and licences), `docs/research/mesh-placement/README.md`

## Context

Cognitum "cogs" (upstream `cognitum-one/cogs`, MIT; our vendored copy is 108 standalone Rust crates) are small native binaries, each with a `cog.toml` manifest. Facts established empirically or from upstream docs:

- Manifest: `[cog]` id / version / category / binary / `hardware_requirement`; typed `[config.*]` (some `secret = true`); `[console]` allowed_commands / max_runtime_secs / output_limit_bytes; optional `[api]`, `[resources]` ram_mb / cpu_pct, `[mcp]`, `[upstream]`, `[integrations.*]`. Entry is a CLI with `--once` / `--interval N`.
- Host contract: input UDP `0.0.0.0:5006` ESP32 packets (magic `0xC5110003` = 48 bytes, 8 LE f32 at offset 16; `0xC5110002` = 32-byte vitals), override `COG_CSI_BIND`; fallback `GET 127.0.0.1:80/api/v1/sensor/stream` (`COG_SENSOR_URL`). Output: `POST 127.0.0.1:80/api/v1/store/ingest {"vectors":[[id,[8 floats]]],"dedup":true}` plus JSON on stdout. Env `COGNITUM_COG_TOKEN`, `COGNITUM_COG_DATA_DIR`, `COG_APP_DIR`.
- Released binaries: aarch64 (107 of 108; `presence-field` missing) and armv7; no x86_64. Upstream ADR-001 rejected WASM for v1. Upstream signing: ADR-154/155 Ed25519 release records with a trust registry; only `anomaly-detect` is release-eligible today.
- Sweep on Apple Silicon, all 107 aarch64 cogs with `--once` against a fake UDP feed and stub ingest in OrbStack: 93 clean, 5 persistent-listener health cogs need `--interval`, 9 need seed peers, assets or other CLI (tailscale, cloud-inference, cognitive-pipeline, and six swarm-* cogs). Corrected 2026-09-29 from an earlier miscount of 7, per the committed conformance baseline in scripts/cogs/.
- OrbStack (docker, linux/aarch64) runs aarch64 natively and armv7 emulated. Apple `container` 1.0 runs aarch64 only (armv7: Exec format error). The available x86_64 dev host is not an ARM target.
- The sensor feed is UDP on the LAN of the sensors, so placement depends on data locality, not only CPU architecture.

## Decision

A cog is workload `kind = "cog"` on the ADR-099 layer.

### 1. Package

A **cog package** is a signed manifest artifact (`cogpkg.json`) listing: the unmodified `cog.toml` (hash), one binary per arch (`aarch64/cog-<id>`, `armv7/cog-<id>`, later `x86_64/`, later `wasm/<id>.wasm`), the source (fork commit or upstream release URL), and signatures. Files live in `ArtifactStore` by BLAKE3 hash; the record hash is the package id. Config and secrets come at place time (`secret = true` values over Noise, hash-only in chain events).

### 2. Requirements derived from the manifest

The kind maps a package plus config to ADR-099 requirements:
- `cpu.arch.<a>` for an arch that has a binary (native preferred; emulated only with `allow_emulated`);
- a runtime capability that can execute it: `runtime.native` (matching arch natively) or a `runtime.container.*` whose `arches_native` / `arches_emulated` covers it;
- `mem.system` free >= `[resources].ram_mb`; cpu headroom >= `cpu_pct`;
- `hardware_requirement` mapped to capability ids (unknown strings become `x.hw.<value>` and match only nodes advertising exactly that id);
- **sensor locality**: if the input is the ESP32 UDP feed, `feed.esp32-csi-udp` with `lan_id` equal to the feed's. Cross-LAN relay is deferred, so v1 is same-LAN only. HTTP-fallback cogs need only reachability of `COG_SENSOR_URL`;
- trust tier from package policy.

Scoring adds the ADR-099 defaults: real ARM hardware over the Mac container over emulation; feed locality.

### 3. Runtime adapters used

- `native` on Seed / Zero, Pi 5, ARM server, any Linux: binary written under `COGNITUM_COG_DATA_DIR`, unprivileged user, `[console]` limits enforced by a cog-runner-style supervisor (kill at `max_runtime_secs`, cap output, rlimits from `[resources]`), `RunEvidence` emitted. Until landlock / seccomp exist, native placement requires `trust.tier >= paired` and an operator-signed package.
- `container.apple` preferred on macOS for aarch64 (per-container VM isolation); `container.docker` (OrbStack) for armv7 and emulation; docker / podman on Linux ARM. The image is a minimal layer containing the one verified binary, built locally from the artifact (no arbitrary registry pulls). This replaces the simulated start at `container.rs:383-405` for this kind.
- `wasm` later, on `clawft-wasm-host` (ADR-099 decision 4: unified stack; the kernel `wasm_runner` is retired or wrapped for cogs).

### 4. Host-contract adapter (common to all runtimes)

- Inject `COG_CSI_BIND` and `COG_SENSOR_URL`; for containers, either publish the feed port or run a node-local UDP forwarder into the container.
- Provide an **ingest bridge** speaking the `POST /api/v1/store/ingest` contract: validate shape and size, rate-limit, forward vectors to the WeftOS store over the mesh as signed `MeshIpcEnvelope`s to the store owner; capture stdout JSON as evidence and logs, never as control input.
- Network policy: sensor feed in, ingest bridge out, nothing else. v1 relies on process user and bind discipline for native, and a restricted network for containers; nftables / landlock net rules are deferred.
- `COGNITUM_COG_TOKEN` is a per-instance token accepted only by that instance's bridge.
- Run mode is part of the kind spec: `once`, `interval N`, or `listener`; the catalog (from conformance results) records which of the 93 / 5 / 9 groups each cog belongs to.

### 5. Cognitum Seed strategy (Decided 2026-09-29; revised after real-hardware measurements)

The first draft recommended installing a WeftOS node agent on the Seed (option a) and treated driving the Seed's own API (option b) as a fallback because the Seed agent was assumed private. Measurement on real hardware on 2026-09-29 reversed that.

**Decision: option (b) is the v1 adapter for Cognitum Seeds; option (a) remains the path for the Pi 5, ARM servers, and any node we control.** Rationale: the Seed exposes a documented HTTP API and an MCP endpoint, so a `remote.api` runtime adapter (ADR-099) can place cogs there without running WeftOS on the device.

What was measured:
- The Seed API (firmware 0.24.2; MCP endpoint with 130 tools at `toolScope` "full"). Relevant endpoints: `/api/v1/apps/install {"id"}`, `/api/v1/apps/{id}/start`, `stop`, `console {"command"}`, `logs`, `config`, `manifest`; pairing via `/api/v1/pair/window` and `/pair` over USB with a bearer token; `/api/v1/upgrade/apply` for binary OTA.
- Store facts: the registry (v2.3.2) lists 107 cogs but **not `anomaly-detect`**. So on Seeds the v1 first cogs are `fall-detect` or `baby-cry` via the store, while `anomaly-detect` goes through our own signed-package path (native adapter, on nodes we control). Installed cogs auto-start. The Seed caps concurrency at 3. Contention on UDP 5006 (upstream cogs#14) is real: running instances must be stopped before a console run. The console runs one cycle.
- Parity and speed: `fall-detect` on the Seed (armhf) versus the Pi 5 (aarch64 native) on an identical synthetic feed gave the same `z_impact` to the last digit printed (0.7071067811865475 versus ...476). A Pi Zero 2 W cycle at `--interval 1` is about 6 s versus about 1 s on the Pi 5, the motivating case for measured-throughput placement (ADR-099).
- Pi 5: already runs weaver v0.8.1 as a mesh member and runs released aarch64 cogs natively (`anomaly-detect`, `fall-detect`, `baby-cry`, `sleep-apnea` ingest). It is the reference native node. Evidence for the card-09 native-on-real-ARM criterion is this 2026-09-29 Pi 5 run (owner-reported; not re-run for card `d4fe33c5`); see [seed-adapter-operations.md](../research/mesh-placement/seed-adapter-operations.md) section 3.
- Firmware quirks: an upgrade from 0.10.x tripped the known witness-chain `writes_gated` state, recovered with `/api/v1/store/truncate-confirm` after a backup, so the adapter must treat firmware upgrade as a governed, backed-up operation. MCP specification defects are tracked upstream (cognitum-one/support#19, cognitum-claude-plugin #3 to #6).

Seed bind status: wired in ADR-106 phase 1d. The daemon's `workload.node.bind` (Admin) runs `SeedBinder` over `dir.join(BIND_STATE_FILE)` with the steward profile, and `workload.node.unbind` withdraws a binding; the operator signs a v2 record with `weaver workload node bind`. The v1 `bind` form stays a tested library. Teardown waiver: `check_teardown` re-decides at the highest node tier (`pinned`), which also waives the secrets-need-pinned deny and the tier-scaled risk threshold; that is intended, because both are caused by the tier. It accepts only `workload.stop`, `workload.unload` and `workload.load` (re-adoption); any other action is denied.

Link rule (2026-10-02): a Seed is bound and placed on only over a pinned link (https with a certificate or public-key pin); plain http or WebPKI-only (even a CA-signed certificate) needs the explicit per-Seed lab opt-in, which is read only from the runtime dir's `workload-seeds.json`. See [seed-adapter-operations.md](../research/mesh-placement/seed-adapter-operations.md) section 4.

Consequences of (b): governance on a Seed is enforced by WeftOS **at the adapter** (gate before any install, chained results) and not on the device; the Seed's own isolation and trust apply on-device. The adapter needs a per-Seed credential, held in the operator secret store and never in chain events. The Seed's registry provenance is Cognitum's, so installs from the store are accepted only for packages the operator has pinned (by id and version) in governance config; our own signed packages cannot be installed through the store path and use option (a) nodes.

Remaining unknowns: whether Seeds can be given the WeftOS node agent later; ~~how Cognitum identity maps to a WeftOS node id~~ (settled: an operator-signed bind record `{device_id, device_pubkey, node_id, bound_at}` verified against the Seed's `GET /api/v1/identity` and chained as `workload.node.bind`; the node id is derived from a per-Seed adapter key, see [seed-adapter-operations.md](../research/mesh-placement/seed-adapter-operations.md) section 5); behavior beyond firmware 0.24.2.

### 6. Trust (Decided 2026-09-29, defaults accepted by user)

1. The pinned WeftOS Ed25519 signer set plus operator-pinned keys (ADR-099 section 8) is authoritative. Rationale: one controlled anchor.
2. Cognitum ADR-154/155 release records are an **optional additional verifier**: with the Cognitum registry key pinned, a valid record counts as a signature. Not required, since only `anomaly-detect` is release-eligible.
3. Our fork (`~/Clients/cognitum/vendor/cogs`, upstream `8970f99`) is the source of truth and is **ahead of upstream until our PRs merge**; packages record the fork commit. Upstream released binaries are usable only after an operator hashes and signs them; a URL is never trust.
4. Cognitum's 30% per cog is contractual and not enforced in code. Rationale: agreement, not code, carries the commercial term.

## Consequences

Positive: cogs get governed, signed, revocable placement onto real ARM hardware with the Mac container path as a supported fallback; the conformance harness becomes a committed admission test.

Cost: package signing, the ingest bridge, container adapters, and Seed integration unknowns.

## Decisions (settled 2026-09-29, defaults accepted by user)

1. **Seed strategy**: option (b), the Seed's own API, is the v1 Seed adapter; option (a) stays for Pi 5 / ARM servers (section 5).
2. **Emulation**: operator opt-in only, never automatic (ADR-099).
3. **v1 cog scope**: the 93 cogs that run clean in the conformance sweep. Rationale: they are demonstrated working. `anomaly-detect` is the first cog on nodes we control (native or container path), since the Seed registry lacks it; `fall-detect` or `baby-cry` are the first via a Seed.
4. **Source of truth**: our fork, ahead of upstream until our PRs merge (section 6).
5. **Resolved 2026-10-02**: see "Decision 5 resolved" below.

## Decision 5 resolved (2026-10-02)

Ingested vectors belong to the **project that placed the cog**. The ingest
bridge forwards a cog's batches to the store owned by that project's kernel,
resolved from the placement's project id (ADR-103: a project kernel owns its
own chain and stores). When the placement has no project, the batch goes to
the placing controller's store. A placement whose project has no known store
owner is refused; it is never redirected to the controller's store, because
that would put one project's data in another's store.

**Where the vectors live (amended 2026-10-02).** On the owner daemon the
store is a `VectorDirectory`: one in-memory HNSW index per project (plus one
for the controller fallback), created on first use. The sentence above means
that store, not a project kernel's durable store. The owner daemon keeps each
index recoverable: every accepted batch is appended and synced to a capped log
`<runtime dir>/cog-ingest-vectors/<project id>.vec` (`_controller.vec` for the
fallback) before it is acknowledged, and a restarted daemon rebuilds the index
and its dedup state from that log on the project's first use. Limits: the log
is capped per project (64 MiB) and a batch past the cap is refused as store
full; there is no compaction, so repeated upserts of one id use space until the
cap; the index is rebuilt in memory, so a restart costs time proportional to the
log. Wiring the owner store to a project kernel's own durable store remains
follow-up work.

Acceptance runs for the real-hardware card use a **replayed ESP32 feed**
(recorded or synthetic packets). A live feed is optional and documented
where it is used. Mechanics and limits: `docs/cogs/ingest-bridge.md`.

Network egress for ingesting cogs is **deferred**, not solved: native
egress enforcement waits for landlock, seccomp and nftables on Linux and a
sandbox profile on macOS (follow-up); container egress is operator network
configuration following the recipe in `docs/cogs/ingest-bridge.md`. Until
then the adapters report `egress` to the gate. The ingest bridge itself is
wired into placement: the placing project id rides `PlaceOrder`,
`PlaceBody` and `PlacementRecord`, tokens are issued at place and revoked at
stop and unload.

The target host decides who may place for a project: the controller must be
the node itself, be listed for the project in the node's `cog-ingest.json`,
or be the node of the project's bound key. A placement whose vectors have no
routed store owner is refused at place time, and a node whose ingest bridge
could not start places cogs without a token and reports `ingest: disabled`.
Whatever re-creates an instance record without going through `place` (adopt,
host-restart re-adoption) must issue a new ingest lease for native and
container instances.

## Amendment (2026-10-02): `redistributable` in the signed cog manifest

The cog manifest body has an optional field `redistributable` (default false, omitted from the signed statement when false, so existing manifests and package ids are unchanged). A cog package is seeded, advertised and served over the swarm (ADR-099 section 6) only when its signer wrote `redistributable = true` and it has no Cognitum provenance (a `cognitum.*` attestation, or a release URL containing `cognitum` as a fail-closed backstop), which stays licence-gated whatever the flag says. `weaver workload pack` stamps such an attestation (`cognitum.install.provenance.v1`) when the cog dir's `provenance.json` says `trust = "cognitum-sha256"`, and refuses `--redistributable` for it. It finds `provenance.json` via `--provenance`, the cog dir, or beside a `--bin`. `--redistributable` also needs a provenance with a non-Cognitum trust whose sha256 matches a packed binary, unless the operator passes `--no-provenance-ok`. `weaver workload pack --redistributable` sets the flag. Our own weftos cogs need the flag to be shared.

**Compatibility.** `CogPackageBody` rejects unknown fields, so a verifier built before this change (v0.8.1 nodes, the cog repository, Seed tooling) rejects a manifest signed with `redistributable = true`. In a mixed-version mesh, upgrade every verifier before packing with the flag. Manifests without it verify everywhere as before.

## Amendment (2026-10-26, Proposed): memory is a reservation, enforced as resident memory

Card 7306a144. `[resources].ram_mb` does two jobs today. The planner reserves it (`mem.system` free must cover it, section 2), and the native adapter also applies it as `RLIMIT_AS` to the cog and every child (`ProcLimits::for_ram_mb`, `workload_runtime/supervise.rs`). The second use is wrong for runtimes that reserve address space they never touch. GenieX (Go) dies at start under 256 MiB of address space ("failed to reserve page summary memory") while needing far less resident memory; any `mmap`-heavy or JIT child does the same. The workaround so far is to declare `ram_mb = 65536`, which makes the scheduling reservation absurd.

**Proposal.** Split the one number by purpose.

1. **Scheduling reservation.** `ram_mb` stays what the cog expects to hold resident (expected RSS). It is the planner's `MemoryDemand` and nothing else. No manifest change.
2. **Enforcement, in order of preference per node:**
   - cgroup v2 `memory.max` on a per-instance cgroup, where the host has a delegated cgroup subtree (systemd user or service units with `Delegate=yes`). The kernel counts resident and page-cache memory, kills the cog's group on excess, and reports it in `memory.events`.
   - Otherwise an RSS watchdog in the supervisor: sample the process group's resident memory about once a second (`/proc/<pid>/status` `VmRSS` on Linux, `ps -o rss=` on macOS) and, above `ram_mb`, terminate the group and record `killed_for_memory` in the run evidence, which `weaver workload status` and the chain show as the stop reason.
   - On Linux, `RLIMIT_DATA` at `ram_mb` as a backstop under the watchdog, with the caveat that it counts private writable mappings (reserved but untouched read-write arenas count; `PROT_NONE` reservations, which Go and most allocators use, do not).
3. **`RLIMIT_AS` is opt-in only**, as a new optional manifest field `[resources] address_space_mb`, absent by default and omitted from the signed statement when absent (the same compatibility rule as `redistributable` above: verifiers built before it reject a manifest that sets it).
4. **`HOME`.** Placed cogs get no `HOME` today (only `COGNITUM_*`, `COG_CSI_BIND`, `PATH`, `WEFTOS_COGBUS*`). Recommend the host sets `HOME` and `TMPDIR` to the instance data directory, which is already the working directory and `COGNITUM_DATA_DIR`. Runtimes that write caches under `$HOME` then stay inside the instance directory and are removed with it.

**Acceptance (card).** A test cog that maps 4 GiB (`PROT_NONE`) but holds 50 MiB resident runs under `ram_mb = 256`; a cog that touches more than `ram_mb` resident is stopped and the stop is reported as a memory stop.

**Needs an owner decision before it is built.**
- Whether the watchdog kills (recommended) or only reports; a kill is a new way for a running cog to die.
- The new optional manifest field changes the signed manifest schema and the mixed-version rule above.
- cgroup v2 needs a delegated subtree on the node, so the unit files and the Terraform inventory that create the host service are part of the change, and Pi Zero and macOS nodes fall back to the watchdog.
- The test for the 4 GiB mapping only exercises the kernel path on Linux. macOS never enforced `RLIMIT_AS`, so it would pass there for the wrong reason; it has to run on the arm64 VM or a Linux host.
