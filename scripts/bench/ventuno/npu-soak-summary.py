#!/usr/bin/env python3
"""Summarise an NPU soak: the consumers' JSON-lines logs plus the monitor CSV.

    npu-soak-summary.py --logs a.jsonl b.jsonl --monitor monitor.csv [--markdown]

Reads only. Prints the numbers the bench write-up needs: calls and failures per
consumer, latency percentiles (all calls, and those that did not load), loads,
the service's last counters (evictions, restarts), queue depth seen, memory,
power, temperature and the OOM counter across the run.
"""
import argparse, csv, json, statistics, sys
from datetime import datetime, timezone

def pct(v, p):
    if not v:
        return 0
    v = sorted(v)
    return v[round((len(v) - 1) * p)]

def load_jsonl(path):
    out = []
    for line in open(path):
        line = line.strip()
        if line.startswith("{"):
            try:
                out.append(json.loads(line))
            except ValueError:
                pass
    return out

def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--logs", nargs="+", required=True)
    ap.add_argument("--monitor")
    ap.add_argument("--from", dest="t_from", help="only rows at or after this UTC time (ISO, e.g. 2026-10-26T08:30:00Z)")
    ap.add_argument("--to", dest="t_to", help="only rows before this UTC time")
    a = ap.parse_args()
    def to_ms(iso):
        return int(datetime.strptime(iso, "%Y-%m-%dT%H:%M:%SZ").replace(tzinfo=timezone.utc).timestamp() * 1000) if iso else None
    lo, hi = to_ms(a.t_from), to_ms(a.t_to)
    def inside(ms):
        return (lo is None or ms >= lo) and (hi is None or ms < hi)
    res = {"consumers": {}}
    tele = []
    t_first, t_last = None, None
    for path in a.logs:
        rows = [r for r in load_jsonl(path) if inside(r.get("t_ms", 0))]
        calls = [r for r in rows if r.get("kind") == "call"]
        tele += [r for r in rows if r.get("kind") == "telemetry"]
        if not calls:
            continue
        ok = [r for r in calls if r.get("ok")]
        bad = [r for r in calls if not r.get("ok")]
        tot = [r["total_ms"] for r in ok]
        warm = [r["total_ms"] for r in ok if not r.get("loaded")]
        errs = {}
        for r in bad:
            errs[r.get("code", "?")] = errs.get(r.get("code", "?"), 0) + 1
        per_model = {}
        for r in ok:
            per_model.setdefault(r["model"], []).append(r["total_ms"])
        role = calls[0].get("role", path)
        res["consumers"][role] = {
            "calls": len(calls), "ok": len(ok), "failed": len(bad), "errors": errs,
            "loads": sum(1 for r in ok if r.get("loaded")),
            "total_ms": {"p50": pct(tot, .5), "p95": pct(tot, .95), "max": pct(tot, 1)},
            "total_ms_without_load": {"p50": pct(warm, .5), "p95": pct(warm, .95), "max": pct(warm, 1)},
            "queue_ms": {"p50": pct([r["queue_ms"] for r in ok if r.get("queue_ms") is not None], .5),
                         "p95": pct([r["queue_ms"] for r in ok if r.get("queue_ms") is not None], .95)},
            "per_model_p50_ms": {m: pct(v, .5) for m, v in per_model.items()},
        }
        ts = [r["t_ms"] for r in calls]
        t_first = min(ts + ([t_first] if t_first else []))
        t_last = max(ts + ([t_last] if t_last else []))
    if t_first:
        res["hours"] = round((t_last - t_first) / 3.6e6, 2)
    if tele:
        tele.sort(key=lambda r: r["t_ms"])
        qd = [r["queue_depth"] for r in tele if r.get("queue_depth") is not None]
        last = tele[-1]
        res["service"] = {
            "telemetry_lines": len(tele), "queue_depth_max": max(qd) if qd else None,
            "queue_depth_mean": round(statistics.mean(qd), 2) if qd else None,
            "queue_oldest_wait_ms_max": max([r["queue_oldest_wait_ms"] or 0 for r in tele]),
            "last": {k: last.get(k) for k in ("served", "failed", "rejected", "expired", "loads", "evictions", "restarts")},
            "resident_models_max": max([r["models_resident"] or 0 for r in tele]),
            "mem_resident_mib_max": max([r["mem_resident_mib"] or 0 for r in tele]),
        }
    if a.monitor:
        rows = [r for r in csv.DictReader(open(a.monitor))
                if inside(to_ms(r["ts"]) or 0)]
        def col(n, f=float):
            out = []
            for r in rows:
                try:
                    out.append(f(r[n]))
                except (ValueError, KeyError, TypeError):
                    pass
            return out
        av, pw, tm, oom, gr = col("mem_avail_mib"), col("power_uw"), col("temp_max_mc"), col("oom_kill", int), col("geniex_rss_mib")
        res["monitor"] = {
            "samples": len(rows),
            "mem_avail_mib": {"min": min(av) if av else None, "p5": pct(av, .05), "median": pct(av, .5)},
            "geniex_rss_mib_max": max(gr) if gr else None,
            "cog_rss_mib_max": max(col("cog_rss_mib")) if col("cog_rss_mib") else None,
            "power_w": {"mean": round(statistics.mean(pw) / 1e6, 2) if pw else None, "max": round(max(pw) / 1e6, 2) if pw else None,
                        "min": round(min(pw) / 1e6, 2) if pw else None},
            "temp_max_c": round(max(tm) / 1000, 1) if tm else None,
            "oom_kill_delta": (oom[-1] - oom[0]) if oom else None,
        }
    json.dump(res, sys.stdout, indent=2)
    print()

main()
