# Watch-list review and new-things scan, 2026-10-11

Scope: re-check every item the repo and memory notes mark "watch", then list twelve new things
worth a look. All facts below were read from primary sources (GitHub API and READMEs, Hugging Face
model metadata, crates.io, arXiv/standards pages) on 2026-10-11. Nothing was downloaded or
installed; no weights were fetched. Where a number is a vendor or paper claim, it is labelled as
such. `search_ruvnet` was used once (RaBitQ); its snapshot is 10.5 days old and returned bench
source, not a measured result.

## Part A. Watch items re-checked

Where the watch items live: `docs/research/personaplex-eval.md`, `jepa-anything.md`, `h-jepa.md`,
`spatial-intelligence-2026/*` (S8, S11, VGGT family, V-JEPA, large-scale 3DGS), `diskann-and-large-scale-indexes.md`,
`antihunter-weftos-crosswalk.md`, `.planning/fork-adoption-build-plan-2026-07.md` (pocket-tts,
rabitq/FTS), and the memory notes `weavelogic-fork-evals-2026-07.md` and `ruv-ecosystem-2026-07.md`.
ADR statuses (Proposed) are not watch items in the same sense; ADR-068 is picked up in B1/B2.

Summary table (details follow):

| # | Item | Old verdict | New verdict | Deciding fact |
|---|------|-------------|-------------|---------------|
| A1 | pocket-tts (WEFT-657) | watch | **adopt as trial** (via Gradium `ptts`) | A Rust port with Core ML, `crates.io` release `0.4.0-rc.1` (2026-10-09), MIT OR Apache-2.0 code, loads Pocket TTS checkpoints. The "no official Rust" blocker is gone in practice. |
| A2 | PersonaPlex | watch | **still watch** (sidecar trial stays cheap to defer) | FP8 PR #103 is still open and now targets GB10, not a 12 GB card; no tool-call seam (issue #93 open, "live prompt injection" PR #69 closed unmerged); repo last pushed 2026-03-02. |
| A3 | JEPA-Anything | pattern-only + watch | **still watch** | No replication; repo untouched since 2026-09-18; we still have no trained LeWM predictor to attach it to. |
| A4 | H-JEPA | pattern-only, watch repo | **still watch** | Repo active (2026-10-10) but only README churn; no new checkpoints or closed-loop result. |
| A5 | V-JEPA 2.1 (as visual embedder) | watch | **promote to compose-when-producer** | 2.1 distilled ViT-B/16 (80M params, 384 px) checkpoint is in the MIT repo; the heavy-GPU worry no longer applies to the small tier. |
| A6 | PixelUMM | watch (C) | **still watch / drop from agenda** | Weights still non-commercial on HF (`nvidia/PixelUMM`, modified 2026-10-01, ungated), no camera control. |
| A7 | A LoD of Gaussians (SIGGRAPH 2026) | watch (waiting on code) | **code landed; still watch** | Code is public (`FelixWindisch/LoDOfGaussians`, 2026-07-03) but under a non-standard licence ("Other" in GitHub metadata); not usable in the product tree until read. |
| A8 | ruvector-rabitq | watch (Phase 4) | **prototype in the bench harness** | Crate is now real: v2.3.0 on crates.io (2026-09-26); but the headline recall claim is the SIGMOD 2024 paper's, not a ruvector measurement. |
| A9 | ruvector-diskann cold tier | deferred | **still deferred** | `Cargo.toml` pins `ruvector-diskann = "2.1"`; upstream is at 2.3.2 (2026-07-27) with hardening commits, but our blockers were our own (WEFT-660/661), not upstream. |
| A10 | agenticow | adopt as native crate | **done (native `clawft-cow-memory`)**; upstream idle | Upstream last pushed 2026-07-04, npm 0.2.4. Nothing new to wait for. |
| A11 | AgentBBS | patterns only | **still patterns only** | Licence is FSL 1.1 with MIT future licence (v0.2.0 released 2026-06-30, so the earliest conversion is mid-2028). |
| A12 | midstream `temporal-compare` | qualified go on vendoring | **still qualified go** | Crate 0.5.0 last updated 2025-09-27; repo active (Apache-2.0, 2026-10-10). Nothing changed. |
| A13 | Open-weight interactive video WMs (S8), Fast3R, VGGT-Omega, Atlas | watch | **still watch** | Not re-checked beyond status lines; no WeftOS consumer exists, so no new fact can flip them. |
| A14 | Antihunter RadarNode 24 GHz | watch | **still watch** | No hardware in the inventory; not re-checked. |

### A1. pocket-tts (WEFT-657): watch -> adopt as a trial

- **What we said** (`.planning/fork-adoption-build-plan-2026-07.md` section 4): official
  implementation is pure Python, which `clawft-voice-tts` forbids ("No Python"); community
  Rust/ONNX ports were single-maintainer forks; no tag performance so it cannot replace Orpheus.
  Unblock condition: an official Rust/ONNX build.
- **What changed.**
  - Upstream is very active: `kyutai-labs/pocket-tts` v3.3.0 on 2026-09-24 (v3.1.0 2026-09-03), six
    languages plus community models. Still no official ONNX/Candle export; the README says
    "We don't have official support for this yet" and lists community ports.
  - The relevant new fact: **Gradium's `xn-ptts` ("Phonon")**, written by Laurent Mazare (the
    author of Kyutai's Rust stack, and listed in the Pocket TTS README as `pocket-tts-xn`). Apache-2.0
    repo, README states code is **MIT OR Apache-2.0**. Rust runtime, Python wheels, npm/WASM, native
    downloads, Core ML on Apple devices, a Snapdragon QNN preview, q8 GGUF weights, an OpenAI-compatible
    server image, 40M and 90M models, five languages, "compatible Pocket TTS checkpoints use the same
    explicit loading interfaces". Crate `ptts` is on crates.io at `0.4.0-rc.1` (2026-10-09).
    The README's performance table is still a TODO comment, so there are no published
    benchmarks; the 200 ms time-to-first-audio and 6x real-time figures are Kyutai's claims for
    the original Python model.
  - Model weights are separate and carry their own licences; Pocket TTS weights are gated CC-BY-4.0 on
    HF (re-check the gate and voice licences before bundling). Kyutai also republished
    `kyutai/pocket-tts` and a no-voice-cloning variant on 2026-10-01.
  - FluidAudio (Apache-2.0, v0.17.7, 2026-10-08) already ships PocketTTS and Kokoro on the Apple
    Neural Engine, which is a second route on the Mac.
- **New verdict: adopt as a feature-flagged trial** behind a new `pocket_tts.rs` sibling to
  `kokoro.rs`, using `ptts` as the engine. The "No Python" invariant holds. Keep it off by
  default until it is an rc-free release and we measure time-to-first-audio on the Mac and on
  the CUDA host's CPU. Its real prize, as the build plan said, is in-process barge-in cancel.
- **Deciding fact:** a first-party-adjacent Rust port with a crates.io release now exists.
- Sources: [pocket-tts releases](https://github.com/kyutai-labs/pocket-tts/releases),
  [pocket-tts README](https://github.com/kyutai-labs/pocket-tts#in-browser-implementations),
  [gradium-ai/xn-ptts](https://github.com/gradium-ai/xn-ptts),
  [crates.io ptts](https://crates.io/crates/ptts),
  [FluidAudio](https://github.com/FluidInference/FluidAudio).

### A2. NVIDIA PersonaPlex: still watch

- **What we said** (`docs/research/personaplex-eval.md`, 2026-10-11): no tool calls, stock build does
  not fit a 12 GB RTX 4070 (about 17 GB bf16), English-only, Python-only, gated licence. Would
  change if an FP8/8-bit build holds real time in 12 GB, or a tool/text-injection seam appears.
- **What changed** (checked today): nothing that flips it.
  - `NVIDIA/personaplex` last commit 2026-03-02, no tags, no releases.
  - PR #103 is open, but its title is "GB10 real-time serving: opt-in quantization +
    serve-path optimizations (2.2x: 101.7 -> 46.5 ms/frame)" and it is built for GB10-class
    (sm_121, unified memory) hardware. It adds fp8-e4m3 weight-only GEMV (Triton), depformer early
    exit and Mimi fast-path flags. The eval's "FP8, about 8.5 GB" description for #103 should be
    re-read; the PR body talks about frame time on GB10, not VRAM on a 4070. Not merged, depends
    on #102 (also open). Real time on a 12 GB Ada card is not demonstrated.
  - Tool-call support: issue #93 ("Tool call support?"), #66 (RAG and tool calls), no maintainer
    answer; PR #69 "Live prompt injection" was closed unmerged on 2026-03-18.
  - Apple Silicon: PR #100 (MLX runtime) and #64/#91 (MPS) are open, unmerged; issue #71 requests
    MLX. Our Mac is still not a target.
- **Verdict: still watch.** One fact decides it: no seam for tools or our LLM, which breaks ADR-118.
  The eval's pattern-only conclusions stand. Correct the PR #103 description in the eval doc.
- Sources: [repo](https://github.com/NVIDIA/personaplex), [PR 103](https://github.com/NVIDIA/personaplex/pull/103),
  [PR 69](https://github.com/NVIDIA/personaplex/pull/69), [issue 93](https://github.com/NVIDIA/personaplex/issues/93),
  [PR 100](https://github.com/NVIDIA/personaplex/pull/100).

### A3/A4. JEPA-Anything and H-JEPA: still watch

- JEPA-Anything (arXiv 2609.20800, Apache-2.0 repo): last push 2026-09-18, 298 stars, one open item.
  No replication seen. H-JEPA (arXiv 2610.06805, MIT): pushes on 2026-10-10 are README and teaser
  changes only. Both stay blocked on the same thing: WeftOS has no trained LeWM encoder/predictor
  (ADR-090 stubs).
- Useful new fact for the LeWM track: upstream LeWM (`lucas-maes/le-wm`, MIT, arXiv 2603.19312) ships
  pretrained checkpoints for four simulated environments on Hugging Face (`quentinll/lewm-pusht`,
  `-cube`, `-tworooms`, `-reacher`) and training data in HDF5. They are simulated-environment
  models, not sensor models, so they prove the pipeline, not the use case. Last push 2026-05-26.
- Verdict: **still watch** both. Re-check trigger is unchanged: our own first trained predictor on
  a real sensor stream (the 12x12 ft room plan in `docs/plans/world-model-data-and-training.md`).
- Sources: [JEPA-Anything](https://github.com/Gen-Verse/JEPA-Anything), [H-JEPA](https://github.com/kevinghst/H-JEPA),
  [LeWM](https://github.com/lucas-maes/le-wm).

### A5. V-JEPA 2.1 as a visual embedder: watch -> compose when a producer exists

- **What we said** (`spatial-intelligence-2026/vjepa-and-lewm.md`): watch dense-feature quality,
  weight licence, and GPU cost of ViT-g/G at 384.
- **What changed:** the `facebookresearch/vjepa2` README announces V-JEPA 2.1 (released 2026-03-16)
  and lists a distilled **ViT-B/16, 80M parameters, 384 px** checkpoint
  (`vjepa2_1_vitb_dist_vitG_384.pt`). The repo licence is MIT (LICENSE file read). The older concern
  about giant-model GPU cost does not apply to the B tier; an 80M ViT runs on the CUDA host or Apple
  Silicon without drama. The paper itself is CC BY-NC-ND, which is separate from the code and
  checkpoint terms (unchanged from the earlier note).
- **New verdict: compose when a producer exists** (a `VISUAL_FEATURES` consumer, ADR-090/world-model
  crates). No ticket needed until a camera cog wants it.
- Sources: [vjepa2 README](https://github.com/facebookresearch/vjepa2).

### A6. PixelUMM: still watch, drop from the agenda

- `nvidia/PixelUMM` on HF is ungated, licence "other" (NVIDIA One-Way Noncommercial), last modified
  2026-10-01. Code Apache-2.0 with one CC BY-NC file. Weights non-commercial and CUDA-only, so
  nothing changed. One pattern (lift-or-push) is already captured in `pixelumm.md`. Verdict: leave
  as watch, no recurring re-check; revisit only if a commercial-licence variant appears.

### A7. A LoD of Gaussians: still watch (the code now exists)

- The doc said "waiting on SIGGRAPH 2026 code". `FelixWindisch/LoDOfGaussians` is public (pushed
  2026-07-03, 75 stars) with the claim "seamless ultra-large 3DGS training and rendering on
  consumer GPUs"; GitHub reports licence "Other", which for 3DGS code usually means the
  non-commercial Inria terms. I did not read the LICENSE text. Next step: read the licence before
  anyone spends time on E1. Verdict: **still watch, licence read pending**.
- Source: [repo](https://github.com/FelixWindisch/LoDOfGaussians), [arXiv 2507.01110](https://arxiv.org/abs/2507.01110).

### A8/A9. ruvector rabitq and diskann

- `ruvector-rabitq` is on crates.io: 2.3.0 (2026-09-26), 584 downloads, with `BENCHMARK.md` and a
  `ruvector-rabitq-wasm` crate. The brain result is a benchmark runner that quotes the IVF-RaBitQ
  paper (SIGMOD 2024: 99.3% recall@10 versus 79.2% for IVF-PQ on SIFT1M, 1-bit codes). That is the
  paper's number, not a measurement of the crate. I did not run it.
- `ruvector-diskann` is at 2.3.2 (2026-07-27); our workspace pin is `"2.1"` (`Cargo.toml:364`). The
  memory note that `diskann` is not in default features (silent brute-force stub) still applies.
- Verdicts: **rabitq: prototype** in the vector-backend bench harness (`docs/brain/vector-backend-bench-2026-07.md`
  setup) as a quantizer in front of HNSW, to answer the zvec-edge question with data. **diskann:
  still deferred.** Hybrid/FTS (tantivy + RRF) remains an own-build item; nothing new found.
- Sources: [crates.io ruvector-rabitq](https://crates.io/crates/ruvector-rabitq),
  [crates.io ruvector-diskann](https://crates.io/crates/ruvector-diskann),
  [ruvector repo](https://github.com/ruvnet/ruvector).

### A10-A12. ruv ecosystem

- agenticow: MIT, npm 0.2.4, last pushed 2026-07-04. We already build `clawft-cow-memory` natively;
  no upstream wait remains. AgentBBS: FSL 1.1 -> MIT after two years; keep to patterns. midstream:
  Apache-2.0, repo active; `temporal-compare` crate unchanged at 0.5.0 (2025-09-27).

## Part B. A dozen new things, ranked by value-to-effort

Ranking blends value to the stated WeftOS goals against effort and risk, best first. "Card" means
a board card via `scripts/dashboard-board.mjs`.

### 1. Smart Turn v3.2 end-of-turn model (small, CPU, BSD-2)

- **Source:** [pipecat-ai/smart-turn](https://github.com/pipecat-ai/smart-turn) README, v3.2; last
  push 2026-01-29; BSD-2-Clause; weights at HF `pipecat-ai/smart-turn-v3`.
- **Summary:** an audio-in model that predicts whether the speaker has finished their turn. 23
  languages, 8 MB int8 CPU build (32 MB fp32 GPU build), inference as low as 10 ms on some CPUs and
  about 65 ms on a cloud instance (vendor-measured). Runs after VAD fires on a silence segment.
- **Why it matters:** `clawft-channels/src/voice/turn.rs` and `vad.rs` decide turns from energy and
  spectral gates (the 8-round VAD work). A learned end-of-turn probability is the missing signal for
  ADR-068's conversation floor and for reducing premature cut-ins, and it can feed the ECC floor
  decision as a score instead of a threshold.
- **Effort/risk:** low-medium / low. ONNX export exists; we already have `clawft-voice-onnx`. Risk:
  accuracy on our mic and speaker (accented, noisy); trained on its own data.
- **Next step:** prototype. Run it offline over recorded Talk Mode sessions beside the existing gate
  and compare end-of-turn decisions. Card.

### 2. Gradium Phonon / `ptts` Rust TTS (see A1)

- **Source:** [gradium-ai/xn-ptts](https://github.com/gradium-ai/xn-ptts), crates.io `ptts` rc, 2026-10-09.
- **Why it matters:** `clawft-voice-tts` gains an in-process streaming TTS with cloning and an
  Apple-native path, and barge-in becomes a dropped generator.
- **Effort/risk:** medium / medium (release candidate, no published benchmark, weights licence).
- **Next step:** prototype behind a feature flag, measure first-audio latency. Card (replaces WEFT-657's watch state).

### 3. LocalVQE: 4.8M-parameter streaming echo canceller

- **Source:** [localai-org/LocalVQE](https://github.com/localai-org/LocalVQE) v1.3 (2026-05-29),
  Apache-2.0, GGML/GGUF C++ engine; also wired into FluidAudio's SDK as "LocalVQE (4.8M)".
- **Summary:** causal single-pass AEC + noise suppression + dereverb for 16 kHz speech, 16 ms
  latency, runs on commodity CPUs. Weights on Hugging Face. Claims are the project's; not measured by us.
- **Why it matters:** `clawft-voice-aec` currently wraps WebRTC AEC3 (and the notes say the Apple VPIO
  path was a dead end). A learned AEC is a candidate to beat AEC3 on a laptop speaker with
  barge-in, which is the weakest part of any half-duplex-with-interruption design. It is also
  a possible edge-device AEC.
- **Effort/risk:** medium (GGML binding or ONNX conversion) / medium (needs far-end reference
  alignment, a known hard problem; quality on our rooms unknown).
- **Next step:** prototype a A/B harness using recorded render + capture pairs from the voicelab
  harness against AEC3. Card.

### 4. Moonshine v2 streaming ASR (MIT, Rust-friendly, small)

- **Source:** [arXiv 2602.12241](https://arxiv.org/abs/2602.12241) (2026-02-12);
  [moonshine-ai/moonshine](https://github.com/moonshine-ai/moonshine) v0.1.5 (2026-08-24).
- **Summary:** sliding-window "ergodic" streaming encoder; the paper claims accuracy on par with
  models 6x larger and low time-to-first-token; the repo says models are MIT by default (the
  non-streaming non-English legacy models are non-commercial), with tiny (1 MB class) to
  high-accuracy tiers, and bindings for Python, JS/WASM, iOS, Android, macOS, Linux, Windows and Pi.
- **Why it matters:** a second streaming STT for `clawft-channels/src/voice/stt.rs` that is small
  enough for the Pi-class cog hosts, not only the Mac/CUDA host.
- **Effort/risk:** medium / low-medium (English-only for the open streaming tier as stated; check the
  per-model licence listing before shipping).
- **Next step:** read the licence table, then prototype as an alternative STT engine and compare
  word error rate and time-to-first-token on our session recordings. Card.

### 5. tau3-bench with full-duplex voice evaluation

- **Source:** [sierra-research/tau2-bench](https://github.com/sierra-research/tau2-bench) v1.0.0
  (2026-03-18, v1.0.1 2026-07-22, MIT), tau-Voice paper [arXiv 2603.13686](https://arxiv.org/abs/2603.13686).
  Also [Full-Duplex-Bench v1.5/v2/v3](https://github.com/DanielLin94144/Full-Duplex-Bench)
  ([arXiv 2604.04847](https://arxiv.org/abs/2604.04847), v3 code released 2026-05-20).
- **Summary:** tau3-bench measures tool-using agents against simulated users with policy and pass^k
  reliability, and now includes voice full-duplex evaluation against real-time audio APIs plus a
  knowledge-retrieval domain. Full-Duplex-Bench scores pause handling, backchannel, turn-taking and
  interruption.
- **Why it matters:** we have no agent-level eval for the chief-of-staff (ADR-118) or the full-duplex
  floor (ADR-068). `clawft-bench-voice` exists but measures voice latency, not behaviour. pass^k is the
  right reliability measure for a tool-calling agent.
- **Effort/risk:** low (read) to medium (adapter so our agent can be driven as a provider) / low.
  The voice harness needs a real-time provider interface; Full-Duplex-Bench's repo licence shows
  "NOASSERTION" so check before copying code.
- **Next step:** read both papers, then card a scenario set adapted from them for `weft`'s own agent.

### 6. FunctionGemma 270M and small-model function calling

- **Source:** [Google blog](https://blog.google/technology/developers/functiongemma/); HF
  `google/functiongemma-270m-it` (created 2025-10-08, gated "manual" Gemma terms); BFCL v4
  ([leaderboard](https://gorilla.cs.berkeley.edu/leaderboard.html)).
- **Summary:** a 270M Gemma 3 variant trained for function calling; Google reports 58% to 85% on its
  Mobile Actions task after fine-tuning. A survey paper, [TinyLLM, arXiv 2511.22138](https://arxiv.org/abs/2511.22138),
  reports multi-turn tool-calling collapses for sub-2B models (for example Qwen3-1.7B at 16.9% multi-turn
  on BFCL versus 55.5% overall).
- **Why it matters:** WEFT-632 found tool-choice only advisorily honoured by llama.cpp with Hermes.
  A tiny local router model that only emits a function call (then a bigger model answers) is a
  cheaper reliability lever for the voice fast-ack tier and for ESP-class leaf commands.
- **Effort/risk:** medium / medium (Gemma licence terms, multi-turn weakness, English).
- **Next step:** read, then run BFCL-style single-turn checks on our own tool schema offline. No code.

### 7. WASI 0.3 is ratified; Wasmtime 49/50 track it

- **Source:** [WASI v0.3.0 release notes](https://github.com/WebAssembly/WASI/releases/tag/v0.3.0)
  (2026-06-11, v0.3.1 2026-08-11); [Wasmtime releases](https://github.com/bytecodealliance/wasmtime/releases)
  (v49.0.2 and v48.0.5 on 2026-10-02, v50.0.0-rc.1 on 2026-10-05).
- **Summary:** async becomes native to the Component Model (`stream<T>`, `future<T>`, `async func`
  replacing `wasi:io` pollables). The Wasmtime 48.0.4 / 49 security releases fixed wasip3-related
  host panics and an async-callback stack overflow (GHSA-mr2v-56j5-cmfc and the one linked from the
  48.0.3 notes), plus a WASI preview0 fuel bypass.
- **Why it matters:** the workspace pins `wasmtime 48.0.4` with `wasmtime-wasi` feature `p1` only
  (`Cargo.toml:289-290`), and `clawft-wasm-host` carries an explicit pin-in-lockstep comment. ADR-115
  (`rvm` workload kind) and the Wasmtime workloads direction would benefit from component-model
  streams for sensor cogs. Our 48.0.4 pin already has the security fixes, which is good.
- **Effort/risk:** medium-high / medium (API churn between 48/49/50; p3 is new). Not for rc.6.
- **Next step:** card a spike: build one sensor-stream component against WASI 0.3 on a Wasmtime
  49/50 branch and measure host overhead. Keep p1 as default.

### 8. MCP specification 2026-07-28: stateless core, Extensions, Tasks

- **Source:** [MCP changelog](https://modelcontextprotocol.io/specification/latest/changelog)
  (spec dated 2026-07-28; summarised by [AAIF](https://aaif.io/blog/mcp-2026-07-28-whats-changing-and-how-to-migrate)).
- **Summary (secondary summaries agree; read the changelog before acting):** protocol-level sessions
  and the `initialize` handshake are removed, requests carry version and capabilities in `_meta`,
  Tasks become an extension, OAuth gets issuer (RFC 9207) validation, and Roots, Sampling and stdio
  logging are deprecated with at least twelve months' notice.
- **Why it matters:** WeftOS exposes and consumes MCP in several places (gateway, `.mcp.json`,
  Seed/Cognitum bridges, `clawft-services` MCP). A stateless core simplifies running MCP over the
  mesh; deprecated Sampling matters if any server relies on it.
- **Effort/risk:** low (audit) / low.
- **Next step:** read the changelog and card an audit of our MCP server/client crates against the
  new `_meta` and Tasks shape.

### 9. WiFi sensing: IEEE 802.11bf-2025 and CSI foundation models

- **Sources:** IEEE Std 802.11bf-2025 (approved 2025-05-28, published 2025-09-26; confirmed by
  multiple index pages, see the [overview paper](https://arxiv.org/abs/2310.17661));
  [Scale What Counts, Mask What Matters, arXiv 2511.18792](https://arxiv.org/abs/2511.18792);
  [AM-FM: A Foundation Model for Ambient Intelligence Through WiFi, arXiv 2602.11200](https://arxiv.org/abs/2602.11200).
- **Summary:** 802.11bf standardises WLAN sensing procedures in sub-7 GHz and 45 GHz bands
  (bistatic/multistatic). The 2511.18792 study pretrains on 14 datasets (1.32 million CSI samples,
  four devices) and reports cross-domain accuracy gains of 2.2 to 15.7 points over supervised
  baselines, with data scale, not model size, as the limiter.
- **Why it matters:** RuView, the ESP32-C5 port, ESPARGOS notes and the world-model plan are CSI based.
  The standard is the long-term reason to keep CSI ingestion behind a vendor-neutral
  `weftos-sensor-pipeline` contract (ADR-104 area), and the foundation-model result says to
  collect heterogeneous data from the start of the 12x12 ft room plan, not one clean room.
- **Effort/risk:** low (read) / low.
- **Next step:** read; add a short "802.11bf alignment" note to the sensor-node architecture doc. Card only the data-diversity item.

### 10. Noise-free secure mesh options: iroh 1.x and OpenMLS 0.9

- **Sources:** [n0-computer/iroh](https://github.com/n0-computer/iroh) v1.3.0 (2026-09-28), Apache-2.0;
  [openmls/openmls](https://github.com/openmls/openmls) v0.9.0 (2026-08-25), MIT.
- **Summary:** iroh is a Rust QUIC library with key-based dialing (dial an Ed25519 public key; relays
  and hole-punching) and a 1.x stable line. OpenMLS is the Rust implementation of RFC 9420 Messaging
  Layer Security for group key agreement with forward secrecy and post-compromise security.
- **Why it matters:** the mesh already has Ed25519 node identity (ADR-025), signed envelopes (ADR-063),
  anti-entropy (ADR-064), subkeys (ADR-065) and capability tokens (ADR-066), all Proposed.
  iroh is a candidate transport for `clawft-mesh-service` between non-tailnet nodes; MLS is a
  candidate for group keys on the multi-agent bus (shared project channels) instead of inventing
  one. Neither fits an ESP32 leaf.
- **Effort/risk:** medium-high / medium (a transport decision touches ADR-099/116; MLS is a big
  surface). I did not evaluate Noise (`snow`) separately; the likely answer for leaves is a Noise
  XX/IK handshake over the existing link.
- **Next step:** read and write a short comparison (iroh vs current transport vs Noise on leaves)
  before any prototype.

### 11. Kyutai STT and MLX-native speech on Apple Silicon (Mac path)

- **Sources:** [kyutai-labs/delayed-streams-modeling](https://github.com/kyutai-labs/delayed-streams-modeling)
  (Apache-2.0): `kyutai/stt-1b-en_fr` (about 1B params, 0.5 s delay, semantic VAD, English and
  French) and `kyutai/stt-2.6b-en`, with a Rust server and MLX implementations;
  [FluidAudio](https://github.com/FluidInference/FluidAudio) (Parakeet TDT v3 ASR, Kokoro and
  PocketTTS, Silero VAD, diarization, LocalVQE, all on the Apple Neural Engine; Apache-2.0, v0.17.7 2026-10-08);
  [mlx-audio](https://github.com/Blaizzy/mlx-audio) (MIT, v0.5.8 2026-10-05).
- **Summary:** three ways to run streaming STT/TTS natively on a Mac, one with an included semantic VAD.
- **Why it matters:** the Mac is the daily-driver; today voice relies on Kokoro/Orpheus and ONNX. A
  semantic-VAD STT is the same idea as item 1 in a different place, and FluidAudio shows the ANE path.
  Caveat: Swift SDK, not Rust, so adoption means a sidecar or Core ML export, which our "no Python"
  rule allows only if Rust-native.
- **Effort/risk:** medium / medium.
- **Next step:** read; pick one STT to compare against the current stack alongside item 4.

### 12. Agent memory benchmarks (ICLR 2026) for the RVF memory layer

- **Sources:** ICLR 2026 posters on [AMA-Bench](https://iclr.cc/virtual/2026/10021275),
  [Evaluating Memory in LLM Agents via Incremental Multi-Turn Interactions](https://iclr.cc/virtual/2026/poster/10010781)
  (MemoryAgentBench), AgentMemoryBench; LongMemEval.
- **Summary:** benchmarks that test four memory competencies: accurate retrieval, test-time
  learning, long-range understanding, and selective forgetting, in agentic settings rather than
  plain chat. Vendor leaderboards (for example Mem0's own blog) are self-reported; treat them as such.
- **Why it matters:** the brain (RVF/ruvector namespaces), `clawft-cow-memory` checkpoints and the
  hermes-loop memory have no published evaluation; selective forgetting maps to the tombstone and
  graft-echo guard fixes (WEFT-664/665).
- **Effort/risk:** low-medium / low.
- **Next step:** read the three papers; card a small regression suite run against the brain using
  public task shapes.

## Candidates for rc.6 or the next cycle

Small and ready (each one-card sized, no new dependency decisions):

1. **Correct the PersonaPlex eval's PR #103 description** (it targets GB10 frame time, not 12 GB VRAM). Doc edit.
2. **Smart Turn offline comparison** over recorded Talk Mode sessions (item 1). No runtime change.
3. **`ptts` trial behind a feature flag** in `clawft-voice-tts` (A1, item 2). Default off.
4. **Read the LoD-of-Gaussians LICENSE** and update `large-scale-3dgs-lod.md` (A7). 10 minutes.
5. **Move V-JEPA 2.1 ViT-B to "compose when a producer exists"** in `vjepa-and-lewm.md` (A5). Doc edit.
6. **MCP 2026-07-28 audit card** (item 8): read the changelog, list our touchpoints.
7. **RaBitQ in the vector bench harness** (A8): one extra runner, no product change.

Larger, next cycle: LocalVQE A/B against AEC3 (item 3), Moonshine v2 / Kyutai STT comparison
(items 4, 11), WASI 0.3 spike on Wasmtime 49/50 (item 7), tau3 / Full-Duplex-Bench scenario set (item 5).

## Not verified

- The `ptts` benchmark numbers (none published), LocalVQE quality, Smart Turn accuracy on our audio,
  RaBitQ recall in the ruvector crate, and the LoD-of-Gaussians licence terms were not tested.
- Items A13 and A14 were not re-checked beyond their current status in the repo.
- The MCP 2026-07-28 summary came from secondary summaries of the spec plus the spec URL; the
  changelog itself was not fetched.
- 802.11bf publication date is from index pages, not the IEEE Xplore record.
