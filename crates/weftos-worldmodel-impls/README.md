# weftos-worldmodel-impls

Concrete implementations of [`weftos-worldmodel-core`](../weftos-worldmodel-core)
traits for the LeWM latent world model (WEFT-521).

## What ships today (honest AC)

| Component | Status | Notes |
|-----------|--------|--------|
| `Null*` / stub `Encoder`, `Predictor`, `LatentPlanner`, `LatticeApi`, `SigRegMonitor` | **Done** | Default feature set; unit-tested; no ML weights |
| `LinearPredPhi` (`pred_φ`) | **Done** | Untrained: weights-free action broadcast. `from_checkpoint_weights`: serves the learned `ẑ = z + A·a + b` |
| `ActionLinearModel` | **Done** | 192 × 32 action matrix + bias; ridge least squares (offline) and normalised-LMS SGD (streaming), pure Rust `no_std`; `LWAL` weight blob |
| `CemPlanner` / `MppiWarmPlanner` / `GradientPlanner` | **Done** | CEM default @ 10 Hz path; toy planning tests (WEFT-529) |
| `WelfordSigRegMonitor` | **Done** | Online Welford + auto-rollback at 0.85 / 30 s (WEFT-528) |
| `FourConditionRollbackGate` | **Done** | AND of SIGReg / held-out probe / VoE diff / temporal straighten (WEFT-530). Probe = fraction of held-out pairs where the model beats copy-last (`GateProbeKind::BeatsCopyLast`, default since 2026-11-14; legacy sign agreement via `with_probe_kind`) |
| `OfflineEdgePipeline` + `StreamingMergePipeline` | **Done** | Both train `ActionLinearModel`; AND-gated promote (WEFT-531) |
| `ImportanceReplayBuffer` | **Done** | Per-class importance × surprise weighted draws |
| `SensorModelRegistry` + RVF segment I/O | **Done** | transmit-gate / aggregate / encode slots, tick hot-swap, gate rollback (WEFT-532); codec `0x02` raw, `0x03` safetensors |
| `ActionEncoder` + `NullActionEncoder` / `HashActionEncoder` | **Done** | Maps control bytes → fixed-width `Action` code |
| `StubLattice` composition | **Done** | Wires encoder + `pred_φ` + CEM for service scaffolding |
| `candle`: `CandleVitEncoder` (ViT-tiny) | **Done, untrained** | Patch embed, learned position embedding, 4 pre-norm MHSA/MLP blocks, mean-pool, head to 192; random init or safetensors load |
| `candle`: `AdaLnPredictor` | **Done, synthetic-trained** | adaLN-zero residual MLP conditioned on the 32-d action; AdamW training (`train_adaln`), safetensors export / load |
| `candle`: safetensors + registry delivery | **Done** | `from_safetensors_path` / `from_safetensors_bytes`, `to_model_checkpoint`, `active_candle_model` |
| ViT encoder training (JEPA: EMA target, SIGReg isotropy penalty) | **Implemented, SYNTHETIC frames only** | `candle::jepa::train_jepa`: online ViT + EMA target, AdaLN `pred_φ`, std-form diagonal isotropy penalty (optional off-diagonal term), AdamW; held-out-by-day collapse checks via `evaluate_by_day` + `WelfordSigRegMonitor` + participation ratio; fixtures in `candle::synthetic` |
| Weights trained on real data | **Blocked** | Needs the W2 recording |

Default builds **never** depend on `candle-core` / `candle-nn` (same idiom as
`clawft-voice-onnx` and its empty default features). CI and
`scripts/build.sh test weftos-worldmodel-impls` exercise only the default path.

## Feature flags

```toml
weftos-worldmodel-impls = { workspace = true }                 # default, no_std + alloc
weftos-worldmodel-impls = { workspace = true, features = ["std"] }
weftos-worldmodel-impls = { workspace = true, features = ["candle"] }  # ViT-tiny + AdaLN on CPU
weftos-worldmodel-impls = { workspace = true, features = ["metal"] }   # candle + Apple GPU (macOS)
```

- **default** — stubs plus the trained linear `pred_φ`, `no_std` + `alloc`
- **std** — host marker forwarded to core
- **candle** — `candle-core` / `candle-nn` **0.11** (lock: 0.11.1). Builds and
  passes `cargo test -p weftos-worldmodel-impls --features candle` on toolchain
  1.95 (verified 2026-11-14). CPU by default. Default CI stays off this feature.
  (candle-core 0.2.x remains blocked, WEFT-216.)
- **metal** — implies `candle` and turns on `candle-core/metal` and `candle-nn/metal`
  (adds `candle-metal-kernels` and `objc2-metal` to the lock, no new direct dependency).
  macOS only, opt-in, never in CI. See "Metal" below.

## Metal (feature `metal`)

Everything that runs on CPU also runs on the Apple GPU, in F32, forward and backward.
Nothing falls back to CPU and no precision is reduced. Measured on candle 0.11.1:

| Path | On Metal | Notes |
|---|---|---|
| ViT-tiny encode (fused `layer_norm`, `softmax_last_dim`) | yes | matches CPU within 2e-3 |
| AdaLN predict | yes | matches CPU within 2e-3 |
| AdaLN training (`train_adaln_with`, AdamW) | yes | `AdaLnTrainConfig::device` |
| JEPA training step and `train_jepa` (composed LayerNorm and softmax, `index_select`, EMA update) | yes | `JepaTrainConfig::device`; first 3 steps within 5 % of CPU |
| safetensors load / export | yes | `*_on(&Device)` loaders read straight to the device; export copies back to the host |
| Host-side work (fixture generation, SplitMix64 init, the eval harness, `to_vec` of results) | CPU | by design |

Select a device with `ComputeDevice` (`Cpu` default, `Metal`, `Auto`), `candle_default_device()`
(Metal with the reason logged when it cannot open, else CPU), or pass a `&Device` to
`CandleVitEncoder::new_random_on`, `AdaLnPredictor::new_random_on` and the
`from_safetensors_*_on` loaders. `ComputeDevice::Metal` is an error, never a silent CPU run,
when the feature or a GPU is missing. `metal_supported()` is the compile-time flag.
There is no op that needs a CPU route today, so no path is gated on it.

**Speed** (`cargo bench -p weftos-worldmodel-impls --features metal --bench encode_predict`,
release, Apple M5 Max, 2026-11-14, random init, machine under load from other jobs):

| Call | CPU µs | Metal µs | Metal speed-up |
|---|---|---|---|
| ViT-tiny encode, 1 frame | 6 666 | 2 847 | 2.3x |
| AdaLN predict, 1 latent | 217 | 654 | 0.33x (slower) |
| encode + predict | 6 841 | 4 529 | 1.5x |
| JEPA train step, batch 16 | 141 435 | 71 948 | 2.0x |
| JEPA train step, batch 32 | 287 207 | 88 426 | 3.2x |

Single-frame inference is launch-bound: the small AdaLN is faster on CPU. Training wins
because the work per launch grows with the batch. The default JEPA regression (288 steps,
`std` fixture, seed 21, release) takes 17.9 s on Metal against 75 s on CPU.

**Accuracy caveat.** Metal is not bit-identical to CPU (GPU summation order, fast-math
shaders), so a training run takes a different trajectory. On the same regression, held-out
participation ratio is 10.07 / 9.93 / 9.98 on seeds 21 / 22 / 23 (CPU 10.63 / 9.97), with 0
collapsed coordinates. The AdaLN error against the θ pairing beats both baselines by the
required margin on seed 22 only (CPU: seeds 21 and 22); on seeds 21 and 23 it lands 5 %
under linear. The ξ pairing beats both baselines on every Metal seed. The ignored Metal
regression therefore asserts the ξ pairing, while the CPU one asserts θ.

## Candle models (feature `candle`)

- **Observation bytes for the ViT:** `3 · H · W` bytes, HWC interleaved RGB u8
  (default 64 × 64 = 12 288 bytes), scaled to `[-1, 1]`. Any other length is
  `WorldModelError::InvalidObservation`; `encode_checked` names the expected size.
- **Checkpoints:** one safetensors file of F32 tensors with a `lewm.config`
  tensor `[arch, format, params…]` (arch 1 = ViT-tiny, 2 = AdaLN), so loaders
  rebuild the architecture from the file. Names follow timm's ViT
  (`patch_embed.proj.weight`, `blocks.{i}.attn.qkv.weight`, …); a PyTorch
  `Conv2d` patch kernel `(E, 3, p, p)` reshaped to `(E, 3·p·p)` loads unchanged.
  Loading is strict: missing, extra or mis-shaped tensors are errors.
- **Delivery:** `to_model_checkpoint` wraps a blob as an Encode-slot
  `ModelCheckpoint`; on the wire it carries codec `0x03`. After a tick commit,
  `active_candle_model(&registry, class)` rebuilds the encoder or predictor.
- **CPU timing, earlier run** (`cargo bench -p weftos-worldmodel-impls --features candle --bench encode_predict`,
  Apple M5 Max, 2026-11-14, random init): ViT-tiny encode ≈ 5.1 ms per 64 px
  frame (≈ 197 frames/s), AdaLN predict (hidden 256, depth 2) ≈ 0.18 ms,
  encode + predict ≈ 6.5 ms (≈ 155 Hz, well over the 10 Hz plan target).

## Training surfaces (WEFT-531 / WEFT-532)

```rust
use weftos_worldmodel_impls::{
    OfflineEdgePipeline, StreamingMergePipeline, SensorModelRegistry,
    OfflineEdgeTrainer, StreamingMergeTrainer, ModelHotSwap,
    TrainingSample, SensorClass, SmallModelKind, encode_model_segment,
    model_segment_to_wire,
};
use weftos_worldmodel_core::zero_latent;

// Surface 1 — offline edge: train → RVF segment → hot-swap at tick.
let mut edge = OfflineEdgePipeline::new();
// ... push TrainingSample ...
let cp = edge.train_cycle(SensorClass::Imu, SmallModelKind::Encode, /*tick=*/10).unwrap();
let wire = model_segment_to_wire(&encode_model_segment(&cp));

// Surface 2 — online streaming-merge with AND gate.
let mut stream = StreamingMergePipeline::new();
// ... push samples ...
let step = stream.train_step(SensorClass::RgbD, SmallModelKind::Encode).unwrap();
assert!(step.promoted || step.gate.blocks_promotion());

// WEFT-532 — three slots per sensor class, tick commit, gate rollback.
let mut reg = SensorModelRegistry::new();
reg.stage(cp).unwrap();
reg.commit_at_tick(10).unwrap();
```

## Residual training work (honest)

1. ~~Train the ViT encoder: JEPA loop~~ implemented on SYNTHETIC moving-blob
   frames (`candle::jepa`, fixtures in `candle::synthetic`).
   ~~Dimensional collapse on the default ViT-tiny~~ fixed by the lane J
   bake-off. Lane G's settings go rank one in the first epoch: the latent
   becomes a session-constant scalar, because mean pooling with a 0.02-std
   position embedding hides blob position while distractor presence and the
   day background move the pooled vector. The variance penalty alone, with
   no prediction term, collapses too. The new `JepaTrainConfig::default()`
   adds batch 32, `τ = 0.99`, a VICReg variance hinge and decorrelation on
   the batch correlation matrix. On 288 steps of the default fixture it
   reaches held-out participation ratio 10.6 (lane G: 1.05), 0 collapsed
   coordinates, held-out health 0.90, and an AdaLN error of 178 against
   copy-last 350 and linear 204. Two more training seeds give ratio
   10.0 / 9.8 and health 0.87 / 0.84. `JepaTrainConfig::lane_g()` keeps the
   old behaviour, and a release-only ignored test guards the thresholds.
   Still missing: the gate vetoes held-out probe accuracy and temporal
   straightening, health sits near the 0.85 bar, and real data. Metal runs it
   4x faster (see above) with the baseline-margin caveat.
2. Train on real data (W2 recording, held-out days), then the AdaLN bake-off
   against `ActionLinearModel` on the same split.
3. PyTorch-on-Mac export into the safetensors layout above (Metal training is done).

## Usage (runtime defaults)

```rust
use weftos_worldmodel_core::{Encoder, Predictor, LatentPlanner, Action};
use weftos_worldmodel_impls::{
    NullEncoder, LinearPredPhi, CemPlanner, WelfordSigRegMonitor, NullActionEncoder, ActionEncoder,
};

let enc = NullEncoder;
let z = enc.encode(b"frame").unwrap();
let a = NullActionEncoder.encode_bytes(b"noop").unwrap();
let z_hat = LinearPredPhi::default().predict(&z, &a).unwrap();
let plan = CemPlanner::default().plan(&z, 4).unwrap();
assert_eq!(plan.steps.len(), 4);
let mut mon = WelfordSigRegMonitor::new(1);
let _ = mon.update(&z).unwrap();
let _ = z_hat;
```

## Related tickets

- WEFT-520 — `weftos-worldmodel-core` traits (landed)
- **WEFT-521** — this crate
- WEFT-522 — facade re-export (`weftos-worldmodel`) — landed; prefer that for consumers
- WEFT-528 — Welford SIGReg + auto-rollback (landed)
- WEFT-529 — pred_φ + CEM planner (landed; neural weights residual)
- WEFT-530 — four-condition AND rollback / promotion gate (landed)
- WEFT-531 — two training surfaces: offline edge + online streaming-merge (landed; neural residual)
- WEFT-532 — per-sensor-class RVF small models, hot-swap, AND-gate rollback (landed)
- WEFT-543 — latent dim = 192 contract
