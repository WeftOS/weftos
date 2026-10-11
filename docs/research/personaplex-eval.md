# NVIDIA PersonaPlex evaluation for WeftOS

- Date: 2026-10-11
- Read: `github.com/NVIDIA/personaplex` at commit `3428dfd9` (2026-03-02, no tags), the public
  Hugging Face model metadata for `nvidia/personaplex-7b-v1` (revision `fdaf4090`), the paper
  (arXiv 2602.06053, v1), the NVIDIA Open Model License page (last modified 2025-10-24), and
  open GitHub issues.
- Not done: no weights downloaded, nothing run, nothing installed. The model card files
  (`safety.md`, `bias.md`, `privacy.md`, `explainability.md`) are behind the HF gate and were
  not read. Latency and VRAM numbers for our hardware are therefore inferred, not measured.

## Verdict: watch (pattern-only for the ideas)

*Updated 2026-10-11: hosts re-sized against the CUDA host's RTX 4070 (12 GB), the mesh's CUDA host.*

Do not adopt. Three deciding reasons:

1. It cannot call tools or use our LLM. The speech model is its own 7B brain; the paper lists
   "integration with external tools" as future work. That breaks the chief-of-staff design
   (ADR-118), where the agent routes through WeftOS model routing and tool calls.
2. The stock build does not fit our CUDA host. Official support is CUDA only, with about
   17 GB of bf16 weights. Our CUDA host is a Windows + WSL desktop (RTX 4070, 12 GB VRAM,
   about 10 GB free at idle), so it needs an 8-bit build (about 8.5 GB of weights), which
   nobody ships yet; PR #103 adds opt-in quantization but targets GB10 frame time, untested in 12 GB. The Mac is not a target.
3. It is English-only and Python/PyTorch-only (torch pinned below 2.5), with weights under a
   revocable, gated NVIDIA licence. Nothing ports to our Rust/ONNX stack without re-doing the
   model.

What would change the verdict: an FP8 or 8-bit build that holds real time (80 ms/frame) in
12 GB on the CUDA host's RTX 4070, which makes a sidecar trial cheap; or a tool-call or
text-injection seam in the model (or a successor that has one).

## 1. What it is

Full-duplex speech-to-speech model, a fine-tune of Kyutai Moshi.

- README: "real-time, full-duplex speech-to-speech conversational model ... based on the Moshi
  architecture and weights." Paper: Moshi-style model with three streams (user audio, agent
  text, agent audio). Base model on HF: `kyutai/moshiko-pytorch-bf16`.
- Config (`moshi/moshi/models/loaders.py`): Mimi codec at 24 kHz, 12.5 Hz frame rate (80 ms
  frames), 8 codebooks generated per frame; temporal transformer dim 4096, 32 layers, 32
  heads; depformer 6 layers. HF reports 8.37 B bf16 parameters (about 17 GB of weights).
- Persona control is hybrid (paper section 3.1): a text role prompt forced onto the text
  channel, followed by a short voice sample on the audio channel (voice conditioning). The
  server wraps the text in `<system> ... <system>` tags. Voices ship as 18 presets (NATF0-3,
  NATM0-3, VARF0-4, VARM0-4) as `.pt` embeddings; a wav voice prompt is also accepted by the
  loader (`server.py`, `load_voice_prompt`). Zero-shot cloning is claimed in the paper.
- Language: English. HF tags `en`; the Fisher English corpus is part of the released
  checkpoint's training. No other language is documented.
- Training data: synthetic dialogs (Qwen-3-32B and GPT-OSS-120B scripts, Dia TTS for audio)
  plus 1,217 hours of Fisher English for the released checkpoint (paper appendix A). The
  released checkpoint "exhibits significantly increased backchannel frequency and improved
  pause handling."
- Latency claims (paper table 2, research checkpoint): smooth turn-taking latency 0.07 s,
  user-interruption latency 0.40 s, against Gemini 1.30 s and Freeze-Omni 0.95 s. Released
  checkpoint (table 6): 0.17 s and 0.24 s. These are model-side latencies on the benchmark,
  measured on NVIDIA hardware; they say nothing about a Mac. Role adherence on their
  Service-Duplex-Bench: 4.48 mean, behind Gemini Live at 4.73.
- Turn-taking, backchannel and interruption are learned, not scripted. There is no explicit
  controller, floor state or API for them; the model just hears the user stream continuously
  and decides to speak, stay quiet or "mm-hmm". Backchannel frequency is still low (0.042
  freq in table 6).

## 2. Licences

| Part | Licence | Evidence |
|---|---|---|
| This repo's code | MIT (NVIDIA, 2026) | `LICENSE-MIT`; README: "code is provided under the MIT license" |
| Moshi-derived code | MIT (Kyutai) | `moshi/LICENSE.moshi` |
| Audiocraft-derived code | MIT (Meta) | `moshi/LICENSE.audiocraft` |
| Web client | MIT-style, no copyright line | `client/LICENSE` |
| Model weights | NVIDIA Open Model License (HF `license: other`) | HF card metadata; README: "weights ... released under the NVIDIA Open Model license" |
| Underlying Moshi weights | CC-BY-4.0 | HF gated prompt: "ADDITIONAL INFORMATION: CC-BY-4.0" pointing at the Kyutai repo |

Weight licence terms that matter (page text read 2026-10-11):

- Commercial use is allowed: "Models are commercially usable. You are free to create and
  distribute Derivative Models." NVIDIA claims no ownership of outputs; the user owns their
  derivative models.
- Redistribution: allowed if each recipient gets a copy of the agreement and a Notice file
  reading "Licensed by NVIDIA Corporation under the NVIDIA Open Model License." The
  "Built on NVIDIA Cosmos" rule applies only to Cosmos models, not this one.
- Attribution: the Notice file above, plus CC-BY-4.0 attribution to Kyutai for the base.
- No field-of-use ban, but use must be "consistent with NVIDIA's Trustworthy AI terms"
  (an external, changeable document).
- Termination triggers: suing anyone over copyright/patent claims about the model ends the
  licence; bypassing or weakening a safety "Guardrail" without an equivalent ends it
  automatically.
- Revocable and unilateral: the licence is "revocable", and "NVIDIA may update this Agreement
  ... at any time"; you must comply with the update or stop. Uncapped indemnity runs from the
  user to NVIDIA for third-party claims.
- Practical friction: the weights are gated on Hugging Face (accept licence, `HF_TOKEN`). The
  server also downloads `config.json`, voices and the web UI from HF at start-up (issue #54:
  fails offline).
- Not diligenced: the training-data chain (Fisher corpus is LDC-licensed; Dia, Qwen, GPT-OSS
  outputs). NVIDIA licenses the result; we did not audit the upstream terms.

Third-party dependencies (`moshi/pyproject.toml`): torch (BSD), numpy, safetensors, einops,
sentencepiece, aiohttp (all permissive), `sphn` (Kyutai Opus bindings), `sounddevice`. Opus
codec library installed from the OS. No GPL/AGPL seen. The pins are tight (`torch>=2.2,<2.5`,
`huggingface-hub<0.25`, `aiohttp<3.11`); issue #22 reports it breaks on CPython 3.13.

## 3. Hardware and protocol

- Reference platform: NVIDIA CUDA. The Dockerfile uses `nvcr.io/nvidia/cuda:12.4.1`; compose
  reserves one NVIDIA GPU. The README adds a cu130 torch install for Blackwell.
- VRAM: not documented in the README. Weights alone are about 17 GB in bf16 (HF
  `usedStorage` 17.1 GB), so a 24 GB card is the practical floor; open issue #10 asks for 8 GB
  support and has no answer. Inference needs one 80 ms frame per 80 ms of audio; issue #103
  measures stock bf16 at 101.7 ms/frame on a GB10 (not real time), and shows a PR bringing it
  to 46.5 ms/frame with FP8 and depformer early exit. That is a community PR, not merged.
- CPU offload: `--cpu-offload` via `accelerate` exists, but it is for fitting, not speed
  (real time is unlikely), and issue #105 reports a crash at warmup.
- Apple silicon: not supported in the shipped code. In `server.py` the MPS branch is commented
  out and the device type is `Literal["cuda"] | Literal["cpu"]`. Open PRs/issues: #64 and #91
  (enable MPS), #25 (MPS bugs), #71 and #100 (MLX). Real-time on a Mac is unproven.
- Quantization: none shipped. FP8 only in the unmerged PR #103.
- ONNX export: no evidence anywhere. The model is a stateful streaming transformer with a
  custom depformer and per-step sampling; a faithful export is a research task, not a port.
- Server (`moshi/moshi/server.py`): aiohttp, one WebSocket endpoint `/api/chat`, optional TLS
  (`--ssl`), port 8998, one session at a time (a global `asyncio.Lock`). Query parameters:
  `voice_prompt` (file name), `text_prompt`, optional `seed`. Frames are binary, with a
  one-byte kind prefix: server sends `0x00` handshake after the system prompts are consumed,
  `0x01` + Opus audio bytes (24 kHz) and `0x02` + UTF-8 text piece; client sends `0x01` + Opus
  audio. The text stream is the model's own spoken-word transcript, token by token, which is
  useful to us (see below).
- Start-up cost: every session re-runs the voice and text prompt through the model before the
  handshake (`step_system_prompts_async`), so connection setup is a visible delay.

## 4. Fit for WeftOS

Ground truth read: ADR-061 (STT -> grounded agent -> dual-layer TTS, AEC, smart-turn), ADR-062
(backchannel as a `Continuer` cross-ref, never a turn), ADR-068 (daemon-side floor controller
driving a thin streaming edge), ADR-118 (push-to-talk console, STT -> agent -> optional spoken
replies, v1 on the Mac).

### What it does that our pipeline cannot

- True overlap. It listens while it speaks and produces backchannels, short interjections and
  barge-in handling inside one model, with no AEC-and-flush choreography.
- A persona voice with no separate TTS stage: role text plus a voice sample steer a
  consistent speaker. Our "voice trilemma" (ADR-061) goes away for this one model.
- Latency: the benchmark's 0.07 to 0.24 s turn-taking is below what a three-stage pipeline
  with a frontier LLM can reach.

### What it loses

- Tools and our LLM. All reasoning comes from a 7B speech model whose knowledge is Moshi's
  Helium base; nothing in the repo lets an agent step into a turn. ADR-118 needs the chief of
  staff to read project data, plan, and act, with model routing per task. PersonaPlex cannot be
  that agent.
- Determinism and audit. WeftOS chains every turn and tool call; an end-to-end audio model
  has no inspectable decision points beyond the text stream.
- ECC control. ADR-062/068 make the floor an ECC verdict actuated by the edge. In PersonaPlex
  the floor is inside the weights; we could observe it, not decide it. The "backchannel is a
  cross-ref, never a turn" invariant would have to be re-derived from the model's output.
- Language and size: English only; about 17 GB resident versus our small ONNX models.
- Offline/robustness: needs HF at start-up, one session per process, fixed role-prompt
  training distribution (assistant and customer-service roles plus casual chat).

### Could it replace or complement the pipeline

- Replace: no, for the reasons above.
- Complement: possible as an optional "social front end" mode for casual or low-stakes
  conversation, behind the same daemon-side `DuplexChannel` (ADR-068). The sound design:
  PersonaPlex is one more edge backend that streams audio over its WebSocket; its text
  stream (`0x02` frames) is mirrored into the conversation as the user-visible transcript and
  into ECC as utterances; when ECC or the agent decides a request needs tools or reasoning,
  the daemon suppresses or interrupts the model's speech and lets the existing path answer. A
  hand-off that clean needs a way to inject text into the model's stream, which does not exist
  today. Without it the model will keep talking and may answer on its own.
- Integration shapes, in order of sanity:
  1. Separate service on a CUDA box, reached over WebSocket (the protocol is already one
     route and four byte kinds). The host is the CUDA host (RTX 4070, 12 GB) with a quantized build.
  2. Sidecar process on the same machine, same protocol. Fine if the host has the GPU.
  3. ONNX export into `clawft-voice-onnx`: not feasible without research work. Rejected.
- Console fit: ADR-118 is push-to-talk, transcribed, with the agent doing real work. Full
  duplex adds little there and removes the transcript-as-input path. The one place it helps is
  optional spoken replies (C5), where the Mac-hosted TTS (Orpheus, Chatterbox) already work.
- Hosts: A Windows + WSL desktop (RTX 4070, 12 GB VRAM) is the mesh's CUDA host. The bf16
  weights (about 17 GB) do not fit; an 8-bit build (about 8.5 GB) would, but none ships yet
  (PR #103's opt-in quantization targets GB10 frame time and is untested in 12 GB), and the WSL CUDA driver is present. A trial there measures whether a quantized build
  holds 80 ms/frame on a 4070.

### Pattern-only takeaways

- Hybrid prompt: text role conditioning followed by a voice-sample segment on a silent
  channel. Worth knowing if we ever fine-tune or condition our own TTS.
- Training recipe: synthetic two-speaker dialogs from a two-speaker TTS (Dia), negative-
  duration silence to simulate barge-in, and Fisher for realistic backchannels. Useful as a
  data-generation reference for our backchannel classifier.
- Benchmarks: Full-Duplex-Bench (arXiv 2503.04721) and their Service-Duplex-Bench extension
  give us ready metrics (TOR, backchannel frequency/JSD, takeover latency) to score our own
  `DuplexChannel`.

## Open items to verify before revisiting

- Whether a quantized build holds 80 ms/frame on the CUDA host's RTX 4070 (12 GB).
- Whether PR #103 (opt-in quantization for GB10) lands and fits 12 GB, or an MLX port (#71/#100) lands and what real-time margin they show.
- The gated model-card files (safety/privacy) once someone accepts the HF terms.
- Whether a successor model adds a tool-call or text-injection seam.
