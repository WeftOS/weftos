#!/usr/bin/env python3
"""Correlate counted loss with the camera being on: camera_corr.py RUN_DIR phase start_epoch"""
import json, sys, bisect
run, phase, start = sys.argv[1], sys.argv[2], int(sys.argv[3])
env = []
for l in open(f"{run}/env.csv"):
    p = l.strip().split(",")
    if len(p) >= 10 and p[0].isdigit():
        env.append((int(p[0]), int(p[9] or 0)))
env.sort()
ts = [e[0] for e in env]
def cam(t):
    i = bisect.bisect_left(ts, t)
    near = [env[j][1] for j in (i - 1, i, i + 1) if 0 <= j < len(env) and abs(env[j][0] - t) <= 12]
    return max(near) if near else 0
rows = [json.loads(l) for l in open(f"{run}/{phase}-cog.out") if l.startswith("{")]
rows = [r for r in rows if "samples" in r]
prev = None
agg = {}
for r in rows:
    t = start + r["uptime_s"]
    if prev:
        dl = r["samples"]["lost"] - prev["samples"]["lost"]
        dd = r["samples"]["delivered"] - prev["samples"]["delivered"]
        key = (cam(t) > 0)
        a = agg.setdefault(key, [0, 0, 0, 0.0])
        a[0] += dd; a[1] += dl; a[2] += 1
        a[3] += r["uptime_s"] - prev["uptime_s"]
    prev = r
for k, (dd, dl, n, secs) in sorted(agg.items()):
    exp = dd + dl
    print(f"camera {'ON ' if k else 'off'}: {n:4d} reports {secs:8.0f} s  delivered {dd:,}  lost {dl:,}  = {100*dl/max(exp,1):.3f} % of delivered+lost")
