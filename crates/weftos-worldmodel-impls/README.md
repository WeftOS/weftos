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
| `FourConditionRollbackGate` | **Done** | AND of SIGReg / held-out probe / VoE diff / temporal straighten (WEFT-530) |
| `OfflineEdgePipeline` + `StreamingMergePipeline` | **Done** | Both train `ActionLinearModel`; AND-gated promote (WEFT-531) |
| `ImportanceReplayBuffer` | **Done** | Per-class importance × surprise weighted draws |
| `SensorModelRegistry` + RVF segment I/O | **Done** | transmit-gate / aggregate / encode slots, tick hot-swap, gate rollback (WEFT-532); codec `0x02` raw, `0x03` safetensors |
| `ActionEncoder` + `NullActionEncoder` / `HashActionEncoder` | **Done** | Maps control bytes → fixed-width `Action` code |
| `StubLattice` composition | **Done** | Wires encoder + `pred_φ` + CEM for service scaffolding |
| `candle`: `CandleVitEncoder` (ViT-tiny) | **Done, untrained** | Patch embed, learned position embedding, 4 pre-norm MHSA/MLP blocks, mean-pool, head to 192; random init or safetensors load |
| `candle`: `AdaLnPredictor` | **Done, synthetic-trained** | adaLN-zero residual MLP conditioned on the 32-d action; AdamW training (`train_adaln`), safetensors export / load |
| `candle`: safetensors + registry delivery | **Done** | `from_safetensors_path` / `from_safetensors_bytes`, `to_model_checkpoint`, `active_candle_model` |
| ViT encoder training (JEPA: EMA target, SIGReg isotropy penalty) | **Residual** | Encoder graph has a differentiable mode; the loop is not written |
| Weights trained on real data | **Blocked** | Needs the W2 recording |

Default builds **never** depend on `candle-core` / `candle-nn` (same idiom as
`clawft-voice-onnx` and its empty default features). CI and
`scripts/build.sh test weftos-worldmodel-impls` exercise only the default path.

## Feature flags

```toml
weftos-worldmodel-impls = { workspace = true }                 # default, no_std + alloc
weftos-worldmodel-impls = { workspace = true, features = ["std"] }
weftos-worldmodel-impls = { workspace = true, features = ["candle"] }  # ViT-tiny + AdaLN on CPU
```

- **default** — stubs plus the trained linear `pred_φ`, `no_std` + `alloc`
- **std** — host marker forwarded to core
- **candle** — `candle-core` / `candle-nn` **0.11** (lock: 0.11.1). Builds and
  passes `cargo test -p weftos-worldmodel-impls --features candle` on toolchain
  1.95 (verified 2026-11-14). CPU only: candle's `metal` feature needs
  `candle-metal-kernels` 0.11, which is not in the offline registry cache.
  Default CI stays off this feature. (candle-core 0.2.x remains blocked, WEFT-216.)

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
- **CPU timing** (`cargo bench -p weftos-worldmodel-impls --features candle --bench encode_predict`,
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

1. Train the ViT encoder: JEPA loop (encoder on frame bytes, EMA target
   encoder, SIGReg-style isotropy penalty, loss through the AdaLN predictor).
2. Train on real data (W2 recording, held-out days), then the AdaLN bake-off
   against `ActionLinearModel` on the same split.
3. Metal / MPS training (needs `candle-metal-kernels`, a new-dep gate item) or
   PyTorch-on-Mac export into the safetensors layout above.

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
