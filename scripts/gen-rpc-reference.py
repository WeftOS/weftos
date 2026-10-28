#!/usr/bin/env python3
"""Generate docs/reference/rpc.md from the daemon's dispatch tables.

Sources (all under crates/clawft-weave/src):
  * rpc_ext.rs      ROUTES table: method prefix or exact name, required capability, handler
  * daemon.rs       the `dispatch` match (core methods) and the streaming intercepts
  * capability.rs   required_capability(): the method -> capability table (default Read)
  * *_rpc*.rs       module dispatchers reached through `starts_with` arms

Deterministic: sorted, no timestamps, no line numbers (they would drift on any
unrelated edit). Methods are grouped by their first dotted segment.

Usage:
  scripts/gen-rpc-reference.py [--check]
    --check  exit 1 if docs/reference/rpc.md differs from what the code yields
"""
import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
SRC = ROOT / "crates/clawft-weave/src"
OUT = ROOT / "docs/reference/rpc.md"

METHOD = r'"([a-z_][a-z_0-9]*(?:\.[a-z_0-9-]+)+)"'
ARM = re.compile(r'^\s*(' + METHOD + r'(?:\s*\|\s*' + METHOD + r')*)(?:\s+if\s+[^=]+?)?\s*=>', re.M)
LIT = re.compile(METHOD)


def rel(p):
    return str(p.relative_to(ROOT))


def capability_table():
    """Exact method -> capability and prefix -> capability from capability.rs."""
    text = (SRC / "capability.rs").read_text()
    start = text.index("pub fn required_capability")
    body = text[start:]
    body = body[: body.index("\n}\n")]
    body = re.sub(r'//[^\n]*', '', body)
    exact, prefix = {}, {}
    for m in re.finditer(r'((?:"[^"]+"\s*\|?\s*)+)=>\s*Capability::(\w+)', body):
        for lit in re.findall(r'"([^"]+)"', m.group(1)):
            exact[lit] = m.group(2)
    for m in re.finditer(r'starts_with\("([^"]+)"\)[^=]*=>\s*Capability::(\w+)', body):
        prefix[m.group(1)] = m.group(2)
    return exact, prefix


def ext_routes():
    """(prefix, capability, handler, cfg) from the non-test ROUTES table."""
    text = (SRC / "rpc_ext.rs").read_text()
    a = text.index("#[cfg(not(test))]\nconst ROUTES")
    b = text.index("];", a)
    routes = []
    for m in re.finditer(
        r'((?:\s*#\[cfg\([^\n]*\)\]\n)*)\s*ExtRoute\s*\{\s*prefix:\s*"([^"]+)",\s*capability:\s*Capability::(\w+),\s*handler:\s*([\w:]+),',
        text[a:b],
    ):
        cfg = " ".join(re.findall(r'#\[cfg\((.*)\)\]', m.group(1)))
        handler = m.group(4).replace("crate::", "")
        routes.append((m.group(2), m.group(3), handler, cfg))
    return routes


def dispatch_methods():
    """Method -> sorted source files, from match arms in the daemon and *_rpc modules."""
    core = {}
    daemon = (SRC / "daemon.rs").read_text()
    d0 = daemon.index("async fn dispatch(\n")
    d1 = daemon.index("other => Response::error(format!(\"unknown method", d0)
    for m in ARM.finditer(daemon[d0:d1]):
        for lit in LIT.findall(m.group(1)):
            core.setdefault(lit, set()).add("daemon.rs")
    known = {k.split(".")[0] for k in core}
    for f in sorted(SRC.glob("*.rs")):
        n = f.name
        if n in ("daemon.rs", "capability.rs", "rpc_ext.rs") or "_tests" in n or n.endswith("_test.rs"):
            continue
        if not re.search(r'_rpc|rpc_|spatial|infer', n):
            continue
        text = f.read_text()
        # Stop at an inline test module so test fixtures are not listed.
        text = text.split("#[cfg(test)]")[0]
        for m in ARM.finditer(text):
            for lit in LIT.findall(m.group(1)):
                if lit.split(".")[0] in known:
                    core.setdefault(lit, set()).add(n)
    return core, daemon


def starts_with_prefixes(daemon):
    d0 = daemon.index("async fn dispatch(\n")
    out = {}
    for m in re.finditer(r'm if m\.starts_with\("([^"]+)"\)\s*=>\s*(?:crate::)?(\w+)::', daemon[d0:]):
        out[m.group(1)] = m.group(2) + ".rs"
    return out


def streaming():
    text = (SRC / "daemon.rs").read_text()
    return sorted(set(re.findall(r'req\.method == "([^"]+)"', text)))


def cap_for(method, exact, prefix):
    if method in exact:
        return exact[method]
    best = None
    for p, c in prefix.items():
        if method.startswith(p) and (best is None or len(p) > len(best[0])):
            best = (p, c)
    return best[1] if best else "Read"


def render():
    exact, cap_prefix = capability_table()
    routes = ext_routes()
    core, daemon = dispatch_methods()
    prefixes = starts_with_prefixes(daemon)
    streams = streaming()

    # Every method name, with capability and handler location.
    rows = {}
    for meth, files in core.items():
        rows[meth] = {"cap": cap_for(meth, exact, cap_prefix), "where": sorted(files)}
    route_prefixes = [r[0] for r in routes]
    for meth in exact:
        if any(meth == rp or (rp.endswith(".") and meth.startswith(rp)) for rp in route_prefixes):
            continue
        rows.setdefault(meth, {"cap": exact[meth], "where": ["(capability table only)"]})
    for meth in streams:
        rows[meth] = {"cap": cap_for(meth, exact, cap_prefix), "where": ["daemon.rs (streaming intercept)"]}

    groups = {}
    for meth in rows:
        groups.setdefault(meth.split(".")[0], []).append(meth)
    rgroups = {}
    for prefix, cap, handler, cfg in routes:
        rgroups.setdefault(prefix.split(".")[0], []).append((prefix, cap, handler, cfg))

    o = []
    w = o.append
    w("# Daemon RPC reference\n")
    w("Generated by `scripts/gen-rpc-reference.py` from the dispatch tables in `crates/clawft-weave/src`. Do not edit by hand; run `scripts/build.sh gen-rpc-reference`.\n")
    w("The daemon speaks JSON-RPC over its unix socket (and the authenticated TCP relay). Each request names a method; the daemon resolves the capability the method needs, checks the caller's scopes, then dispatches. Capabilities, lowest to highest: `Read`, `Chat`, `Write`, `Admin`. A method missing from the capability table defaults to `Read`.\n")
    w("There are three registration paths, all listed below:\n")
    w("1. **Core methods**: arms of the big `match` in `daemon.rs` `dispatch`, plus module dispatchers it forwards prefixes to.")
    w("2. **Extension routes**: the `ROUTES` table in `rpc_ext.rs`. Each route is a method prefix (ends in `.`) or an exact name, with an explicit capability. Routes are consulted first, in order.")
    w("3. **Streaming intercepts**: methods that take over the connection and are matched before routes.\n")
    w("Method sets here are the registration points; per-method parameters and results live in each handler. Where a handler module is named, its source file is under `crates/clawft-weave/src/`.\n")

    w("## Groups\n")
    allp = sorted(set(groups) | set(rgroups))
    for p in allp:
        n = len(groups.get(p, [])) + len(rgroups.get(p, []))
        w(f"- [`{p}.*`](#{p}) ({n})")
    w("")

    for p in allp:
        w(f"## {p}\n")
        if p in groups:
            w("| Method | Capability | Handler |")
            w("|--------|------------|---------|")
            for meth in sorted(groups[p]):
                r = rows[meth]
                w(f"| `{meth}` | {r['cap']} | {', '.join('`' + x + '`' if not x.startswith('(') else x for x in r['where'])} |")
            w("")
        if p in rgroups:
            w("Extension routes (prefix match unless the name is exact):\n")
            w("| Route | Capability | Handler | Build condition |")
            w("|-------|------------|---------|-----------------|")
            for prefix, cap, handler, cfg in sorted(rgroups[p]):
                w(f"| `{prefix}` | {cap} | `{handler}` | {('`' + cfg + '`') if cfg else ''} |")
            w("")

    w("## Forwarded prefixes\n")
    w("Core `dispatch` forwards these prefixes to a module dispatcher:\n")
    w("| Prefix | Module |")
    w("|--------|--------|")
    for p in sorted(prefixes):
        w(f"| `{p}*` | `{prefixes[p]}` |")
    w("")
    if cap_prefix:
        w("Prefix rules in the capability table:\n")
        w("| Prefix | Capability |")
        w("|--------|------------|")
        for p in sorted(cap_prefix):
            w(f"| `{p}*` | {cap_prefix[p]} |")
        w("")
    return "\n".join(o)


def main():
    text = render()
    if "--check" in sys.argv:
        cur = OUT.read_text() if OUT.exists() else ""
        if cur != text:
            import difflib
            d = difflib.unified_diff(cur.splitlines(), text.splitlines(), str(OUT), "generated", lineterm="", n=1)
            sys.stderr.write("DRIFT: docs/reference/rpc.md differs from the dispatch tables\n")
            sys.stderr.write("\n".join(list(d)[:60]) + "\n")
            sys.exit(1)
        print("rpc reference up to date")
        return
    OUT.write_text(text)
    print(f"wrote {rel(OUT)}")


if __name__ == "__main__":
    main()
