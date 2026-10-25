# Ventuno Q NPU benchmark

**Date**: 2026-10-23
**Status**: Measured. The first section is a single-model speed comparison (2026-10-23). The model suite below (2026-10-25) covers LLMs, vision and speech. Packs are small, so treat scores as indicative.

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
  - concurrent requests;
  - prompt-processing speed on the board.
- **Storage.** For this first run the model cache was on the 59 GB eMMC. It now lives on the M.2 NVMe (`/data/models/geniex`), where Gemma 4 E2B generated at 18.7 tok/s.

## Model suite (2026-10-25)

Run with the board on its M.2 NVMe (`/data`), GenieX v0.9.3 on the NPU, and speech on the 8 CPU cores. The scripts are in `scripts/bench/ventuno/` (see *Re-running* below). Board thermal zones stayed between 39 and 46 °C.

### LLMs (NPU, `~/llm` eval packs over `geniex serve`)

| Model | instruct | math | code | tool_use | Generation |
|---|---|---|---|---|---|
| Qwen3.5-0.8B Q4_0 | 4/5 | 5/6 | 4/6 | 8/12 | 33.0 tok/s |
| Gemma 4 E2B Q4_0 | 5/5 | 6/6 | 6/6 | 5/12 | 19.4 tok/s |
| Qwen3-4B-Instruct-2507, Qualcomm AI Hub W4A16 | 5/5 | 6/6 | 5/6 | 11/12 | 15.0 tok/s |
| Gemma 3 1B it Q4_0 (unsloth) | 4/5 | 6/6 | 5/6 | 4/12 | 32.5 tok/s |

- **Gemma 3 1B** (added 2026-10-25, Hugging Face token now on the board) is as fast as Qwen3.5-0.8B with better math and code, and the weakest at tool calls.
- **Gemma 4 E4B does not fit.** It is OOM-killed even at 8k context on the 14 GiB board.
- **The NPU needs pure Q4_0 GGUF.** The unsloth Qwen3-4B "Q4_0" file mixes in q4_1/q6_K tensors and fails to load. Use the Qualcomm AI Hub W4A16 bundle instead.
- **Context.** The largest `--nctx` that loads is 16384; 20480 fails. In a needle test, Qwen3.5-0.8B and Gemma 4 E2B found the needle at about 3k and 12.5k real tokens, and the server rejected about 21k with `context_length_exceeded`. Prefill is about 790 tok/s (0.8B) and about 510 tok/s (E2B). The AI Hub Qwen3-4B bundle has a short fixed context: it passed at 2.8k tokens and rejected 12k.
- **Memory.** Run `geniex serve` with `--keepalive 30`. Without it, earlier models stay resident and the next load is OOM-killed.

### Vision (NPU, 8-item objective pack, `geniex infer`)

The pack covers OCR, counting, colour, shape, arithmetic, left/right, a zero-count negative control and reading a room sign. The images are synthetic, 384×256, in `scripts/bench/ventuno/vlm-pack/`.

| Model | Score | Generation | First token | Wall per item (incl. load) |
|---|---|---|---|---|
| Qwen3-VL-4B, AI Hub W4A16 | **8/8** | 14–26 tok/s | 0.5 s | 12 s |
| Qwen2.5-VL-7B, AI Hub W4A16 | **8/8** | 10–19 tok/s | 0.6 s | 15 s |
| Gemma 4 E2B Q4_0 + mmproj | 7/8 (counted 5 circles, not 4) | 18–20 tok/s | **46 s** | 56 s |

Gemma's 46 s to first token is the image encoder; text generation is as fast as text-only. The AI Hub bundles encode on the NPU. Every item reloads the model, so a resident server would cut the wall time to about the first-token time plus generation.

### Speech to text (CPU, 8 threads)

GenieX rejects audio models (`unsupported domain MODEL_DOMAIN_AUDIO`), so speech runs on the CPU: Parakeet TDT 0.6B v2 int8 via sherpa-onnx 2.0.0, and Whisper via whisper.cpp. The datasets are the `~/llm` voicelab sets: LibriSpeech clean and other (40 clips each) and FLEURS (30 clips: 6 each of en, es, fr, de, hi). WER is the per-clip mean from `voicelab.metrics.wer`, the same metric as the lab's Mac baselines. RTFx is seconds of audio per second of compute.

| Engine | LS-clean WER | LS-other WER | FLEURS-en WER | RTFx |
|---|---|---|---|---|
| Parakeet TDT 0.6B v2 int8 | 2.7% | **1.8%** | 4.4% | **7.0–8.1×** |
| Whisper base.en | 4.1% | 7.3% | 7.1% | 1.1–1.6× |
| Whisper small.en | 3.6% | 6.9% | 4.6% | 0.3–0.5× (slower than real time) |
| Whisper large-v3-turbo q5_0 | 2.6% | 3.0% | 4.6% | 0.1–0.2× |

- **Multilingual.** Whisper large-v3-turbo with `-l auto` scores 6.9% across all five FLEURS languages (en 4.6, es 2.4, fr 10.5, de 1.8, hi 15.2) at 0.1× real time. The English-only engines score 63–86% on the full FLEURS set because 24 of the 30 clips are not English, so only their English subset is in the table.
- **Against the Mac.** Accuracy is the same as on the Mac. Speed is about 10–15× lower: Parakeet runs at about 80–123× real time on the M5 Max and 7–8× here.
- **Pick for the board: Parakeet.** It is the only engine that is both accurate and comfortably faster than real time. Whisper beyond base.en cannot keep up with live audio on this CPU.

### Text to speech (CPU, 8 threads)

There are ten test sentences with times, numbers, names and units. Each wav is transcribed back with Parakeet as an intelligibility check.

| Engine | RTF (lower is faster) | Median per sentence | Round-trip WER | Real word errors |
|---|---|---|---|---|
| Piper en_US amy medium (sherpa-onnx 1.13.8) | **0.12 (8.4× real time)** | 0.5 s | 15.2% | 3 (Akonquo, NTU, Piber) |
| Kokoro en v0.19 (sherpa-onnx 1.13.8) | 1.15 (slower than real time) | 4.3 s | 14.8% | 1 (Akonkwo) |
| Phonon 31m-en q8 (`ptts`) | 0.61 (1.6× real time) | 2.6 s | 17.3% | 2 (Weftdoes, Cocoro) |

- **Most round-trip WER is number formatting, not speech errors.** For example, "4471" comes back as "four thousand four hundred seventy one", and "3:45 PM" as "3.45 p.m.". The *Real word errors* column counts only misheard words.
- **sherpa-onnx 2.0.0 cannot run Piper or Kokoro.** It has no built-in espeak frontend ("no lexicon, and no frontend available"), so TTS uses a separate venv with 1.13.8.
- **Pick for the board: Piper for speed, Phonon for a better voice.** Kokoro is too slow on this CPU for interactive use.

### Camera, vision and audio on live input (USB camera, 2026-10-25)

Innomaker U20CAM (UVC, 1080p) with its built-in microphone; test speech played from the
Mac's speaker. Details in [the brick spike](ventuno-q-app-lab-brick-spike.md).

| Test | Result |
|---|---|
| Object detection, YOLOX-nano QNN (App Lab's EI runner, run directly) | person in 60/60 live frames; NPU 7 ms per frame; 7.6 frames/s with one `curl` per frame |
| Wake word "Hey Arduino" (EI, NPU) | 3/3 caught but 3 false triggers on other speech; usable only as a first stage confirmed by ASR |
| Parakeet on the camera microphone | word for word on eight phrases; 8.3× real time |
| H.264 encode, 720p30, 300 frames | hardware (`v4l2h264enc` via GStreamer): real time at 10% of 8 cores; software libx264 veryfast: real time at 35%; `ffmpeg h264_v4l2m2m` crashed, so use the GStreamer path |

### Re-running

On the board, `/data/bench` holds a venv with `sherpa-onnx` 2.0.0 (ASR) and `venv-tts` with 1.13.8 (TTS). It also holds the models under `models/`, a `whisper.cpp` build with ggml models, `ptts` under `ptts/bin`, and the voicelab audio under `asr/`.

- **LLM packs** run from the Mac with an SSH tunnel to `:18181`: `scripts/bench/ventuno/serve.sh 16384`, then `MODELS_LIST="..." scripts/bench/ventuno/suite.sh <log>`.
- **Speech** runs on the board:
  - `asr.py <impl> <dataset> <out.json>` (set `WHISPER_LANG=auto` for multilingual).
  - `tts.py <engine> <outdir>`, then `tts-roundtrip.py <outdir>/<engine>.json` followed by `asr.py parakeet tts-<engine> ...`.
- **Vision** runs from the Mac: `VENQ_HOST=user@board scripts/bench/ventuno/vlm.py <model> <out.json>`, with the pack copied to `/data/bench/vlm`.

## What it means for placement

For the governed workload placement layer (ADR-099), treat the Ventuno as an `npu` class host:

- Good for:
  - small models up to about 4B at interactive speed (15–33 tok/s, under 1 s to first token);
  - 4–7B vision models (Qwen3-VL-4B, Qwen2.5-VL-7B);
  - a CPU speech loop, with Parakeet in and Piper or Phonon out.
- Not a fit: anything where throughput matters, which belongs on the 4070 or the Mac.
- Serving: GenieX has an OpenAI-compatible server (`geniex serve`, :18181, **no authentication**). Expose it only behind the WeftOS router or over loopback, never bound to all interfaces.

## Sources

- Arduino GenieX guide: `arduino/docs-content`, `content/hardware/ventuno/boards/ventuno-q/tutorials/ai-workflows/geniex/geniex.md`
- Arduino NPU guide: same repo, `…/ai-workflows/npu-guide/npu-guide.md`
