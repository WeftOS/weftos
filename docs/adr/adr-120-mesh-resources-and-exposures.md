# ADR-120: Mesh resources and exposures: announce, see, switch off, log

- **Status**: Proposed (2026-10-11; owner direction)
- **Deciders**: owner
- **Builds on**: ADR-099 (governed placement; signed node facts, `ServiceAdvertisement`,
  `workload.ctl`), ADR-101 (inference kind; `expose_to_mesh`, per-model advertise opt-out,
  `infer.<role>` adverts, loopback-only binds, mesh forwarding), ADR-108 (pairing, grants),
  ADR-114 (`weftos://` names), ADR-116 (router), ADR-022 (chain audit), the dashboard OS
  redesign (`docs/plans/dashboard-os-redesign-2026-10.md`, Mesh Network page).

## Context

Owner, 2026-10-11: for the mesh, resources matter — GPUs and the libraries that go with them,
whether they are accessible, what exposing them means, logs of activity, and a way to switch
them off locally. The MacBook has strong AI hardware; a DGX Spark or other boxes may join. If a
node exposes an API for something it must announce it. Harnesses that can be kicked off are the
same: when exposed, they must be visible and configurable.

What exists (ADR-099/101): signed `NodeFacts` probe CPU, memory, storage, `accel.gpu.cuda`
(nvidia-smi: model, VRAM), `accel.gpu.metal`, `accel.npu.ane`, `accel.npu.hailo`, Coral, and
runtimes (native, Docker/OrbStack/Podman/Apple container, `llama-server`, `mlx_lm`, `ollama`).
Inference roles are advertised as `infer.<role>` only when `expose_to_mesh` is set, and the
choice is audited. Gaps: exposure covers inference only; there is no general registry of exposed
APIs or harnesses, no single local kill switch, no readable per-exposure activity log, libraries
and versions are thin, and none of it reaches the dashboard.

## Decision

### 1. Resources: what a node has
Node facts gain library and toolchain facts next to the hardware they serve, each with version
and provenance: driver and CUDA/cuDNN/TensorRT versions, ROCm, Metal family, MLX, Core ML,
ONNX Runtime execution providers, PyTorch build (CUDA/MPS), and `free` vs `total` for VRAM and
unified memory. Facts describe; they never grant access.

### 2. Exposures: what a node offers
One registry of exposures per node, generalising ADR-101's `expose_to_mesh`:

| Field | Meaning |
|---|---|
| `id` | stable name, e.g. `infer.chat`, `tts.orpheus`, `stt.whisper`, `embed.default`, `harness.metaharness`, `cog.<id>.api` |
| `kind` | `inference`, `tts`, `stt`, `embedding`, `harness`, `cog-api`, `other` |
| `resource` | what it uses (`accel.gpu.cuda:0`, `accel.npu.ane`, `cpu`) and an estimated load |
| `audience` | `local` (default), `paired` (ADR-108 peers with a grant), named nodes, or a project scope |
| `api` | protocol and path (`openai-chat`, `openai-audio`, `mcp`, `weftos-harness`), never a raw port to the world |
| `limits` | concurrency, rate, max request size, allowed hours |
| `state` | `on`, `off`, `paused` (local), plus `refused` when local policy blocks a remote request |

Nothing is exposed by default. Exposing is an explicit, audited act. Every exposure is
announced: a `ServiceAdvertisement` for peers, and a section in the dashboard report. An exposure
that is reachable but not announced is a bug, and the daemon refuses to serve it (the ADR-101
"StoppedExposed" rule generalised).

### 3. Harnesses are exposures
A harness that can be started remotely (a MetaHarness run, an agent team, a ruflo/Claude/Codex
headless job) is an exposure of kind `harness`: who may start it, with what inputs, on which
resource, with what budget. Starting one is a governed `workload.ctl` request (ADR-099), never a
shell endpoint.

### 4. Local control always wins
- `weaver expose list | on <id> | off <id> | pause <id>` and a master switch
  (`weaver expose off --all`, and `[mesh.expose] enabled = false` in `weave.toml`).
- The dashboard can request a change (a queued node action), but the node applies its local
  policy first: a locally switched-off exposure stays off and the action reports `refused`.
- Switching off withdraws the advertisement at once and refuses in-flight new requests.

### 5. Activity is logged and readable
Every request to an exposure is recorded: time, caller node and user (when known), exposure id,
size in and out, duration, outcome. The record is chained (ADR-022), kept locally with a
retention limit, readable with `weaver expose log [<id>]`, and summarised in the dashboard
report (counts and last use per exposure, never request bodies).

### 6. The dashboard shows it
The Mesh Network page shows, per node: resources (GPUs/NPUs, VRAM free/total, libraries),
exposures with audience, state and last use, and recent activity, with on/off/pause controls
that queue node actions. The report stays within the 60 KB budget (resources and exposure
summaries are compact; full logs stay on the node).

## Phases

| Phase | Delivers | Done when |
|---|---|---|
| X1 (built; defaults pending owner) | Library/toolchain facts; the exposure registry with ADR-101 inference roles migrated into it; `weaver expose` and the master switch; refusal of unannounced exposures | `weaver expose list` on the Mac shows Metal/ANE/MLX resources and the local llama/ollama roles with their audience; `off --all` withdraws every advert |
| X2 (built; defaults pending owner) | Activity log (chained, retained, `weaver expose log`); report section with resources, exposures and activity summary | a request from a peer to an exposed role appears in both nodes' logs and on the dashboard |
| X3 | Dashboard Mesh Network resources/exposures panels and queued on/off/pause actions; local-wins refusal shown | switching an exposure off in the dashboard withdraws it on the node, and a locally-off exposure shows `refused` |
| X4 | Harness exposures through `workload.ctl` with budgets | a paired node starts a MetaHarness run on the CUDA host and sees its result and log |

## Open questions for the owner

1. Default audience when an operator exposes something without saying: `paired` peers with a
   project grant, or ask every time?
2. Activity log retention: days or size, and whether caller identity beyond the node is kept.
3. Should the dashboard be able to switch an exposure **on**, or only off (on stays local)?

## Defaults pending owner (X1/X2 as built)

The three questions above are unanswered. X1 and X2 ship these defaults; each is a default
only, to be confirmed or changed by the owner:

1. **Audience**: `local`. Exposing beyond `local` is an explicit act (`weaver expose on <id>
   --audience paired|nodes:<a,b>|project:<id>`). ADR-101 inference roles that an operator
   already exposes with `expose_to_mesh` migrate as `paired` (they already name the peers
   they serve), so their behaviour is unchanged.
2. **Retention**: 14 days or 50 MB per node, whichever first. Caller identity is kept at the
   node plus a user label (the label is empty until the mesh carries one).
3. **Dashboard**: may switch exposures **off** only; **on** stays local. The queued
   dashboard action is X3; the registry itself records the acting source for audit.

## As built (X1, X2)

- **Resources**: `x.lib.*` capabilities in the signed node facts (experimental ids, so no
  vocabulary change): `x.lib.cuda` (driver, API, toolkit, cuDNN and TensorRT majors),
  `x.lib.rocm`, `x.lib.coreml`, `x.lib.mlx`, `x.lib.torch` (version, build `cuNNN` / `rocm` /
  `mps` / `default`), `x.lib.onnxruntime` (version, execution providers). VRAM and unified
  memory were already `total`/`free`; the live refresh now re-reads free VRAM per CUDA device.
- **Registry**: `clawft_kernel::exposure::ExposureRegistry`; ids, kinds, audience, api, limits,
  state `on|off|paused`. Operator decisions persist in `<runtime>/exposures.json` and win over
  configuration, so a locally switched-off exposure stays off across restarts. ADR-101 roles
  are `infer.<role>`; `PlacementTable::expose_to_mesh` registers them.
- **One serve check**: the advertisement and the serving path both ask
  `ExposureRegistry::check_serve`. An exposure that is unregistered, switched off, paused,
  audience `local`, master-off, or on without an advertisement to publish is refused
  (ADR-101's `StoppedExposed` rule, generalised). Switching off queues a `withdraw` message
  to every peer, which removes the advertisement from their tables at once.
- **CLI**: `weaver expose list | on <id> [--audience A] | off <id> | pause <id> | off --all |
  log [<id>] [--limit N]`; master switch `[mesh.expose] enabled = false` in
  `~/.weftos/weave.toml` (read at daemon start; an invalid section fails closed).
- **Activity**: `<runtime>/exposure-activity.jsonl`, SHA-256 hash-chained, never bodies;
  the chain tip is anchored on the ExoChain on every refusal or error and every 64th record.
- **Report** (`report.resources`, `report.exposures`): see `dashboard_budget.rs`; trimmed after
  `routes` and before `pair_requests` (resources: library list first; exposures: item rows).

