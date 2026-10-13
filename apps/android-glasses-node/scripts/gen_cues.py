#!/usr/bin/env python3
"""Render the glasses indicator cue WAVs (16 kHz mono PCM16) into the APK assets.

The synthesis mirrors crates/clawft-channels/src/voice/cue.rs (WEFT-664): sine tones with
8 ms raised-cosine ramps, quiet (peak <= 0.16 of full scale), each cue at most 250 ms.
`ack` and `working` are the Rust definitions verbatim; the other cues are new in the same
style (design section 3.7). Before writing anything the script checks that the tone
numbers for those two still appear in cue.rs and fails if the Rust side has moved on, so
the glasses never silently drift from the WeftOS cue voice.

Usage: gen_cues.py [--check]    (--check verifies the committed WAVs are up to date)
"""
import math
import pathlib
import re
import struct
import sys
import wave

RATE = 16000
RAMP_MS = 8
HERE = pathlib.Path(__file__).resolve().parent
APP = HERE.parent
OUT = APP / "app" / "src" / "main" / "assets" / "cues"
CUE_RS = HERE.parents[2] / "crates" / "clawft-channels" / "src" / "voice" / "cue.rs"

E5, A5, C5 = 659.25, 880.0, 523.25


def tone(freq, ms, amp, decay=None):
    """Sine with raised-cosine ramps; `decay` (seconds) adds an exponential tail."""
    n = RATE * ms // 1000
    ramp = min(RATE * RAMP_MS // 1000, n // 2)
    out = []
    for i in range(n):
        if i < ramp:
            env = (1 - math.cos(i / ramp * math.pi)) / 2
        elif i >= n - ramp:
            env = (1 - math.cos((n - i) / ramp * math.pi)) / 2
        else:
            env = 1.0
        if decay:
            env *= math.exp(-i / RATE / decay)
        s = math.sin(2 * math.pi * freq * i / RATE)
        out.append(int(s * env * amp * 32767))
    return out


def rest(ms):
    return [0] * (RATE * ms // 1000)


# name -> samples. `listening_start` is cue.rs CueKind::Ack, `thinking_tick` is
# CueKind::Working at lower amplitude (the loop repeats it); the rest are new.
CUES = {
    "listening_start": tone(E5, 70, 0.15) + rest(20) + tone(A5, 90, 0.12),
    "captured": tone(A5, 70, 0.12) + rest(20) + tone(E5, 90, 0.12),
    "thinking_tick": tone(C5, 80, 0.05),
    "done_ok": tone(784.0, 240, 0.14, decay=0.09),
    "done_error": tone(392.0, 70, 0.14) + rest(30) + tone(329.63, 110, 0.14),
}


def check_against_rust():
    src = CUE_RS.read_text()
    for needle in ("659.25, 70, 0.15", "880.0, 90, 0.12", "523.25, 80, 0.08"):
        if needle not in src:
            sys.exit(f"cue.rs no longer contains `{needle}`; update gen_cues.py to match")


def wav_bytes(samples):
    import io

    buf = io.BytesIO()
    with wave.open(buf, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(RATE)
        w.writeframes(struct.pack(f"<{len(samples)}h", *samples))
    return buf.getvalue()


def main():
    check_against_rust()
    for name, s in CUES.items():
        ms = len(s) * 1000 // RATE
        peak = max(abs(x) for x in s)
        assert ms <= 250, f"{name} is {ms} ms"
        assert peak <= 0.16 * 32767, f"{name} peak {peak}"
    if "--check" in sys.argv:
        bad = [n for n, s in CUES.items() if not (OUT / f"{n}.wav").exists()
               or (OUT / f"{n}.wav").read_bytes() != wav_bytes(s)]
        if bad:
            sys.exit(f"stale cue assets: {', '.join(bad)} (run gen_cues.py)")
        print("cue assets up to date")
        return
    OUT.mkdir(parents=True, exist_ok=True)
    for name, s in CUES.items():
        (OUT / f"{name}.wav").write_bytes(wav_bytes(s))
        print(f"{name}.wav  {len(s) * 1000 // RATE} ms  peak {max(abs(x) for x in s)}")


if __name__ == "__main__":
    main()
