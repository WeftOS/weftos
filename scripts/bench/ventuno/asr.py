#!/usr/bin/env python3
"""Speech-to-text on the Ventuno CPU. Writes hyps + timing; WER is scored on the Mac with voicelab.metrics.
Usage: asr.py <impl> <dataset> <out.json>
  WHISPER_LANG=auto for multilingual sets (default en).
  impl: parakeet | whisper-base.en | whisper-small.en | whisper-large-v3-turbo-q5_0"""
import json, os, subprocess, sys, time
import soundfile as sf
impl, ds, out = sys.argv[1:4]
B = "/data/bench"; M = f"{B}/models"
rows = [json.loads(l) for l in open(f"{B}/asr/{ds}.jsonl")]
if impl == "parakeet":
    import sherpa_onnx
    d = f"{M}/sherpa-onnx-nemo-parakeet-tdt-0.6b-v2-int8"
    rec = sherpa_onnx.OfflineRecognizer.from_transducer(encoder=f"{d}/encoder.int8.onnx", decoder=f"{d}/decoder.int8.onnx",
        joiner=f"{d}/joiner.int8.onnx", tokens=f"{d}/tokens.txt", num_threads=8, model_type="nemo_transducer")
    def run(path):
        a, sr = sf.read(path, dtype="float32"); s = rec.create_stream(); s.accept_waveform(sr, a); rec.decode_stream(s); return s.result.text
else:
    model = impl.split("whisper-", 1)[1]
    def run(path):
        r = subprocess.run([f"{B}/whisper.cpp/build/bin/whisper-cli", "-m", f"{B}/whisper.cpp/models/ggml-{model}.bin",
                            "-f", path, "-nt", "-np", "-t", "8", "-l", os.environ.get("WHISPER_LANG", "en")], capture_output=True, text=True)
        return r.stdout.strip()
res = []; audio_s = 0.0; t_all = 0.0
for r in rows:
    p = f"{B}/asr/{r['audio']}"; info = sf.info(p); audio_s += info.duration
    t = time.time(); hyp = run(p); dt = time.time() - t; t_all += dt
    res.append({"audio": r["audio"], "ref": r["text"], "hyp": hyp, "s": round(dt, 3), "dur": round(info.duration, 2)})
json.dump({"impl": impl, "dataset": ds, "host": "ventunoq-cpu8", "n": len(res), "audio_s": round(audio_s, 1),
           "decode_s": round(t_all, 1), "rtfx": round(audio_s / t_all, 1), "rows": res}, open(out, "w"), indent=1)
print(f"[asr] {impl} {ds}: n={len(res)} rtfx={audio_s / t_all:.1f}")
