#!/usr/bin/env python3
"""Vision pack on the Ventuno via `geniex infer` (image passed as <abs path> in the prompt).
Usage: VENQ_HOST=user@board vlm.py <model> <out.json>   (pack in /data/bench/vlm on the board)"""
import json, os, re, subprocess, sys, time
model, out = sys.argv[1], sys.argv[2]
BOARD = os.environ.get("VENQ_HOST", "arduino@ventunoq"); REMOTE = "/data/bench/vlm"
pack = json.load(open(__file__.rsplit("/", 1)[0] + "/vlm-pack/pack.json"))
ANSI = re.compile(r"\x1b\[[0-9;?]*[A-Za-z]|\[\?[0-9;]*[a-z]|\r")
def check(c, text):
    t = text.lower()
    if c["type"] == "contains":
        return all(w.lower() in t for w in c.get("all", [])) and (not c.get("any") or any(w.lower() in t for w in c["any"]))
    if c["type"] == "numeric":
        nums = re.findall(r"-?\d+(?:\.\d+)?", text)
        return bool(nums) and float(nums[-1]) == float(c["answer"])
rows = []
for it in pack:
    cmd = (f"export PATH=$HOME/.local/bin:$PATH; geniex infer '{model}' --compute npu --max-tokens 48 "
           f"--think=false --prompt \"{it['prompt']} <{REMOTE}/{it['image']}>\" 2>&1")
    t0 = time.time()
    raw = subprocess.run(["ssh", BOARD, cmd], capture_output=True, text=True, timeout=900).stdout
    wall = time.time() - t0
    clean = ANSI.sub("", raw)
    m = re.search(r"([\d.]+) tok/s\s*•\s*(\d+) tok\s*•\s*([\d.]+) s first token", clean)
    # the answer is the text after the echoed prompt line and before the stats line
    body = clean.split("— ")[0] if "— " in clean else clean
    ans = body.split(">", 1)[-1].replace("encoding...", "").strip().splitlines()[-1:] if body else []
    ans = ans[0].strip() if ans else ""
    ok = check(it["check"], ans)
    rows.append({"id": it["id"], "pass": ok, "answer": ans[:120], "tps": float(m.group(1)) if m else None,
                 "first_token_s": float(m.group(3)) if m else None, "wall_s": round(wall, 1)})
    print(f"  [{'✓' if ok else '✗'}] {it['id']:16} ans={ans[:40]!r} ttft={rows[-1]['first_token_s']} wall={wall:.0f}s", flush=True)
res = {"model": model, "host": "ventunoq-npu", "pack": "vision-objective-8", "n": len(rows),
       "passed": sum(r["pass"] for r in rows), "items": rows}
json.dump(res, open(out, "w"), indent=1)
print(f"[vlm] {model}: {res['passed']}/{res['n']} -> {out}")
