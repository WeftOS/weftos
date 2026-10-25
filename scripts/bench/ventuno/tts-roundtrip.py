#!/usr/bin/env python3
"""Build an ASR manifest from a tts.py result so the wavs can be transcribed back (intelligibility check).
Usage: tts-roundtrip.py <tts-result.json>   -> /data/bench/asr/tts-<engine>.jsonl"""
import json, os, sys
d = json.load(open(sys.argv[1])); B = "/data/bench"
out = f"{B}/asr/tts-{d['engine']}.jsonl"
with open(out, "w") as f:
    for r in d["rows"]:
        f.write(json.dumps({"audio": os.path.relpath(os.path.abspath(r["wav"]), f"{B}/asr"), "text": r["text"]}) + "\n")
print(out)
