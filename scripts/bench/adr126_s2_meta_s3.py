#!/usr/bin/env python3
"""Minimal SigV4 S3 client (stdlib only) for the ADR-126 S2 metadata latency bench.

Reads MESH_S3_* from an env file given as argv[1]; never prints the keys.
Subcommands:  s3 <envfile> <bucket> [n]   raw List/Head timings on a bucket
              net <url> [n]                unauthenticated round trips to a URL
"""
import datetime, hashlib, hmac, http.client, os, statistics, sys, time, urllib.parse


def load_env(path):
    d = {}
    for line in open(path):
        if "=" in line:
            k, v = line.strip().split("=", 1)
            d[k] = v
    return d


class S3:
    def __init__(self, env):
        u = urllib.parse.urlparse(env["MESH_S3_ENDPOINT"])
        self.host, self.port = u.hostname, u.port or 80
        self.ak, self.sk, self.region = env["MESH_S3_ACCESS_KEY"], env["MESH_S3_SECRET_KEY"], env.get("MESH_S3_REGION", "us-east-1")
        self.conn = http.client.HTTPConnection(self.host, self.port)  # keep-alive, like a pooled client

    def _sign(self, method, path, query, payload_hash):
        now = datetime.datetime.now(datetime.timezone.utc)
        amz, day = now.strftime("%Y%m%dT%H%M%SZ"), now.strftime("%Y%m%d")
        hostport = f"{self.host}:{self.port}"
        hdrs = {"host": hostport, "x-amz-content-sha256": payload_hash, "x-amz-date": amz}
        signed = ";".join(sorted(hdrs))
        canon_q = "&".join(f"{urllib.parse.quote(k, safe='-_.~')}={urllib.parse.quote(v, safe='-_.~')}" for k, v in sorted(query.items()))
        canon = "\n".join([method, urllib.parse.quote(path, safe="/-_.~"), canon_q,
                           "".join(f"{k}:{hdrs[k]}\n" for k in sorted(hdrs)), signed, payload_hash])
        scope = f"{day}/{self.region}/s3/aws4_request"
        sts = "\n".join(["AWS4-HMAC-SHA256", amz, scope, hashlib.sha256(canon.encode()).hexdigest()])
        k = ("AWS4" + self.sk).encode()
        for m in (day, self.region, "s3", "aws4_request"):
            k = hmac.new(k, m.encode(), hashlib.sha256).digest()
        sig = hmac.new(k, sts.encode(), hashlib.sha256).hexdigest()
        hdrs["Authorization"] = f"AWS4-HMAC-SHA256 Credential={self.ak}/{scope}, SignedHeaders={signed}, Signature={sig}"
        return hdrs, canon_q

    def req(self, method, path, query=None, body=b""):
        query = query or {}
        hdrs, q = self._sign(method, path, query, hashlib.sha256(body).hexdigest())
        t = time.perf_counter()
        self.conn.request(method, path + ("?" + q if q else ""), body=body or None, headers=hdrs)
        r = self.conn.getresponse()
        data = r.read()
        return r.status, data, (time.perf_counter() - t) * 1000

    def list_all(self, bucket, page=1000):
        """Full ListObjectsV2 walk. Returns (keys, [per-page ms])."""
        keys, times, tok = [], [], None
        while True:
            q = {"list-type": "2", "max-keys": str(page)}
            if tok:
                q["continuation-token"] = tok
            st, data, ms = self.req("GET", f"/{bucket}", q)
            assert st == 200, (st, data[:200])
            times.append(ms)
            s = data.decode()
            keys += [x.split("</Key>")[0] for x in s.split("<Key>")[1:]]
            if "<IsTruncated>true</IsTruncated>" in s:
                tok = s.split("<NextContinuationToken>")[1].split("</NextContinuationToken>")[0]
            else:
                return keys, times


def pct(xs, p):
    xs = sorted(xs)
    return xs[min(len(xs) - 1, int(round(p / 100 * (len(xs) - 1))))]


def summ(xs):
    return f"p50 {statistics.median(xs):.2f} ms  p99 {pct(xs, 99):.2f} ms  max {max(xs):.2f} ms  (n={len(xs)})"


def main():
    cmd = sys.argv[1]
    if cmd == "s3":
        env, bucket = load_env(sys.argv[2]), sys.argv[3]
        n = int(sys.argv[4]) if len(sys.argv) > 4 else 50
        c = S3(env)
        keys, _ = c.list_all(bucket)
        print(f"bucket {bucket}: {len(keys)} keys")
        walls, pages = [], []
        for _ in range(n // 5 or 1):
            t = time.perf_counter(); _, ts = c.list_all(bucket); walls.append((time.perf_counter() - t) * 1000); pages += ts
        print("List full walk (pages of 1000):", summ(walls), f"pages/walk={len(ts)}")
        print("List single page of 1000     :", summ(pages))
        small = [k for k in keys if k.startswith("worktrees/")][:1] or keys[:1]
        for label, ks in (("HeadObject registry-ish/first key", small),
                          ("HeadObject 20 spread objects", [keys[i * len(keys) // 20] for i in range(20)])):
            ts = []
            for i in range(n):
                k = ks[i % len(ks)]
                st, _, ms = c.req("HEAD", f"/{bucket}/{k}"); assert st == 200, (st, k); ts.append(ms)
            print(f"{label:34s}:", summ(ts))
        for label, ks in (("GetObject small (<=20KB)", [k for k in keys][:1]),):
            ts = []
            for i in range(n):
                st, d, ms = c.req("GET", f"/{bucket}/{ks[0]}"); assert st == 200; ts.append(ms)
            print(f"{label:34s}:", summ(ts), f"size={len(d)}")
    elif cmd == "net":
        u = urllib.parse.urlparse(sys.argv[2]); n = int(sys.argv[3]) if len(sys.argv) > 3 else 20
        ts_new, ts_keep = [], []
        for _ in range(n):
            t = time.perf_counter(); cn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=10)
            try:
                cn.request("GET", "/"); r = cn.getresponse(); r.read(); st = r.status
            except Exception as e:
                st = type(e).__name__
            ts_new.append((time.perf_counter() - t) * 1000); cn.close()
        cn = http.client.HTTPConnection(u.hostname, u.port or 80, timeout=10)
        for _ in range(n):
            t = time.perf_counter()
            try:
                cn.request("GET", "/"); r = cn.getresponse(); r.read(); st2 = r.status
            except Exception as e:
                st2 = type(e).__name__
            ts_keep.append((time.perf_counter() - t) * 1000)
        print(f"{u.geturl()} new connection each (TCP connect + request): {summ(ts_new)} last status {st}")
        print(f"{u.geturl()} keep-alive request only                    : {summ(ts_keep)} last status {st2}")


if __name__ == "__main__":
    main()
