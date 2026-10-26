#!/usr/bin/env python3
"""Make a PLUTOIQ1 fixture from the running owner cog's /raw: mkfix.py OUT [port]"""
import struct, sys, urllib.request
out = sys.argv[1]
port = sys.argv[2] if len(sys.argv) > 2 else "8063"
st = __import__("json").load(urllib.request.urlopen(f"http://127.0.0.1:{port}/status"))
r = urllib.request.urlopen(f"http://127.0.0.1:{port}/raw?samples=65536")
data = r.read()
centre = int(r.headers["X-Centre-Hz"]); rate = int(r.headers["X-Rate-Sps"])
b = st["band"]
assert b["centre_hz"] == centre and b["rate_sps"] == rate, (b, centre, rate)
assert len(data) == 65536 * 4, len(data)
hdr = b"PLUTOIQ1" + struct.pack("<QIIII", centre, rate, b["bw_hz"], 65536, 0)
open(out, "wb").write(hdr + data)
print(out, centre, rate, b["bw_hz"], len(data), "power_dbfs", st["power_dbfs"])
