#!/usr/bin/env python3
"""Text-to-speech on the Ventuno CPU: generation time and RTF per sentence; writes wavs for round-trip ASR.
Usage: tts.py <engine> <outdir>   engine: piper-amy | kokoro | phonon"""
import json, os, subprocess, sys, time
import numpy as np
import soundfile as sf
eng, outd = sys.argv[1:3]; os.makedirs(outd, exist_ok=True)
B = "/data/bench"; M = f"{B}/models"
TEXT = ["The meeting moved to Thursday at 3:45 PM in room 12B.",
        "WeftOS joined the mesh and reported its node ID to the dashboard.",
        "Please call Doctor Okonkwo back before the 28th of October.",
        "The sensor read 21.7 degrees and 48 percent humidity.",
        "Turn left at the second light, then continue for two kilometres.",
        "Order 4471 shipped from Toronto and arrives on Monday.",
        "The Qualcomm NPU ran Gemma at twenty tokens per second.",
        "Battery is at 9 percent; plug the glasses in soon.",
        "Kokoro and Piper both run locally without a network.",
        "Reply yes to confirm, or no to cancel the transfer."]
if eng in ("piper-amy", "kokoro"):
    import sherpa_onnx
    if eng == "piper-amy":
        d = f"{M}/vits-piper-en_US-amy-medium"
        cfg = sherpa_onnx.OfflineTtsConfig(model=sherpa_onnx.OfflineTtsModelConfig(vits=sherpa_onnx.OfflineTtsVitsModelConfig(
            model=f"{d}/en_US-amy-medium.onnx", tokens=f"{d}/tokens.txt", data_dir=f"{d}/espeak-ng-data"), num_threads=8))
        sid = 0
    else:
        d = f"{M}/kokoro-en-v0_19"
        cfg = sherpa_onnx.OfflineTtsConfig(model=sherpa_onnx.OfflineTtsModelConfig(kokoro=sherpa_onnx.OfflineTtsKokoroModelConfig(
            model=f"{d}/model.onnx", voices=f"{d}/voices.bin", tokens=f"{d}/tokens.txt", data_dir=f"{d}/espeak-ng-data"), num_threads=8))
        sid = 0
    tts = sherpa_onnx.OfflineTts(cfg); tts.generate("Warm up.", sid=sid)
    def synth(text, path):
        a = tts.generate(text, sid=sid); sf.write(path, np.asarray(a.samples, dtype=np.float32), a.sample_rate); return len(a.samples) / a.sample_rate
else:
    exe = f"{B}/ptts/bin/ptts"
    subprocess.run([exe, "--model", "hf://gradium/phonon-31m-en", "--lang", "en", "--quant", "q8", "Warm up.", "-o", f"{outd}/warm.wav"], capture_output=True)
    def synth(text, path):
        subprocess.run([exe, "--model", "hf://gradium/phonon-31m-en", "--lang", "en", "--quant", "q8", text, "-o", path], capture_output=True, check=True)
        return sf.info(path).duration
rows = []
for i, t in enumerate(TEXT):
    p = f"{outd}/{eng}-{i:02d}.wav"; t0 = time.time(); dur = synth(t, p); gen = time.time() - t0
    rows.append({"i": i, "text": t, "wav": p, "gen_s": round(gen, 3), "audio_s": round(dur, 2), "rtf": round(gen / dur, 3)})
gen = sum(r["gen_s"] for r in rows); aud = sum(r["audio_s"] for r in rows)
json.dump({"engine": eng, "host": "ventunoq-cpu8", "rows": rows, "gen_s": round(gen, 2), "audio_s": round(aud, 2),
           "rtf": round(gen / aud, 3)}, open(f"{outd}/{eng}.json", "w"), indent=1)
print(f"[tts] {eng}: rtf={gen / aud:.3f} (x{aud / gen:.1f} realtime) median_gen_s={sorted(r['gen_s'] for r in rows)[5]}")
