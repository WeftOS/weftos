# Turntable scan: a chain of single-purpose cogs, with the console as the demo

- **Status**: Plan, revised 2026-10-27 (owner: one cog per job, composed over
  topics; the console shows the chain working; NPU, an agent, model placement
  across the mesh, a capture coach, a live pipeline graph, a demo script).
  Not started. No tickets yet.
- **Relates-To**: ADR-122 (cog data plane: the broker in the node daemon,
  `[[inputs]]` / `[[outputs]]`, topic names, classes, cross-node exposure),
  ADR-120 (mesh exposures, activity log), ADR-099 (governed placement,
  `clawft_types::placement`), ADR-100 (cog workload kind), ADR-101
  (inference kind, role-based provider resolution, `InferProxy`), ADR-104
  (guides, `weftos-cog-companion`), ADR-077/078/056 (capture UX, world
  model, BVH), `docs/plans/ventuno-q-cog-plan.md` (device owners, NPU
  service, media cogs, phases P0–P8), `docs/architecture/sensor-node.md`.

## 1. Ground truth (verified 2026-10-27)

- **Rig**: fixed USB camera (1920x1080 MJPEG ~30 fps) over a free-running
  motorised turntable (buttons R/L, speed SR, no control interface) with a
  printed marker pattern; object: a long thin PCB. `venq-camview` (std-only
  Rust, on the board, not in a repo) serves `/stream.mjpg`, `/snapshot.jpg`,
  `/status`, `/ctrl` (V4L2), `/config`, `/record` (frames + `manifest.json`
  with per-frame `unix_ms`), `/record/status`, `/record/stop`.
- **Board**: Ventuno Q on the tailnet, 8-core aarch64, 14 GiB, Adreno 623
  (turnip Vulkan 1.3, Qualcomm OpenCL ICD), Hexagon NPU, CPU-only COLMAP,
  OpenCV 4.6. No CUDA.
- **NPU, as installed**: GenieX (libgeniex + QAIRT) with models
  `google/gemma-4-E2B` (Q4, ~20 tok/s), `qualcomm/qwen3_4b_instruct`
  (W4A16, 15 tok/s, 11/12 tool calls in the bench),
  `qualcomm/qwen3_vl_4b_instruct` (VLM, 12 s/item, 0.5 s first token),
  `qualcomm/qwen2_5_vl_7b_instruct`, small Qwen3.5/Gemma-3 GGUFs. Gemma 4
  E4B is OOM on this board. App Lab brick images present: Edge Impulse QNN
  models runner (object detection; YOLOX nano ships on the board), pose,
  gesture, llama.cpp NPU runner, the Qualcomm GenAI LLM/VLM service. No
  segmentation model is installed.
- **Cog data plane, as built**: the `npu-infer` cog already runs on the
  board and answers `svc.infer/venq-npu` calls over the cogbus (ADR-122:
  Unix control socket in the node runtime dir, length-prefixed JSON,
  governance-gated declare/publish/subscribe, every call in the node's
  hash-chained activity log). P1 of the Ventuno plan landed `[[outputs]]` /
  `[[inputs]]` in `cog.toml` with schema ids under `contracts/cogs/`. B1
  (local broker) and B4 (ingest bridge as a consumer) shipped; cross-node
  topic delivery (ADR-122 §8) is designed, not yet verified shipped. ADR-122
  is on `target-0.8.4`, not this branch.
- **Placement, as built**: `clawft_types::placement::{place,
  PlacementRequest, Requirement, Decision, explain}` with dotted capability
  ids (`accel.npu.qualcomm`, `accel.gpu.metal`, `runtime.infer.llamacpp`,
  `format.gguf`, ...); ADR-101 §5 `PlacementResolver` / `PlacedProvider`
  in `clawft-llm` resolve a role to an endpoint; `clawft-core::placement_hook`
  installs it for the `local` provider; `InferProxy` is one loopback listener
  per role forwarding `/v1/*`. A cog host for x86_64 exists (`cog-host-x86`
  lane).
- **Agent pieces, as built**: `clawft-core::tools::registry::Tool` (`name`,
  `description`, `parameters` JSON schema, `execute`) and `ToolRegistry`;
  `clawft-core::agent::loop_core`; `clawft-tools` (shell, files, web, voice,
  delegate, spawn); `clawft-llm::{Provider, ProviderRouter, LocalProvider}`
  (prefix routing: `local/` llama.cpp `:8090`, `ollama/` `:11434`, OpenAI
  compatible); `clawft-services::delegation` routes Local vs Claude by rule
  and complexity.
- **Graph UI, as built**: `clawft-gui-egui::explorer::viewers::graph::GraphViewer`
  (`ui://graph` primitive; value `{nodes:[{id,label,kind,pos}],
  edges:[{source,target,kind,label}]}`; hand-rolled egui painter, read-only,
  given positions or a circle layout; `egui_graphs` rejected for petgraph
  and the wasm budget); `weftos-cog-manager::node_tree` and the Network tab
  fed by `weftos-cog-host` `GET /mesh/cogs` (which nodes run which cogs, at
  what version and state) and `GET /network`; ADR-067 conversation graph
  (timeline-scrubbed, not needed here). No `namespace_drops` example and no
  Trajectory Atlas code exist on this branch.
- **Conventions**: cogs are standalone crates in the public `weftos-cogs`
  repo, `src/cogs/<id>/{Cargo.toml,cog.toml,guide/,src/,README.md}`, one
  `docs/adrs/ADR-NNN-<cog>.md` each (latest ADR-166), `scripts/gate.sh`;
  cogs speak the cogbus protocol themselves rather than linking WeftOS
  crates (as `npu-infer` does). Companion apps are WeftOS crates
  `crates/weftos-<name>-scope` on `weftos-cog-companion::SensorApp`, built
  with `scripts/build.sh scope <name>` / `scope-web <name>`.

## 2. The chain

Each cog does one thing, declares its inputs and outputs in `cog.toml`
(ADR-122 names: `<kind>/<source_id>`; on the mesh `mesh.scan.v1.<kind>`
on `weftos-sensor-pipeline-wire`'s signed framing), ships its own guide and
is placed on its own.

```
                 Ventuno board (CPU)                         Ventuno NPU         elsewhere (placed)
 UVC cam ─► camera-source ─ video.frame-ref ─┬─► turntable-tracker ─ scan.turntable ─┐
            │  camera.status, scan.record     ├─► frame-quality ───── scan.quality ───┤
            ▲  camera.control                 ├─► segmenter ───────── scan.mask/segments ◄── (QNN seg model)
            │                                 ├─► scale ───────────── scan.scale ─────┤
            │                                 └─► describer ────────► svc.infer/venq-npu (qwen3-vl-4b)
            │                                           scan.description               │
            │     coverage ◄── turntable, quality, record, sparse pose ── scan.coverage, scan.passplan
            │     capture-coach ◄── coverage, quality, segments, scale, turntable ── scan.coach
            │     photogrammetry-sparse ◄── frame-ref, mask, turntable ── scan.sparse (+ model blob)
            │                                                               │
            │     modeller-dense ◄── sparse model + frames by handle ───────┴─► CUDA host (accel.gpu.cuda)
            │                       scan.dense (+ dense/mesh/splat blobs)
            │     world-model-export ◄── mesh, scale, description ── wm.object (BVH leaf, ADR-078)
            └──── scan-agent ◄── all readings; tools → camera.control, scan.control; model: NPU → Mac → CUDA
                  console (Mac, egui) ◄── every reading + blobs by handle; layers; agent panel; pipeline graph
```

Local-only by default: `video.frame-ref` (blob by handle), `scan.mask`
and the model blobs. Readings (≤ 4 KB) may cross the mesh as exposures.

### 2.1 Cog table

| Cog | Owns / does | Inputs | Outputs (kind, class) | State | Placement, hardware | Guide summary |
|---|---|---|---|---|---|---|
| `camera-source` | the UVC camera (replaces `venq-camview`); MJPEG child, V4L2 controls, recording to frames + manifest; HTTP export for the console (`/stream.mjpg`, `/snapshot.jpg`, `/ctrl`, `/record*`, `/scan/<run>/<file>`) | `camera.control` (reading) | `video.frame-ref` (blob, local), `video.stream.mjpeg` (stream, local), `camera.status`, `scan.record` (readings) | frame ring, record manifest, runs under the cog data dir | `media.camera.usb`; one owner per device (Ventuno plan §4.1); CPU | camera on USB 3, height/tilt, exposure lock, WB, 409 while recording |
| `turntable-tracker` | platter rim ellipse (hand-placed or fitted), table plane, camera elevation (`sin(el)` = axis ratio), θ from a measured period (S1) then from polar cross-correlation of the unwrapped platter texture (S2), period drift | `video.frame-ref`, `scan.control` (mark, fit points) | `scan.turntable` {ellipse, elevation_deg, axis, θ, θ_source, period_s} | reference angular profile, fit per run | same node as the camera (frames are local-only); CPU, ~5 ms/frame at 640 px | platter diameter, marker pattern, measuring the period, what the ellipse tells you |
| `frame-quality` | per-frame blur (Laplacian variance), clipped-pixel fraction, specular blobs with their platter-side location, θ-step overlap | `video.frame-ref`, `scan.turntable` | `scan.quality` per frame | last N scores | same node; CPU (an NPU model buys nothing here) | what a bad frame looks like, exposure lock, diffusing light |
| `segmenter` | part vs platter vs background masks, part bbox, centroid offset from platter centre, area; the mask feeds sparse and coach | `video.frame-ref`, `scan.turntable` | `scan.mask` (blob PNG, local), `scan.segments` (reading) | background model per run | same node; **NPU** via a QNN segmentation model (not installed: compile through AI Hub, licence check) with a **CPU fallback** that works because the camera is fixed: median background over a revolution, rotating region minus the known platter texture | the three regions, why static background must be masked, choosing the model |
| `scale` | mm per pixel on the platter plane from marker pitch, a ruler, or the platter diameter; scale bar; dimensions of the segmented part | `video.frame-ref`, `scan.segments`, config | `scan.scale` {mm_per_px, source, confidence}, `scan.measure` on request | calibration per run | same node; CPU (ArUco/AprilTag-class decode or a known-diameter circle); the YOLOX detector only if a ruler must be found in clutter | printing the marker sheet, entering the platter diameter, reading a measurement |
| `coverage` | azimuth × pass bins from θ(t) and the record manifest, replaced by solved poses when sparse exists; missing bins; the pass plan (+20°, +45°, flip for a flat part) | `scan.turntable`, `scan.quality`, `scan.record`, `scan.sparse` | `scan.coverage` {bins, passes, current, missing}, `scan.passplan` | bins per run | any node that can reach the readings; CPU, trivial | passes, bins, why 36 views, flipping the part |
| `capture-coach` | prioritised physical instructions with evidence and a confirming check: raise/lower/tilt camera (from elevation vs plan), move light / diffuse (specular location), flip the part, centre the part (segment centroid), slow the table (θ-step), exposure | `scan.coverage`, `scan.quality`, `scan.segments`, `scan.scale`, `scan.turntable`, `scan.sparse` (pose) | `scan.coach` [{id, priority, text, evidence, check, state: open/confirmed/stale}] | open instructions, confirmation windows | any node; rules on CPU; phrasing through `svc.infer` (local LLM) is optional and cached | the instruction catalogue, how a check confirms, the optional actuator path |
| `photogrammetry-sparse` | frames + masks + θ → poses and a sparse cloud: `colmap` when present (mask from `scan.mask`, subsample one frame per 5° by quality, sequential matcher with loop pairs, mapper) and the built-in axis solver (`pose_i = R_axis(θ_i)·pose_0`, triangulate, short BA with poses on the axis) for the live cloud | `video.frame-ref`, `scan.mask`, `scan.turntable`, `scan.control` (start) | `scan.sparse` {job, stage, n_registered, n_points, model_handle}, `scan.model.sparse` (blob: COLMAP bin bundle + poses) | job record (`clawft-splat-pipeline` `JobRecord`/`Stage` shape, vendored) | same node as frames; `tool.colmap` optional; CPU, 72 images ≈ 5–10 min, live cloud under the 3 fps record rate | subsampling, masks, what registered/points mean, `no_tool` |
| `modeller-dense` | the 3-D modeller: sparse model + frames by handle → dense cloud, mesh, optional splat; writes blobs back | `scan.model.sparse`, frames by handle, `scan.control` | `scan.dense` {job, stage, timings}, `scan.model.dense`, `scan.model.mesh`, `scan.model.splat` (blobs) | job record | **placed by ADR-099**: `accel.gpu.cuda` + `tool.colmap` (dense/mesh), `accel.gpu.vulkan` + `tool.brush` (splat); the mesh's CUDA host (RTX 4070, 12 GB); never the board | why dense needs CUDA, timings, what a mesh vs splat is for |
| `describer` | a short description of the part (type, visible connectors, markings) from one frame + the segment | `video.frame-ref` (one), `scan.segments`, `scan.control` (ask) | `scan.description` {text, model, placement} | cache per run | `svc.infer/venq-npu` with `qwen3-vl-4b` (~12 s); fallback: a VLM on the Mac controller or the CUDA host through role resolution | what the model sees, honesty about hallucination, choosing a frame |
| `world-model-export` | ADR-078 structure stage: scene AABB, object leaf with metric scale, provenance (run, cogs, versions), BVH leaf publish | `scan.model.mesh`, `scan.scale`, `scan.description` | `wm.object` reading + the leaf via the daemon | none | any node with the daemon's BVH; CPU | what the world model keeps, scale, provenance |
| `scan-agent` | plain-language control: a tool loop over the chain ("scan this part", "what angles am I missing?", "raise the camera?", "measure the board"); explains each step | every `scan.*` reading, `agent.turn` requests | `agent.event` (tool calls, answers), `camera.control`, `scan.control` | conversation per run | **local model first** (`svc.infer/venq-npu`, `qwen3-4b` or `gemma4-e2b`), fallback by placement to the Mac controller (llama.cpp `:8090`, Ollama `:11434`) or the CUDA host through the ADR-101 role listener; decision published in `agent.event` | the tools, what the agent may and may not do, where the model runs |

### 2.2 Topic contract (`contracts/cogs/scan.*.v1`)

Every reading carries `source_id`, `node_id`, `t_ns`, `seq`, `run` and
`schema`. Golden vectors per schema in `contracts/cogs/`, as P1 requires.
Blob handles are `{hash, bytes, mime, ttl_s}`; the bytes come from the
source node's content store or the owning cog's `/scan/<run>/<file>`.
The console's wire crate gains a `ScanTopic` enum next to `SensorTopic`.

### 2.3 Manifest shape (every cog)

```toml
[cog]            id, name, version, category = "scan", binary = "cog-<id>-arm64"
hardware_requirement = ["media.camera.usb"]      # camera-source only; others name interfaces or nothing
interfaces = ["npu"]                             # segmenter, describer, scan-agent (optional)
[[inputs]]       kind = "video.frame-ref", schema = "video.frame-ref.v1", class = "blob"
[[outputs]]      kind = "scan.turntable",  schema = "scan.turntable.v1",  class = "reading", rate = 30
[config.*]       typed, cli_arg, advanced
[api]            bind_port = 806x               # the latest reading, /guide, /scan files (camera-source)
[console]        allowed_commands = ["--once --simulate", "--help"], max_runtime_secs
[resources]      ram_mb, cpu_pct
[placement]      requires = ["accel.gpu.cuda", "tool.colmap"]   # modeller-dense; see gap G3
```

`--simulate` on every cog: `camera-source` renders a synthetic platter
(configurable tilt, radial marker pattern, a box part) rotating at a set
period; the others consume it, so the whole chain runs on a Mac with no
hardware and the companion's test run passes.

## 3. NPU and CPU: what fits where

| Task | Where | Why |
|---|---|---|
| Part / platter / background masks | NPU (QNN segmentation model via `svc.infer`), CPU fallback | the only per-frame vision job heavy enough to pay for the NPU; the fallback is strong because the camera and background are static |
| Frame quality | CPU | three cheap filters; no model needed |
| Marker / scale detection | CPU | classical decode; a detector only to find a ruler in clutter |
| Describe the part | NPU, `qwen3-vl-4b`, 12 s per image | on demand, once per run |
| Agent turns | NPU, `qwen3-4b` (tool calls measured) or `gemma4-e2b` | local first; placement moves heavier reasoning to the Mac or CUDA host |
| Coach phrasing | NPU LLM, optional | rules decide, the model only words it |
| Sparse SfM | CPU | COLMAP's GPU SIFT is not this ICD; the axis solver is cheap |
| Dense / mesh / splat | placed off-board | CUDA for COLMAP dense; Brush on Adreno via turnip unproven |

The NPU is one service (`npu-infer`, Ventuno plan P4): cogs call it, they
never open GenieX themselves. Model residency and the memory budget are its
problem; the chain's VLM and LLM fit together (6 GB + 4.5 GB under the
8 GB default budget only if one is evicted: the agent should prefer
`gemma4-e2b` while the describer holds the VLM, or run the describer once
and release).

## 4. Model and job placement across the mesh (ADR-099)

Placement is the demo, not a detail. The console shows every decision.

- **Facts**: the Ventuno publishes `accel.npu.qualcomm` with loaded models
  and queue depth (P0/P4), `media.camera.usb`, `tool.colmap`; the Mac
  controller publishes `runtime.infer.llamacpp` / `runtime.infer.ollama`
  with its models (ADR-101 adapters); the CUDA host publishes
  `accel.gpu.cuda` with 12 GB and `tool.colmap` / `tool.brush`.
- **Roles**: `llm.scan-agent`, `vlm.describe`, `llm.coach-phrase`,
  `job.dense`, `job.splat`. Each cog asks the daemon to resolve a role; the
  daemon runs `place()` with the cog's `Requirement`s, affinity for the
  board (`local first`), and returns a `Decision` plus `explain(d)`. The
  cog publishes `{role, node, capability, reason}` in its reading
  (`scan.description.placement`, `agent.event.placement`,
  `scan.dense.placement`).
- **Local first, then fallback**: the NPU for the VLM and the agent LLM;
  the Mac (llama.cpp `:8090`, Ollama) when the NPU is busy, the model is
  evicted, or the task is marked heavy; the CUDA host for dense, splats and
  the 7B VLM. The agent's "escalate" is `clawft-services::delegation`'s
  Local vs Claude rule set reused with a `Mesh` target.
- **Transport**: NPU calls go over the cogbus (`svc.infer`); off-board
  models go through the node's ADR-101 `InferProxy` role listener
  (OpenAI-v1 on loopback, allowlisted paths), so a cog never holds a remote
  URL or a key.
- **Jobs**: `modeller-dense` is placed as a cog onto the CUDA host; its
  inputs arrive by handle, its blobs come back the same way (gap G1/G3).

## 5. The console (`weft-scan-scope`, ADR-104 companion)

Crate `crates/weftos-scan-scope` (window title "Scan console"),
`SensorApp` on `weftos-cog-companion`, extended for a chain (gap G7):
`cog_id` becomes the chain's anchor (`camera-source`) and the shell learns
the other cogs from the node's `/mesh/cogs`. Native first; wasm view-only.

**Layers** (each enabled + opacity): L0 live video (`/stream.mjpg` native,
`/snapshot.jpg` wasm); L1 turntable model and θ sweep (`scan.turntable`);
L2 coverage ring and sphere (`scan.coverage`); L3 masks (`scan.mask`) and
part box (`scan.segments`); L4 quality: bad-frame marks, specular heat
overlay (`scan.quality`); L5 scale bar and measurements (`scan.scale`);
L6 sparse points reprojected (pose nearest in θ, then `R_axis(θ)`) and in
a glow `PaintCallback` 3-D pane (`scan.model.sparse`); L7 dense/mesh/splat
(`scan.model.*`); L8 coach overlays: arrows for camera moves, ghost outline
for target part placement, heat for glare, a checklist with evidence and
check state (`scan.coach`); L9 description card; L10 record / ctrl / job
strip.

**Agent panel**: a chat with `scan-agent`; every tool call rendered as a
step ("recording 90 s at 3 fps → camera-source", "reading coverage: 7 bins
missing at 200–270°") with the placement badge ("qwen3-4b on Ventuno NPU",
"escalated to Mac llama.cpp: long plan"). Coach instructions are spoken or
shown here too.

**Pipeline graph panel (S1)**: the live cog workflow.
- Nodes: one per cog, badge with host and compute (Ventuno NPU / CPU, Mac,
  CUDA host), version, health/state from `/mesh/cogs` and the cog's
  `/status`; the agent and the console are nodes too.
- Edges: the topics, from `weaver cogbus list` (writer, readers, rate,
  drops) and the readings' `t_ns` (latency); label = last payload schema.
- Click a node: its guide (ADR-104, the shell's renderer), manifest,
  recent log tail, and the placement decision (`explain(d)` text).
- Click an edge: the schema from `contracts/cogs/`, a live sample.
- Live flow: pulses travel along an edge when a message arrives; the
  agent's tool calls flash agent → target; a placement that moves a job to
  the CUDA host animates the node moving between host lanes.
- Reuse: lift `GraphViewer`'s painter and `graph_adapter` model out of
  `clawft-gui-egui` into `weftos-cog-companion::widgets` (or a small
  `weftos-pipeline-graph` crate both can use), keep its `{nodes, edges}`
  value shape, add fixed layered positions (the chain is a DAG), host lanes,
  badges and edge pulses. Data from `weftos-cog-host` `/mesh/cogs` and
  `/network` (the Network tab's sources in `weftos-cog-manager::node_tree`)
  plus the cogbus listing. `egui_graphs` stays out for the same reasons
  `GraphViewer` gave.

Model code in `src/model/` with unit tests; UI thin (as `weftos-ecg-scope`).

## 6. Timing expectations on the board

| Path | Expected | Note |
|---|---|---|
| tracker, quality, scale per frame | 5–30 ms at 640 px working size | CPU |
| segmenter CPU fallback | 20–60 ms/frame | background model after one revolution |
| segmenter NPU | model dependent, tens of ms | model not installed yet |
| describer | ~12 s per image, 0.5 s first token | measured |
| agent turn | 2–8 s at 15–20 tok/s | tool calls add a round each |
| sparse, 72 frames, COLMAP CPU | 5–10 min | exhaustive on 270 frames is 30–90 min: subsample |
| axis solver live cloud | under the 3 fps record rate | the "running 3-D model" |
| dense + mesh, 72 frames, RTX 4070 | 5–15 min; 36 frames 2–4 min | never on the board |
| splat (Brush) on the 4070 | 10–20 min | optional |

## 7. Slices (each a working chain and demo)

### S1: the smallest chain and the graph

- weftos-cogs: `camera-source` (from `venq-camview`, plus cogbus declare /
  publish of `video.frame-ref`, `camera.status`, `scan.record`, `/scan`
  file routes, CORS header, `--simulate`), `turntable-tracker` (hand-placed
  ellipse, period θ), `coverage` (bins, missing, pass plan); three guides,
  three ADRs (ADR-167..169), schemas `video.frame-ref.v1`,
  `scan.turntable.v1`, `scan.record.v1`, `scan.coverage.v1`,
  `scan.passplan.v1`, `scan.control.v1`, `camera.control.v1` with golden
  vectors; gate green; installed on the Ventuno beside the running
  `venq-camview` (other port) until the current COLMAP run ends.
- WeftOS: `crates/weftos-scan-scope` with L0, L1, L2, L10, the pipeline
  graph panel and the chain-aware shell extension; catalog entries (and a
  `scan` chain entry, gap G4); `scripts/build.sh scope scan` unchanged.
- Acceptance: on the board, three cogs appear in the graph with host, version
  and health; frames pulse camera → tracker → coverage; the live view, the
  ellipse handles, the period mark and the coverage ring work during a
  recording; clicking a node shows its guide and manifest; the same console
  against `--simulate` on the Mac shows all of it with no hardware.
- Tests: MJPEG splitter, ellipse fit, elevation from axis ratio, θ from
  period, bin seam, record manifest, cogbus frame codec, schema golden
  vectors, graph layout and pulse timing, layer opacity math.

### S2: quality, masks, scale, sparse

- Cogs: `frame-quality`, `segmenter` (CPU fallback; the QNN model as its own
  card), `scale`, `photogrammetry-sparse` (`colmap` stage + axis solver),
  tracker upgraded to platter tracking.
- Console: L3–L6, the 3-D pane, θ-source switch; edges for the new topics.
- Acceptance: a run recorded from the console yields masks that exclude the
  background, a sparse model whose points land on the PCB in the live view
  within a few pixels while the table turns, a live cloud growing during
  the recording, and a scale bar with the board's length within 2 %.
- Tests: COLMAP binary reader on a tiny fixture, projection round trips,
  axis fit, tracker within 1° on the simulated pattern, subsample by bin,
  background model on synthetic frames, marker decode fixtures.

### S3: coach, agent, describer (NPU)

- Cogs: `capture-coach` (rules table, evidence, checks), `scan-agent`
  (tool loop over the chain; local model via `svc.infer`; placement
  fallback through the role listener), `describer` (`qwen3-vl-4b`).
- Console: L8 coach overlays and checklist, L9, the agent panel with tool
  steps and placement badges; graph flashes for tool calls.
- Acceptance: "scan this part" records, fits, and reports missing bins;
  the coach says "raise the camera about 10 cm and tilt to ~45°" with the
  elevation evidence, and marks it confirmed when the next frames show it;
  a glare instruction appears when a lamp is pointed at the platter and
  clears when it is diffused; the describer names the part as a PCB with
  its placement badge; pulling the NPU (stop `npu-infer`) makes the agent's
  next turn show "placed on Mac llama.cpp" and still work.
- Tests: coach rule table with evidence and check windows; agent tool
  contracts (JSON schema per tool, mirrored from `Tool::parameters`);
  placement decision serialisation and `explain` text; describer prompt and
  cache.

### S4: dense on the CUDA host, world model, the demo

- Cogs: `modeller-dense` (x86_64 package, placed on the CUDA host, blobs by
  handle both ways), `world-model-export`.
- Console: L7, the placement card for the job, the BVH entity view.
- Acceptance is the demo script (§8) run end to end.
- Tests: PLY reader fixtures, job record serde, placement request for
  `accel.gpu.cuda`, world-model export fixture, leaf round trip.

## 8. Demo script (3–5 minutes; acceptance for S4)

1. **Install from the catalog** (0:00–0:40): in the cog manager, pick the
   `scan` chain; cogs land on the Ventuno; the console's graph shows each
   node appear and turn green with version and host.
2. **Guide** (0:40–1:00): click `camera-source`, read the placement page;
   click an edge, see `video.frame-ref.v1`.
3. **Live view and fit** (1:00–1:30): stream on; drag the five rim handles;
   elevation reads +22°; the θ hand sweeps as the table turns.
4. **Agent-driven scan** (1:30–2:30): "scan this part". Tool steps appear;
   recording runs; the coverage ring fills; the coach asks for a camera
   raise, the presenter raises it, the check confirms; "flip the board"
   after pass two; masks and the scale bar show on the live view.
5. **NPU** (2:30–3:00): the describer card fills ("a long thin PCB with ...",
   badge: qwen3-vl-4b on Ventuno NPU); the agent's turns carry the NPU
   badge; stop `npu-infer` and the next turn shows the Mac fallback.
6. **Sparse and placement** (3:00–3:45): the live cloud sits on the board in
   3-D; "model it": the placement card lists candidates (Ventuno: no CUDA;
   Mac: no CUDA; CUDA host: `accel.gpu.cuda` 12 GB, `tool.colmap`), the job
   node moves to the CUDA lane, blobs flow back; the mesh overlay appears
   (for a live demo use 36 frames, 2–4 min, or show a pre-run result while
   the real one runs).
7. **World model and measure** (3:45–4:30): "measure the board" returns
   length and width with the scale source; export; the object appears as a
   BVH entity with provenance that another viewer can query.

## 9. Platform gaps to close (and the interim path)

| # | Gap | Where | Interim |
|---|---|---|---|
| G1 | Cross-node topic delivery (ADR-122 §8: exposure kind `topic`, remote subscribe, credits) not verified shipped | daemon broker | the console and off-board cogs read each cog's HTTP export (latest reading, `/scan` files by handle); `scan.*` readings are published locally from day one so nothing changes when §8 lands |
| G2 | Container and VM cogs cannot reach the broker socket (ADR-122 gap 1); publish rate limits (gap 5) | broker | all chain cogs are native; declared `rate` in `[[outputs]]` |
| G3 | Placing a cog on a GPU host: x86_64 cog packages (ADR-100 "later"), a cog host on the CUDA host (Windows + WSL), `accel.gpu.cuda` / `tool.colmap` / `tool.brush` in the capability vocabulary (`clawft-types/src/placement/vocabulary.rs`), run-to-completion job cogs vs `[console] max_runtime_secs` | packages, cog-host, vocabulary | `cog-host-x86` lane exists; add the capability ids; `modeller-dense` runs with `--once` under a job budget; until the host is up, the console's "place on <node>" calls the same cog on the Mac for a CPU dense (slow) or skips to a pre-run result |
| G4 | The catalog has no chain / pipeline entry; guides and installs are per cog | `weftos-cog-market` catalog, cog manager | add a `chain` record listing cogs, topics and the anchor cog; install = N installs |
| G5 | No segmentation model on the NPU; licence check for any AI Hub model | NPU service | CPU fallback is the S2 default |
| G6 | `scan.*` schemas and golden vectors do not exist | `contracts/cogs/` | written in S1 |
| G7 | `weftos-cog-companion` assumes one cog (`cog_id`, `export_port`) | companion shell | chain-aware extension: anchor cog + `/mesh/cogs` discovery, one `CogClient` per cog |
| G8 | Cog-to-cog placement affinity ("frame consumers on the camera node") exists only as "inputs reachable" (ADR-122 §10) | placement | local-only blob classes make it fall out; verify `place()` refuses a remote frame consumer |
| G9 | NPU calls from off-board agents: GenieX is loopback-only and `svc.infer` is a cogbus service | `npu-infer`, `InferProxy` | the agent runs on the board as a cog; the Mac `weft agent` can drive the same tools over the exports but cannot use the NPU until a role listener fronts `svc.infer` |

## 10. Risks

- Dense needs CUDA and a GPU host that can run a cog (G3): the biggest
  schedule risk; S1–S3 do not depend on it.
- NPU memory: VLM + LLM do not both stay resident under the default budget;
  sequence them or accept a reload.
- Marker pattern type unknown: tracking does not decode it; scale may need
  the platter diameter typed in.
- A static background breaks turntable SfM: masks always come from the
  segmenter or the tracker's ellipse, never a hard-coded one.
- Coach checks can oscillate (raise, then lower): hysteresis and one open
  instruction per category.
- Actuators (GPIO light, IR turntable, pan-tilt) are governance-gated effects
  on `scan.control`, after the chain works by hand; not in these slices.
- The console as remote subscriber is HTTP polling until G1; fine at the
  demo's rates.

## 11. Open owner decisions (defaults in bold)

1. Repo placement: **all cogs in `weftos-cogs` under `src/cogs/<id>/`, one
   ADR each; console `weftos-scan-scope` in WeftOS; schemas in
   `contracts/cogs/`.**
2. Agent host: **a `scan-agent` cog on the board** (local NPU model first,
   placement fallback); the Mac `weft agent` with `ToolRegistry` as a second
   client of the same tool contracts later.
3. Segmenter: **CPU fallback first; a QNN model behind its own card** after a
   licence check.
4. Dense placement: **`modeller-dense` as a placed cog on the CUDA host;**
   OpenMVS on the board only if no CUDA node is reachable.
5. Camera ownership: **`camera-source` is the owner cog** (it is the
   Ventuno plan's `camera` cog for USB; the CSI variant is a config).
6. Turntable control: **none; measured period.** IR/GPIO actuators later,
   governed.
7. Pass plan default for the PCB: **+20°, +45°, flip, 36 bins.**
8. Browser console: **S3, view-only.**
9. Chain catalog entry: **add a `chain` record in S1** (G4) rather than
   installing cogs one by one in the demo.
