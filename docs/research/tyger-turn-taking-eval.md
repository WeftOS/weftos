# TYGER (NineNineSix) turn-taking model: evaluation for the CoS voice loop

Read 2026-10-17 from [nineninesix/tyger](https://huggingface.co/nineninesix/tyger) (created
2026-10-12, updated 2026-10-16) and the reference agent
[nineninesix-ai/tyger-s2s](https://github.com/nineninesix-ai/tyger-s2s). Card, evaluation and
license read; the 2.6 GB weights were not downloaded or run. Numbers below are the publisher's.

## Verdict: spike it (hands-free mode only); push-to-talk stays the default

It is the first open model that produces, every 80 ms, exactly the floor signals ADR-068's
`DuplexChannel` consumes (end of turn, barge-in, backchannel), with a permissive license. Its
published error rates are too high to trust blind, and none were measured on our audio, so it
earns a bounded spike on the GPU host, not a design change.

## What it is

- **NVIDIA Nemotron 3.5 ASR Streaming 0.6B, unchanged** (decoder and tokenizer included), plus a
  **14M-parameter causal GRU** "turn-taker" (3 layers, width 768) trained on Nemotron's layer-15
  features. 652M parameters, FP32, 2.61 GB.
- **Two aligned 16 kHz mono inputs:** the user's mic and the agent's own playback. 320 ms hops,
  8 s re-encode window per stream, no lookahead, 9 KiB of GRU state per conversation.
- **Per-frame output:** turn state (listen / hold / speak / backchannel) plus hazard, barge-in,
  backchannel and endpoint scores. App-side events with guards: endpoint (0.91), barge-in (0.50),
  backchannel (0.40), 350 ms of speech minimum, 1.2 s silence fallback.
- **Speed:** GRU 0.8 ms/frame on an RTX 4090; about 4 GB of GPU memory for 4 conversations in
  tyger-s2s. English only. The weights repo has no inference code; tyger-s2s
  (`app/plugins/ears/tyger/`) is the runtime.

## Published evaluation (offline, endpoint 0.91 / barge-in 0.93 / backchannel 0.80)

| | Open Yap 1K test | Full-Duplex-Bench v1.0 (CANDOR) |
|---|---|---|
| Endpoint latency, median / mean | 252 / 465 ms | 308 / 383 ms |
| Early cut-ins (endpoint before the turn ended) | **29.9%** | **25.2%** |
| No endpoint within 1.2 s | 7.4% | 5.0% |

Open Yap test also: turn-state macro-F1 0.94; barge-ins caught within 640 ms **25%** (median
400 ms); no false barge-ins in 4.7 h of clean speech; backchannels caught 15%, fired 135/hour.
Limits they state: offline diagnostics only, one training seed, eval sets guided development, no
live agent benchmark, Full-Duplex-Bench adapted (silent agent channel).

## License

OpenMDW-1.1 for TYGER's GRU and files, and NVIDIA's Nemotron under its own OpenMDW-1.1 grant:
use, modify and distribute "without restriction", keeping the license and notices. Commercial CoS
use is fine. tyger-s2s is Apache-2.0. Training data (Open Yap 1K) keeps its own terms and is not
included.

## Fit for the CoS voice loop

Ground truth: ADR-061 (smart-turn semantic endpointing, AEC barge-in), ADR-062 (a backchannel is
a `Continuer` cross-ref, never a turn), ADR-068 (daemon-side `DuplexChannel` floor controller on
the 50 ms tick; the edge stays dumb), and today's P0 path (glasses push-to-talk → phone relay →
cloud speech workflow with ElevenLabs STT/TTS).

- **Where it fits:** as a floor-verdict provider for ADR-068. Its per-frame endpoint, barge-in
  and backchannel scores are the inputs the `DuplexChannel` expects, and its backchannel output
  maps onto ADR-062's `Continuer` without changing that decision.
- **What it could replace:** smart-turn for endpointing, and possibly ElevenLabs STT on the
  local path (Nemotron streaming ASR, no per-turn cost, offline-capable). Neither until measured.
- **What it needs from us:** the agent-playback stream aligned with the mic. The speech workflow
  renders the TTS, so it has the samples; aligning them to what the glasses actually played needs
  the `playback.done` / `spoken_through_seq` reports the glasses already send.
- **Placement:** a GPU service. aepod-xpc (RTX 4070, 12 GB) fits several conversations at the
  published ~1 GB each. Not Vercel functions (no GPU). Apple Silicon is untested. It would be a
  `speech.v1` provider the placement layer picks, as the phone plan already allows.
- **Why not default yet:** a 25-30% early cut-in rate means a CoS that interrupts people a
  quarter of the time on conversational speech. Barge-in recall (25% within 640 ms) and
  backchannel recall (15%) are low at the published thresholds. Our glasses mic is quiet (rms in
  the hundreds), unlike the clean two-channel training data. Push-to-talk (built 2026-10-14)
  avoids all of this for now.

## Spike (if approved)

1. Run tyger-s2s (Docker) on aepod-xpc; confirm GPU memory and per-conversation latency.
2. Replay recorded CoS turns (phone debug WAVs plus agent TTS) through it and through smart-turn;
   compare endpoint latency, early cut-ins and misses on our audio, and Nemotron WER against
   ElevenLabs on the same clips.
3. If it beats smart-turn on our audio: wrap it as a `speech.v1` floor-verdict provider for the
   `DuplexChannel`, behind a hands-free mode flag, with thresholds tuned per device.

## Related

- [personaplex-eval.md](personaplex-eval.md): NVIDIA PersonaPlex (full speech-to-speech; verdict
  "watch"). TYGER is the narrower piece: decisions only, with our own LLM and TTS kept.
- ADR-061, ADR-062, ADR-068; `docs/plans/phone-parent-node.md` (speech workflow placement).
