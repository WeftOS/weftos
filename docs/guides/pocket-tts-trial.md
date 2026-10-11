# Pocket TTS trial (`pocket-tts` feature)

`clawft-voice-tts` can render the fast tier with Pocket TTS through the Gradium
`ptts` crate (pinned `=0.4.0-rc.1`, MIT OR Apache-2.0). It is a trial: off by
default, native-only, never part of the browser or WASI builds.

## Use

```bash
# Build/test with the feature (Accelerate / Metal variants: pocket-tts-accelerate, pocket-tts-metal)
scripts/build.sh test clawft-voice-tts --features pocket-tts

# Measure time-to-first-audio and real-time factor (cold + warm run)
cargo run --release -p clawft-voice-tts --features pocket-tts-accelerate \
  --example pocket_tts_bench -- <model dir> [voice] [text]
```

Config (`PocketTtsConfig`, JSON, unknown keys refused) or env:
`WEFTOS_POCKET_TTS_DIR` (checkpoint directory with `config.json`, weights,
optional `tokenizer.json` and voices), `WEFTOS_POCKET_TTS_VOICE`. Select it as the
fast engine with `WEFTOS_FAST_TTS=pocket-tts`; `auto` never picks it, and a missing
feature or model falls back to Kokoro with a warning. Weights are never downloaded.

Barge-in is dropping the generator: cancelling the `CancellationToken` ends the
iteration, which stops and joins ptts's two worker threads.

## Weights and voice licences

The crate code is MIT OR Apache-2.0. **Model weights are separate.** Pocket TTS
weights are gated on Hugging Face under CC-BY-4.0 (attribution required; accept the
gate before downloading), and individual voices carry their own licences. Re-check
the gate and each voice's licence before bundling or redistributing anything.
