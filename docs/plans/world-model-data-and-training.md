# World model: data, labels and weights (plan)

- **Updated:** 2026-10-26 (W4 harness status in §4).
- **Status:** Accepted 2026-10-07 (owner decisions in §8). Builds on RuView ADR-384 and the
  sensor-contracts plan (§1a). W7 is undecided.
- **Asks:** "Do we have a plan to build the weights and labels we need, and does H-JEPA add
  anything?" Short answers: **no plan existed**, and **yes, H-JEPA helps**, mainly as the
  reference trainer and evidence for a two-level predictor. It does not solve our data problem.
- **Builds on:** RuView ADR-384 (external sensor evidence and readings ingest, Proposed
  2026-10-05) and `docs/design/sensor-contracts-plan.md` on aepod/RuView branch
  `docs/adr-384-sensor-contracts`, ADR-090 (LeWM / ECC decoupling, Accepted), the LeWM crates
  `weftos-worldmodel-core` / `-impls` / `weftos-worldmodel` / `clawft-worldmodel-service`
  (WEFT-519..532), ADR-107 (spatial evidence engine), ADR-110 (agent transcripts),
  RuView's own training ADRs (070, 071, 079), and the research notes
  [h-jepa.md](../research/h-jepa.md) and [jepa-anything.md](../research/jepa-anything.md).

## 1. Where we are (verified 2026-10-07)

| Piece | State |
|---|---|
| Architecture (192-d SIGReg latent, `pred_φ`, CEM planner at 10 Hz, four-condition rollback gate, two training surfaces, RVF model segments, hot-swap) | Built, **stubs only**. `weftos-worldmodel-impls/README.md`: "no ML weights"; the candle ViT-tiny / AdaLN path is a skeleton returning `Unavailable`. |
| Training | **On hold** since 2026-09-28 (owner, Skill-3D card 65f8a922). This plan asks to lift the hold for the sensor world model only. |
| Recorded sensor data | **None.** RuView `data/recordings` and `v2/data/recordings` are empty; the spatial workspace ships only `demo-room.synthetic.jsonl`. The first real room capture and the first hardware-backed live frame are still open cards (61020daa, f491f12e). |
| Long-running logs we do have | ExoChain (51 MB on the Mac, about 89 k events), agent sessions (ADR-110 will collect them), governance `TrajectoryRecorder`. |
| Labels | None recorded. |
| Sensor classes in code | `SensorClass` = generic, RGB-D, IMU, proprio, LiDAR, audio. **Our real sensors are missing:** mmWave radar (LD2450, LD6002), Wi-Fi CSI (RuView), ToF, ECG / biopotential, environmental. Fix by following ADR-384's HAL mapping (§1a), not new names. |
| RuView ingest (ADR-384) | Proposed; steps 1-3 are planned PRs on aepod/RuView, nothing merged. Today the server accepts only `rf_gaussian` / `rf_link_observation` and depends on none of the evidence, HAL, fusion or labeller crates. |
| Training hardware | MacBook M5 Max, 128 GB (Metal/MPS). photo-gallery: 16 cores, 188 GB, **no GPU** (storage and CPU preprocessing only). RuView ships `gcloud-train.sh` and `mac-mini-train.sh`. |
| RuView training pipeline | Designed, unused: ADR-070 self-supervised pretraining and collection protocol, ADR-071 training pipeline (Proposed), ADR-079 camera ground truth (Accepted); `scripts/collect-training-data.py`, `collect-ground-truth.py`, `align-ground-truth.js`; crates `homecore-recorder`, `wifi-densepose-train`. |

## 1a. Who owns what (RuView ADR-384 and the cog plan)

ADR-384 §1 already splits the work, and this plan follows it rather than building a parallel
pipeline:

| Owner | Role in the world-model data path |
|---|---|
| **Cogs** (producers, moving out of weftos into the cog repo) | Read sensors. Radar cogs write `spatial.evidence.v1` (`ld2450-radar --spatial-out file|export`, `GET /spatial`; `ld6002-radar` file only); non-spatial readings go out as **SenML** (RFC 8428) with `proof_`. Later shipped as signed RVF producers (planned ADR-386). |
| **RuView** | Ingests evidence and readings by push, pull or file (ADR-384 §6), keeps a bounded store, fuses people, objects and geometry into **its** world model (ADR-306, symbolic, with evidence levels), and **records sessions**: `<data-dir>/recordings/<session>.evidence.jsonl` next to the CSI recording (§8). Its labeller (`ruview_groundtruth::auto_label` over `windows_from_evidence`) and calibration-status diagnostics are the first consumers (§9). |
| **WeftOS** | Owns the evidence schema (ADR-107) and **long-term history** (out of RuView's scope by ADR-384 §1). For training that means: archive finished sessions on photo-gallery as a versioned dataset, and own the learned model (**LeWM**). |

Two things called "world model" now exist and must stay distinct: RuView's fused, symbolic
world model of a space, and WeftOS's **learned latent predictor** (LeWM). LeWM consumes the
same evidence, readings and CSI that RuView ingests, and RuView's fused output as further
observations. Its predictions are model output: they carry proof `CODE`, so under ADR-384 §7
they can never become ground truth, calibration input or training labels, and under ADR-090
they never override ECC.

Consequences for this plan:

- **No new WeftOS recorder.** Recording is ADR-384 step 2. WeftOS adds only a dataset
  archiver (session → manifest → photo-gallery) and the training side.
- **Observations are the contracts, not ad-hoc sensor classes.** LeWM's inputs are
  `spatial.evidence.v1` record types, SenML readings by quantity, and CSI frames (ADR-385
  frame contract). `SensorClass` is extended to match ADR-384's HAL mapping (mmWave, UWB,
  IMU, ToF, CSI, reading) instead of inventing names.
- **Labels follow ADR-384's rules.** Only `MEASURED` records may label; `human_confirm` is the
  operator label; vitals are about an anonymous track, never an object, and stay behind the
  vitals gate; privacy mode stops position and reading recording, so training sessions run
  in a consented test room with privacy mode off.
- **The first dataset is ADR-384 step 4,** lengthened. Step 4 already specifies the measured
  room, empty / walking / seated phases and `human_confirm` labels.
- **Clock alignment is a WeftOS deliverable.** ADR-384 keeps producer and receive times and
  applies published clock corrections, but the correction record does not exist yet. The
  sensor-contracts plan (§9 Q9) asks WeftOS to add three types to ADR-107: acoustic range,
  cooperative radio delay, and **clock correction**. Multi-node training needs the last one.

## 2. What H-JEPA adds

1. **It is the same model family.** H-JEPA's baseline is LeWM, the design our crates
   implement (192-d latent, action-conditioned predictor, CEM). A hierarchy plugs in above
   `pred_φ` without changing ADR-090: upper levels are still Observation / Impulse writers.
2. **The hierarchy suits our signals.** Upper levels filter fast, unpredictable variation and
   keep slow state. CSI and radar are noisy at 10-100 Hz, while what we care about (occupancy,
   who is where, breathing state) changes over seconds to minutes. This mirrors ECC's
   multi-timescale ticks.
3. **A working reference trainer.** MIT code with 84 trained models replaces our residual
   training stubs as a design reference, and its released models let us test our planner,
   SIGReg monitor and rollback gate on a known-good model before ours exists.
4. **What it does not give us:** data, labels or actions from our world. Its gains need
   action-labelled sequences long enough for three levels; ours do not exist yet.

## 3. Key point: labels are needed for evaluation, not for training

JEPA-style models train **self-supervised**: predict the next latent from the current one
(and an action). So the bottleneck is **recorded streams and actions**, not labels. Labels
are needed for the rollback gate's held-out probe, for checking the model learned something
real, and for downstream heads (pose, vitals). We can get most of them automatically:

| Label | Source | Notes |
|---|---|---|
| Person position / pose | Camera as a training-time teacher (RuView ADR-079), discarded at deployment | Needs a consent and handling rule (§7) |
| Room geometry | Tape-measured shell (ADR-107, already `MEASURED`) | Already captured for the test room |
| Heart rate / breathing | ECG cog (SEN0213) worn during LD6002 sessions | Cross-modal: radar learns, ECG grades |
| Presence / count | Agreement across radar, CSI, ToF, PIR, plus operator `human_confirm` events | Disagreement is flagged, not guessed |
| Protocol step | The script the operator follows ("walk A to B", "sit", "leave") | Doubles as the **action** channel |

**Actions** in a sensing mesh are scarce, so we create them: scripted protocol steps,
actuator commands (lights, the iDotMatrix panel, cog config changes), and node placement
moves. For the agent side, every tool call in ExoChain and ADR-110 transcripts is an action
with an observed outcome, the one place we already have actions at scale.

## 4. Phases

| Phase | Delivers | Done when |
|---|---|---|
| **W0 Decide** | Owner lifts the hold for the sensor world model; picks the training stack and the camera rule (§8). | Decisions recorded here and on the board. |
| **W1 Data path** | Depends on RuView ADR-384 steps 1, 2 and 2b (inbound parsing, server ingest with recording, SenML readings). WeftOS side: publish the evidence contract as WeftOS ADR-111 with the clock-correction record and the two ranging types the RuView plan asks for; rebuild the cog0 `ld2450-radar` with the `spatial-evidence` feature and serve `GET /spatial` where RuView can pull it (owner: use cog0, no Mac build); publish the shared SenML quantity vocabulary in WeftOS (Q8, ADR-111); extend `SensorClass` to the ADR-384 HAL mapping; build the dataset archiver that copies a finished RuView session (`.evidence.jsonl`, CSI recording, readings) with a manifest (room, `shell_measure`, node `pose`s, protocol, consent, proof counts) to photo-gallery under the `ruview-demo` project. | A recorded session replays through RuView's file transport with identical counts, and the archived copy on PG checksums identical. |
| **W2 First real dataset** | ADR-384 step 4, extended for training: the measured test room, empty / walking / seated / two-person phases with `human_confirm` per phase, at least 3 hours over several days, CSI + LD2450 + LD6002 + ToF + readings, plus the camera teacher and ECG where §8 allows. Closes cards 61020daa and f491f12e. The room is the 12 × 12 ft room with its own Wi-Fi router (§8). | Dataset card: hours per phase, held-out split by day, proof-tag counts, checksums; ADR-384 step 4 validation record filed. |
| **W3 Labels** | RuView's labeller over the recordings (`auto_label`, `verified` only for `MEASURED`), `human_confirm` phases, the tape shell, camera-teacher positions (RuView ADR-079), ECG for vitals bound to an anonymous track; all as label tracks separate from observations, never from `CODE`/`SYNTHETIC` records. | Held-out probe set; label agreement report alongside RuView's calibration-status diagnostics. |
| **W4 Evaluation harness** | Matched baseline (copy-last and linear `pred_φ`), held-out probe, VoE diff, the activity-floor collapse alarm and geometry audit from jepa-anything.md, wired into the existing rollback gate and SIGReg monitor. | The gate can say no to a model on real data. **Status 2026-10-26:** harness built and tested on labelled SYNTHETIC fixtures only (`weftos-worldmodel-impls/src/eval_harness.rs`): contiguous held-out split, copy-last and per-dimension linear baselines, activity-floor collapse alarm, orthonormality audit, and the four-condition gate fed with SIGReg health from the Welford monitor; promotes only when all agree. Missing for "on real data": the W2 recording, a held-out-by-day split (the harness splits contiguously), real action channels, and the pseudoinverse round-trip leg of the geometry audit (needs a learned basis to audit). Finding: the gate's probe is a sign-agreement score against the mean residual, not a trained probe, and the SIGReg monitor's auto-rollback resets its statistics under sustained low health, so offline use needs a constant clock. |
| **W5 Flat LeWM** | Train encoder + `pred_φ` on W2 (Mac first), export weights to the RVF model segment, load through the candle path, swap at a tick. | Beats the matched baseline on held-out days, passes the gate, runs at 10 Hz on the Mac. |
| **W6 H-JEPA two-level** | 2-level vs flat on the same data, action-free first, then with protocol steps as actions. | Long-horizon prediction or planning improves over W5, or we record that it does not. |
| **W7 Agent-domain world model** (optional) | ExoChain + ADR-110 transcripts as action-labelled trajectories (tool call → outcome); same harness. | Owner decides after W5. |

## 5. Training stack (recommendation)

Train offline in **PyTorch on the Mac (MPS)**, because H-JEPA, JEPA-Anything and RuView's
`wifi-densepose-train` sources are PyTorch, and our model is small (ViT-tiny class, 192-d).
Export **safetensors** and run inference through the existing **candle** path in Rust; no
Python at runtime. Use cloud GPUs (RuView `gcloud-train.sh`) only when a run outgrows the Mac.
Keep RuView ADR-071's ruvllm route open for CSI-only heads that need SONA/LoRA adaptation
on device. photo-gallery stores sessions and does CPU preprocessing.

## 6. What stays out

- Skill-3D training (Qwen3-VL SFT/GRPO, card 65f8a922) stays on hold.
- No world-model output may override ECC reasoning (ADR-090 R1-R5); every model write goes
  through `lewm_invariant`.
- No client data: recordings come from our own test room and our own hardware.

## 7. Privacy and handling

Camera frames and ECG are sensitive. Proposed rule: camera is training-time only, frames stay
on photo-gallery under the project's 0700 home, only derived labels (keypoints, positions)
leave it, raw frames have a retention date, and only people who consent are recorded. ECG
data is labelled with the subject's consent and never published.

## 8. Owner decisions (2026-10-07)

| # | Decision |
|---|---|
| 0 | Start the WeftOS side now; the sensors are being hooked up soon. RuView ADR-384 steps proceed alongside. |
| 1 | **Training hold lifted for the sensor world model** ("we have disk space, let's get the training we need"). Skill-3D training stays on hold. |
| 2 | Training stack as recommended in §5 (PyTorch on the Mac → safetensors → candle). |
| 3 | **Camera ground truth is allowed, except in one client engagement's work.** The §7 handling rule applies. |
| 4 | **Test room: the 12 × 12 ft room (3.66 × 3.66 m) with its own Wi-Fi router.** The 6.40 × 3.66 m room in the spatial ADR is not the training room. |
| 5 | Producers: the LD2450 cog already running on cog0 (armv7), rebuilt with the `spatial-evidence` feature and pulled by RuView; no Mac build needed. |
| 6 | The shared contracts (`spatial.evidence.v1` with the three new record types, and the SenML quantity vocabulary) are **published in WeftOS** as ADR-111 and `contracts/sensors/`. |
| 7 | W7 (agent-session world model): undecided; revisit after W5. |
