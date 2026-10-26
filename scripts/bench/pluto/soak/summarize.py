#!/usr/bin/env python3
"""Summarise a Pluto soak run directory: python3 summarize.py RUN_DIR [phase1|phase2]"""
import json, sys, statistics, glob, os
run = sys.argv[1]
phases = sys.argv[2:] or ["phase1", "phase2"]
for ph in phases:
    f = os.path.join(run, f"{ph}-cog.out")
    if not os.path.exists(f):
        continue
    rows = []
    for l in open(f):
        try:
            rows.append(json.loads(l))
        except Exception:
            pass
    rows = [r for r in rows if "samples" in r]
    if not rows:
        print(ph, "no reports")
        continue
    last = rows[-1]
    s, c = last["samples"], last["counters"]
    print(f"== {ph}: {len(rows)} reports over {last['uptime_s']:.0f} s")
    print(f"   delivered {s['delivered']:,}  expected {s['expected']:,}  lost {s['lost']:,}  pending {s['pending_unconfirmed']:,}  loss {100*s['loss_fraction']:.4f} %")
    print(f"   gap_events {c['gap_events']}  overflows {c['overflows']}  zero_blocks {c['zero_blocks']}  disconnects {c['disconnects']}  apply_errors {c['apply_errors']}  refill_errors {c['refill_errors']}  retunes {c['retunes']}")
    print(f"   bus: {last['bus']}")
    tp = [r['hw']['temp_phy_c'] for r in rows if r['hw'].get('temp_phy_c') is not None]
    tz = [r['hw']['temp_zynq_c'] for r in rows if r['hw'].get('temp_zynq_c') is not None]
    if tp:
        print(f"   AD9364 temp C: min {min(tp):.1f} mean {statistics.mean(tp):.1f} max {max(tp):.1f}")
    if tz:
        print(f"   Zynq temp C:   min {min(tz):.1f} mean {statistics.mean(tz):.1f} max {max(tz):.1f}")
    bad = [r for r in rows if not r.get("healthy")]
    print(f"   unhealthy reports: {len(bad)}")
    duty = last.get("duty", [])
    if duty:
        print("   duty:", ", ".join(f"{d['plan']}={d['seconds']:.0f}s" for d in duty))
    seen = {}
    for r in rows:
        b = r.get("band") or {}
        seen[b.get("centre_hz")] = seen.get(b.get("centre_hz"), 0) + 1
    print("   reports per centre:", seen)
env = os.path.join(run, "env.csv")
if os.path.exists(env):
    P = []; V = []; Cur = []; usb0 = 0; ping0 = 0; n = 0
    for l in open(env):
        p = l.strip().split(",")
        if len(p) < 7 or not p[1].isdigit():
            continue
        n += 1
        P.append(int(p[1]) / 1e6)
        if p[2].isdigit(): V.append(int(p[2]))
        if p[3].isdigit(): Cur.append(int(p[3]))
        usb0 += p[4] == "0"; ping0 += p[5] == "0"
    if P:
        print(f"== board: {n} samples; INA232 power W min {min(P):.2f} mean {statistics.mean(P):.2f} max {max(P):.2f}; Pluto missing from USB in {usb0} samples, ping failed in {ping0}")
