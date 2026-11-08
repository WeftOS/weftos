#!/usr/bin/env python3
"""ADR-126 S2 metadata latency bench driver (stdlib only).

Needs: a running daemon on RustFS (env HOME / WEFTOS_RUNTIME_DIR / WEFTOS_STORE_RUSTFS_ENV set by the
caller, see docs/research/adr126-s2-metadata-latency-2026-11-08.md), a prebuilt `weaver`, and trees made by
adr126_s2_meta_gen.py. Every measurement is a `weaver store worktree <verb>` process, so the numbers
include CLI start + RPC to the daemon + the daemon's S3 work; `buckets` (no S3 I/O) gives that floor.

  adr126_s2_meta_bench.py <weaver> <envfile> <scratch> <step> [args]
Steps: create <tree> <slug> | publish <tree> <slug> | republish <tree> <slug> | floor | follow <slug>
       fanout <slug> <n> | search | syncdocs <slug> | fetch <slug> | leftovers | overlaps <a> <b>
Results are appended as JSON lines to <scratch>/logs/results.jsonl.
"""
import json, os, statistics, subprocess, sys, tempfile, time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import adr126_s2_meta_s3 as s3mod  # noqa: E402

DEST = os.environ.get("BENCH_DEST_ROOT", os.getcwd())
PROJECT = "01JBENCHS2META000000000A1A"


def load():
    return os.getloadavg()[0]


def run(weaver, verb, args, scratch):
    t = time.perf_counter(); l0 = load()
    p = subprocess.run([weaver, "store", "worktree", verb, "--project", PROJECT, *args, "--json"], capture_output=True, text=True)
    dt = time.perf_counter() - t
    if p.returncode:
        raise SystemExit(f"{verb} failed: {p.stderr[-400:]}")
    try:
        out = json.loads(p.stdout)
    except Exception:
        out = {"raw": p.stdout[-300:]}
    return dt, (l0 + load()) / 2, out


def record(scratch, row):
    with open(os.path.join(scratch, "logs", "results.jsonl"), "a") as f:
        f.write(json.dumps(row) + "\n")
    print(json.dumps(row))


def repeat(weaver, scratch, name, verb, args, n=3, prep=None):
    ts, loads, out = [], [], None
    for i in range(n):
        if prep:
            prep(i)
        dt, ld, out = run(weaver, verb, args(i) if callable(args) else args, scratch)
        ts.append(dt); loads.append(ld)
    record(scratch, {"step": name, "runs_s": [round(x, 3) for x in ts], "median_s": round(statistics.median(ts), 3),
                     "worst_s": round(max(ts), 3), "loadavg_per_run": [round(x, 2) for x in loads], "out": out})


def git(tree, *a, z=False):
    return subprocess.run(["git", "-C", tree, *a], capture_output=True, check=True).stdout


def inputs(tree, tmp):
    """Same inputs scripts/worktree-store.sh share builds."""
    base = git(tree, "merge-base", "master", "HEAD").decode().strip()
    head = git(tree, "rev-parse", "HEAD").decode().strip()
    branch = git(tree, "symbolic-ref", "--short", "HEAD").decode().strip()
    open(f"{tmp}/paths.z", "wb").write(git(tree, "ls-files", "-co", "--exclude-standard", "-z"))
    unt = git(tree, "ls-files", "-o", "--exclude-standard", "-z")
    open(f"{tmp}/uncommitted.z", "wb").write(git(tree, "diff", "--name-only", "-z", "HEAD") + unt)
    ch = git(tree, "diff", "--name-status", "-z", "--no-renames", base)
    for f in unt.split(b"\0"):
        if f:
            ch += b"A\0" + f + b"\0"
    open(f"{tmp}/changes.z", "wb").write(ch)
    bundle = []
    if base != head:
        subprocess.run(["git", "-C", tree, "bundle", "create", f"{tmp}/branch.bundle", f"refs/heads/{branch}", f"^{base}"], capture_output=True, check=True)
        bundle = ["--bundle", f"{tmp}/branch.bundle"]
    return ["--root", tree, "--branch", branch, "--base", base, "--head", head, "--paths-z", f"{tmp}/paths.z",
            "--uncommitted-z", f"{tmp}/uncommitted.z", "--changes-z", f"{tmp}/changes.z", *bundle], base, branch


def main():
    weaver, envfile, scratch, step, *a = sys.argv[1:]
    env = s3mod.load_env(envfile)
    if step == "floor":
        repeat(weaver, scratch, "floor_buckets_verb(no S3 I/O)", "buckets", ["--slug", "x"], n=10)
    elif step == "create":
        tree, slug = a
        _, base, branch = inputs(os.path.join(scratch, "trees", tree), tempfile.mkdtemp(dir=scratch))
        dt, ld, out = run(weaver, "create", ["--slug", slug, "--branch", branch, "--base", base], scratch)
        record(scratch, {"step": f"create {slug}", "runs_s": [round(dt, 3)], "loadavg_per_run": [round(ld, 2)]})
    elif step in ("publish", "republish"):
        tree, slug = a
        tmp = tempfile.mkdtemp(dir=scratch)
        args, _, _ = inputs(os.path.join(scratch, "trees", tree), tmp)
        n = 1 if step == "publish" else 3
        repeat(weaver, scratch, f"{step} {tree} as {slug}", "publish", ["--slug", slug, *args], n=n)
    elif step == "follow":
        slug = a[0]; sp = os.path.join(scratch, "state", f"follow-{len(os.listdir(os.path.join(scratch,'state')))}.json")
        repeat(weaver, scratch, "follow cold state (all entries New)", "follow", lambda i: ["--state", f"{sp}.cold{i}"])
        repeat(weaver, scratch, "follow warm state (no changes)", "follow", ["--state", sp + ".warm"],
               prep=lambda i: (run(weaver, "follow", ["--state", sp + ".warm"], scratch) if i == 0 else None))
    elif step == "fanout":
        # Make the registry hold n entries by copying the real entry under other slugs (poll only reads the
        # registry; entries still point at the real worktree bucket, so search/overlaps read its real manifest).
        slug, n = a[0], int(a[1])
        c = s3mod.S3(env); pb = f"p-{PROJECT.lower()}"
        st, d, _ = c.req("GET", f"/{pb}/worktrees/{slug}.json"); assert st == 200, st
        e = json.loads(d)
        keys, _ = c.list_all(pb)
        have = [k for k in keys if k.startswith("worktrees/")]
        for i in range(len(have), n):
            e2 = dict(e, slug=f"fill-{i:02d}")
            assert c.req("PUT", f"/{pb}/worktrees/fill-{i:02d}.json", body=json.dumps(e2).encode())[0] == 200
        keys, _ = c.list_all(pb)
        record(scratch, {"step": f"registry now {len([k for k in keys if k.startswith('worktrees/')])} entries", "entry_bytes": len(d)})
    elif step == "search":
        repeat(weaver, scratch, "search 'note'", "search", ["--query", "note"])
    elif step == "syncdocs":
        slug = a[0]
        def prep(i):
            subprocess.run(["rm", "-rf", f"{DEST}/cache{i}"])
        repeat(weaver, scratch, f"sync-docs {slug} (fresh cache)", "sync-docs", lambda i: ["--slug", slug, "--cache", f"{DEST}/cache{i}"], prep=prep)
    elif step == "fetch":
        slug = a[0]
        def prep(i):
            subprocess.run(["rm", "-rf", f"{DEST}/fetch{i}"])
        repeat(weaver, scratch, f"fetch {slug} whole tree (fresh dest)", "fetch", lambda i: ["--slug", slug, "--dest", f"{DEST}/fetch{i}", "--bundle-out", f"{DEST}/fetch{i}.bundle"], prep=prep)
    elif step == "leftovers":
        repeat(weaver, scratch, "leftovers", "leftovers", [])
    elif step == "overlaps":
        repeat(weaver, scratch, f"overlaps {a[0]} {a[1]}", "overlaps", ["--a", a[0], "--b", a[1]])


if __name__ == "__main__":
    main()
