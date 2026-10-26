import json, statistics, bisect, sys
R = sys.argv[1]; start = int(sys.argv[2])
env = []
for l in open(f"{R}/env.csv"):
    p = l.strip().split(",")
    if len(p) >= 10 and p[0].isdigit():
        env.append((int(p[0]), int(p[9] or 0)))
ts = [e[0] for e in env]
def cam(t):
    i = bisect.bisect_left(ts, t)
    near = [env[j][1] for j in (i - 1, i, i + 1) if 0 <= j < len(env) and abs(env[j][0] - t) <= 12]
    return max(near) if near else 0
rows = [json.loads(l) for l in open(f"{R}/phase2-cog.out") if l.startswith("{")]
rows = [r for r in rows if "samples" in r]
prev = None; agg = {}
for r in rows:
    if prev and r["status"] == "streaming" and prev["status"] == "streaming":
        c = r["band"]["centre_hz"]; k = (c, cam(start + r["uptime_s"]) > 0)
        a = agg.setdefault(k, [0, 0, 0.0])
        a[0] += r["samples"]["delivered"] - prev["samples"]["delivered"]
        a[1] += r["samples"]["lost"] - prev["samples"]["lost"]
        a[2] += r["uptime_s"] - prev["uptime_s"]
    prev = r
for (c, cm), (d, l, s) in sorted(agg.items()):
    print(f"{c/1e6:8.2f} MHz camera {'ON ' if cm else 'off'} {s:8.0f} s delivered {d:>16,} lost {l:>14,} {100*l/max(d+l,1):6.2f} %")
tp = [r["hw"]["temp_phy_c"] for r in rows if r["hw"].get("temp_phy_c") is not None]
good = [t for t in tp if -40 < t < 125]; bad = [t for t in tp if not -40 < t < 125]
print("AD9364 temp valid min/mean/max", min(good), round(statistics.mean(good), 1), max(good), "implausible readings:", len(bad), sorted(set(bad)))
tz = [r["hw"]["temp_zynq_c"] for r in rows if r["hw"].get("temp_zynq_c") is not None]
print("Zynq min/mean/max", round(min(tz), 1), round(statistics.mean(tz), 1), round(max(tz), 1))
