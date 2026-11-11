#!/usr/bin/env python3
"""Generate docs/cogs/inventory.md: every cog this repository knows about (WO-159).

Run from anywhere:   python3 -I scripts/cogs/inventory.py [--out PATH] [--check] [--root DIR ...]

Sources, all read-only and all inside the repository unless --root adds a directory:
  - scripts/cogs/expectations.json     the id list and conformance group (ADR-100 catalog)
  - scripts/cogs/results/*/results.json  per-run binary sha256, arch and runtime
  - cog.toml / *.cog.toml              id, version, binary, category
  - cogpkg.json                        signed package manifests (id, version, binary keys, signers)
  - docs/cogs/evidence/*.json          placement decisions: which node a cog was placed on

It does not verify signatures, contact a node or read ~/.weftos. "Signature" says what a manifest
carries, not that it verified. A cell is `-` when the repository holds no such fact.
--check exits 1 when the committed file is not what this script would write.
"""

import argparse
import glob
import json
import os
import re
import sys

SKIP_DIRS = {".git", "target", "node_modules", ".planning", "graphify-out"}
LICENCE = "MIT (upstream cogs repo, ADR-100); Cognitum-provenance binaries are licence-gated (ADR-105/106)"


def repo_root():
    here = os.path.dirname(os.path.abspath(__file__))
    return os.path.abspath(os.path.join(here, "..", ".."))


def rel(root, p):
    return os.path.relpath(p, root).replace(os.sep, "/")


def walk(base, names_or_suffix):
    """Files under `base` whose name equals or ends with one of the given strings."""
    out = []
    for d, dirs, files in os.walk(base):
        dirs[:] = sorted(x for x in dirs if x not in SKIP_DIRS)
        for f in sorted(files):
            if any(f == n or f.endswith(n) for n in names_or_suffix):
                out.append(os.path.join(d, f))
    return out


def load_json(path):
    try:
        with open(path) as fh:
            return json.load(fh)
    except (OSError, ValueError):
        return None


def parse_cog_toml(path):
    """The `[cog]` table only; a line parser, since the manifest subset needed is flat key = value."""
    cog, in_cog = {}, False
    try:
        lines = open(path).read().splitlines()
    except OSError:
        return cog
    for line in lines:
        s = line.strip()
        if s.startswith("["):
            in_cog = s == "[cog]"
            continue
        m = re.match(r'^([A-Za-z_]+)\s*=\s*"([^"]*)"\s*(#.*)?$', s)
        if in_cog and m:
            cog[m.group(1)] = m.group(2)
    return cog


def short_key(key_id, pub):
    return key_id or (pub[:12] if pub else "?")


def collect(root, extra_roots):
    cogs = {}

    def entry(cid):
        return cogs.setdefault(cid, {
            "versions": set(), "sources": set(), "keys": set(), "targets": set(), "sha": {},
            "signers": set(), "unsigned_pkg": False, "nodes": set(), "group": None, "needs": [],
        })

    exp_path = os.path.join(root, "scripts/cogs/expectations.json")
    exp = load_json(exp_path) or {}
    for cid, e in sorted((exp.get("cogs") or {}).items()):
        c = entry(cid)
        c["sources"].add(rel(root, exp_path))
        c["group"] = e.get("group")
        c["needs"] = e.get("needs") or []
        if exp.get("arch"):
            c["keys"].add(exp["arch"])

    for path in sorted(glob.glob(os.path.join(root, "scripts/cogs/results/*/results.json"))):
        data = load_json(path) or {}
        run = os.path.basename(os.path.dirname(path))
        recs = data.get("results") or []
        for r in recs if isinstance(recs, list) else []:
            cid = r.get("id")
            if not cid or r.get("status") != "ran":
                continue
            c = entry(cid)
            arch = {"arm64": "aarch64"}.get(r.get("host_machine"), r.get("host_machine"))
            if arch:
                c["keys"].add(arch)
                c["targets"].add(run)
            if r.get("sha256"):
                c["sha"][r["sha256"]] = arch or "?"

    for base in [root] + extra_roots:
        for path in walk(base, ["cog.toml", ".cog.toml"]):
            t = parse_cog_toml(path)
            if not t.get("id"):
                continue
            c = entry(t["id"])
            c["sources"].add(rel(root, path) if base == root else path)
            if t.get("version"):
                c["versions"].add(t["version"])
            if t.get("binary"):
                c["binary"] = t["binary"]
        for path in walk(base, ["cogpkg.json"]):
            env = load_json(path) or {}
            body = env.get("body") or {}
            if env.get("kind") != "cog" or not body.get("id"):
                continue
            c = entry(body["id"])
            c["sources"].add(rel(root, path) if base == root else path)
            if body.get("version"):
                c["versions"].add(body["version"])
            c["keys"].update((body.get("binaries") or {}).keys())
            sigs = env.get("signatures") or []
            if sigs:
                c["signers"].update(short_key(s.get("key_id"), s.get("public_key")) for s in sigs)
            else:
                c["unsigned_pkg"] = True

    placed = re.compile(r"workload (\S+) \(kind cog\)(.*?)(?=workload \S+ \(kind cog\)|\Z)", re.S)
    node = re.compile(r"PLACED on (n-[0-9a-f]+)")
    for path in sorted(glob.glob(os.path.join(root, "docs/cogs/evidence/*.json"))):
        text = json.dumps(load_json(path) or {})
        text = text.replace("\\n", "\n")
        for m in placed.finditer(text):
            n = node.search(m.group(2))
            if n:
                c = entry(m.group(1))
                c["nodes"].add(n.group(1))
                c["sources"].add(rel(root, path))
    return cogs


def cell(items, sep=", "):
    items = sorted(i for i in items if i)
    return sep.join(items) if items else "-"


def render(cogs, root):
    n_ver = sum(1 for c in cogs.values() if c["versions"])
    n_pkg = sum(1 for c in cogs.values() if c["signers"] or c["unsigned_pkg"])
    n_run = sum(1 for c in cogs.values() if c["sha"])
    n_node = sum(1 for c in cogs.values() if c["nodes"])
    out = [
        "# Cog inventory",
        "",
        "Generated by `python3 -I scripts/cogs/inventory.py`; do not edit by hand. Rerun it after the cog",
        "sources below change; `--check` fails when this file is stale.",
        "",
        "This repository does not hold the cog catalog. The cogs are built and released from the separate",
        "cogs repository (ADR-100, ADR-105), so this page lists what *this* tree records about each cog:",
        "the conformance expectations, the recorded runs, any `cog.toml` or `cogpkg.json`, and placement",
        "evidence. A cell is `-` where the tree holds no such fact; nothing is filled in from memory.",
        "",
        "## Counts",
        "",
        f"- cogs known: {len(cogs)}",
        f"- with a version recorded in the tree (`cog.toml` or `cogpkg.json`): {n_ver}",
        f"- with a recorded conformance run that executed the binary: {n_run}",
        f"- with a `cogpkg.json` in the tree: {n_pkg}",
        f"- with a placement recorded in evidence: {n_node}",
        "",
        "## Columns",
        "",
        "- **Version**: from `cog.toml` or `cogpkg.json` only. The conformance files do not record one.",
        "- **Source**: the files in this tree that name the cog (first one is the id source).",
        "- **Binary keys / targets**: keys are the architectures a binary or package is recorded for",
        "  (`aarch64`, `armv7`, `x86_64`, `wasm`, `aarch64-macos`). Targets are the conformance runs in",
        "  `scripts/cogs/results/` that executed the binary (runtime and host class are in the directory name).",
        "- **Signature**: `signed: <key ids>` when a `cogpkg.json` in the tree carries signatures, `unsigned`",
        "  when it carries none, `-` when no package is in the tree. Signatures are listed, not verified here.",
        "- **Declared on nodes**: node ids from `docs/cogs/evidence/` placement decisions. The tree has no",
        "  live node list; a running mesh answers this with `weaver fleet status`.",
        "- **Group**: the conformance group from `scripts/cogs/expectations.json`.",
        "",
        "## Licence",
        "",
        f"{LICENCE}. A per-cog licence is not recorded in this tree, so the column below repeats this one rule.",
        "",
        "## Cogs",
        "",
        "| Id | Version | Source | Binary keys / targets | Signature | Declared on nodes | Group | Licence |",
        "|---|---|---|---|---|---|---|---|",
    ]
    for cid in sorted(cogs):
        c = cogs[cid]
        # The id source (expectations) first, then the rest in path order.
        src = sorted(c["sources"], key=lambda p: (not p.endswith("expectations.json"), p))
        src_cell = f"`{src[0]}`" + (f" (+{len(src) - 1})" if len(src) > 1 else "") if src else "-"
        keys = cell(c["keys"])
        tg = cell(c["targets"], "; ")
        kt = keys if tg == "-" else f"{keys} / {tg}"
        if c["signers"]:
            sig = "signed: " + cell(c["signers"])
        elif c["unsigned_pkg"]:
            sig = "unsigned"
        else:
            sig = "-"
        group = c["group"] or "-"
        if c["needs"]:
            group += " (needs " + ", ".join(c["needs"]) + ")"
        out.append(
            f"| {cid} | {cell(c['versions'])} | {src_cell} | {kt} | {sig} | {cell(c['nodes'])} | {group} | MIT upstream |"
        )
    out.append("")
    return "\n".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--out", default=None)
    ap.add_argument("--root", action="append", default=[], help="extra directory to scan for cog.toml / cogpkg.json")
    ap.add_argument("--check", action="store_true")
    a = ap.parse_args()
    root = repo_root()
    out = a.out or os.path.join(root, "docs/cogs/inventory.md")
    text = render(collect(root, [os.path.abspath(r) for r in a.root]), root)
    if a.check:
        try:
            cur = open(out).read()
        except OSError:
            cur = None
        if cur != text:
            print(f"{out} is stale; rerun scripts/cogs/inventory.py", file=sys.stderr)
            return 1
        return 0
    with open(out, "w") as fh:
        fh.write(text)
    print(f"wrote {out}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
