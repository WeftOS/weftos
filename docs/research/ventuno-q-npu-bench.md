# Ventuno Q NPU benchmark (Gemma 4 E2B)

**Date**: 2026-10-23
**Status**: Measured. One model, one prompt. Results are indicative, not a full sweep.

## Summary

On the Arduino Ventuno Q, the Hexagon NPU generates about **20 tokens/s**, with **0.1 s to the first token**. That is roughly 2–3× the board's CPU and 4.5× its GPU. The same model file runs about 9× faster on an RTX 4070 (aepod-xpc) and on an M5 Max MacBook.

The board is not a replacement for the mesh's GPU or Mac hosts for large models. It is a usable local-inference node for small models (2–4B) at the edge, such as a voice loop, a sensor-side classifier, or a VLM on its own cameras.

## Setup

| | |
|---|---|
| Model | Gemma 4 E2B, Q4_0, `gemma-4-E2B_q4_0-it.gguf` (3.10 GiB, 4.63 B params), from `google/gemma-4-E2B-it-qat-q4_0-gguf` |
| Same file everywhere | sha256 `fa401b55b07ee70a…`, checked on all three machines |
| Board | Arduino Ventuno Q (Qualcomm QCS8275: Kryo CPU, Adreno 623 GPU, Hexagon NPU), Ubuntu 24.04.4, kernel 6.8.0-1078-qcom, on its built-in eMMC |
| Board runtime | Qualcomm GenieX v0.9.3 (QAIRT 2.50), `geniex infer --compute <npu\|cpu\|gpu\|hybrid> --max-tokens 256 --think=false` with one ~40-word prompt; 2 runs per mode. GPU mode needs `qcom-adreno-cl1`. |
| GPU host | aepod-xpc, RTX 4070 12 GB (WSL), CUDA 13.2 llama.cpp build, `llama-bench -ngl 99 -p 64 -n 256 -r 3` |
| Mac | MacBook Pro M5 Max, Homebrew llama.cpp (Metal + BLAS), same `llama-bench` flags. An idle mlx_lm server was loaded at the time. |

## Results

Generation speed (tokens/s), higher is better:

| Machine / processor | Generation (tok/s) | First token / prompt |
|---|---|---|
| Ventuno Q — **NPU** | **19.7, 20.1** | 0.1 s |
| Ventuno Q — CPU | 9.2, 7.0 | 0.7 s |
| Ventuno Q — hybrid (CPU+NPU) | 7.7, 7.8 | 0.4–0.5 s |
| Ventuno Q — GPU (Adreno, OpenCL) | 4.4, 4.4 | 1.3–1.4 s |
| RTX 4070 (aepod-xpc), CUDA | 189.2 ± 0.5 | prompt 3,949 ± 1,717 tok/s (pp64) |
| M5 Max MacBook, Metal | 174.1 ± 1.8 | prompt 2,172 ± 6 tok/s (pp64) |

The board's thermal zones read 35–37 °C after the runs, with the active cooler fitted.

## Notes

- **Arduino's figure is lower than ours.** Their GenieX guide reports 12.7 tok/s on the NPU and 4.7 on the CPU for this model (GenieX v0.4.0, a 128-token prompt). We measured 20 on the NPU with GenieX v0.9.3, so the newer runtime is likely faster. Their hybrid number (6.5) and ours (7.8) agree that hybrid is *slower* than NPU-only.
- **The two tools measure differently.** GenieX prints one tokens/s figure per run, which includes sampling. `llama-bench` measures pure generation (tg256) and prompt processing (pp64) separately. Treat the board-versus-host ratio as approximate (about 9×), not exact.
- **Not measured:**
  - power draw;
  - sustained throughput over long runs (throttling);
  - larger models such as Qwen3-4B, Qwen3-VL-4B or Qwen2.5-VL-7B, which App Lab lists as Genie bundles;
  - concurrent requests;
  - prompt-processing speed on the board.
- **Storage.** The model cache lives in `~/.cache/geniex` on the board's 59 GB eMMC. Move it to the M.2 drive when that arrives.

## What it means for placement

For the governed workload placement layer (ADR-099), treat the Ventuno as an `npu` class host:

- Good for: small models up to about 4B at interactive speed (about 20 tok/s, 0.1 s to first token).
- Not a fit: anything where throughput matters, which belongs on the 4070 or the Mac.
- Serving: GenieX has an OpenAI-compatible server (`geniex serve`, :18181, **no authentication**). Expose it only behind the WeftOS router or over loopback, never bound to all interfaces.

## Sources

- Arduino GenieX guide: `arduino/docs-content`, `content/hardware/ventuno/boards/ventuno-q/tutorials/ai-workflows/geniex/geniex.md`
- Arduino NPU guide: same repo, `…/ai-workflows/npu-guide/npu-guide.md`
