#!/usr/bin/env python3
"""Compare two GUI colour-snapshot directories (see scripts/gui-colour-snapshot.sh).

usage: gui-colour-compare.py <base-dir> <tip-dir> [--verbose]

For every surface present in either directory it reports:
  * strict    - the full dump (positions, uvs, colours, text) is byte-identical
  * colours   - the colour view (every vertex rgba + text fallback colours, in
                order) is identical, ignoring positions/uvs/string contents
  * differing vertices, max per-channel difference, and the first locations.

Exit status 0 only when every surface is strict- or colour-identical and no
surface is missing from either side.
"""
import os
import re
import sys

VERT = re.compile(r"^(P\d+)\.v(\d+) pos=\(([^)]*)\) uv=\(([^)]*)\) rgba=#([0-9a-f]{8})$")
TEXT = re.compile(r'^T pos=\(([^)]*)\) fallback=#([0-9a-f]{8}) text=(.*)$')


def load(path):
    with open(path) as f:
        return f.read().splitlines()


def parse(lines):
    verts, texts, heads = [], [], []
    for ln in lines:
        m = VERT.match(ln)
        if m:
            verts.append((m.group(1), int(m.group(2)), m.group(3), m.group(5)))
            continue
        m = TEXT.match(ln)
        if m:
            texts.append((m.group(1), m.group(2), m.group(3)))
            continue
        heads.append(ln)
    return verts, texts, heads


def chans(h):
    return [int(h[i:i + 2], 16) for i in range(0, 8, 2)]


def compare(name, a_lines, b_lines, verbose):
    strict = a_lines == b_lines
    av, at, ah = parse(a_lines)
    bv, bt, bh = parse(b_lines)
    # Primitive headers carry vertex counts; compare them without clip noise.
    shape_same = [h.split(" clip=")[0] for h in ah] == [h.split(" clip=")[0] for h in bh]
    col_same = (
        len(av) == len(bv)
        and all(x[3] == y[3] for x, y in zip(av, bv))
        and len(at) == len(bt)
        and all(x[1] == y[1] for x, y in zip(at, bt))
        and shape_same
    )
    diffs = []
    if len(av) == len(bv):
        for x, y in zip(av, bv):
            if x[3] != y[3]:
                d = max(abs(p - q) for p, q in zip(chans(x[3]), chans(y[3])))
                diffs.append((x[0], x[1], x[2], x[3], y[3], d))
    tdiffs = []
    if len(at) == len(bt):
        for x, y in zip(at, bt):
            if x[1] != y[1]:
                tdiffs.append((x[0], x[2], x[1], y[1]))
    return {
        "name": name,
        "strict": strict,
        "colours": col_same,
        "verts": (len(av), len(bv)),
        "texts": (len(at), len(bt)),
        "vdiffs": diffs,
        "tdiffs": tdiffs,
        "shape_same": shape_same,
        "extra_lines": abs(len(a_lines) - len(b_lines)),
    }


def main():
    args = [a for a in sys.argv[1:] if not a.startswith("--")]
    verbose = "--verbose" in sys.argv
    if len(args) != 2:
        print(__doc__)
        return 2
    base, tip = args

    def names(d):
        return {f[:-4] for f in os.listdir(d) if f.endswith(".txt") and ".nondeterministic" not in f}

    nb, nt = names(base), names(tip)
    ok = True
    print(f"{'surface':34} {'verts base/tip':>16} {'strict':>7} {'colours':>8} {'diff verts':>10} {'max ch':>7}")
    for n in sorted(nb | nt):
        if n not in nb or n not in nt:
            print(f"{n:34} MISSING in {'base' if n not in nb else 'tip'}")
            ok = False
            continue
        r = compare(n, load(os.path.join(base, n + ".txt")), load(os.path.join(tip, n + ".txt")), verbose)
        mx = max([d[5] for d in r["vdiffs"]] or [0])
        print(
            f"{n:34} {r['verts'][0]:>7}/{r['verts'][1]:<8} {str(r['strict']):>7} "
            f"{str(r['colours']):>8} {len(r['vdiffs']):>10} {mx:>7}"
        )
        if not (r["strict"] or r["colours"]):
            ok = False
            for d in r["vdiffs"][: (50 if verbose else 5)]:
                print(f"    {d[0]}.v{d[1]} pos=({d[2]}) base=#{d[3]} tip=#{d[4]} maxch={d[5]}")
            for t in r["tdiffs"][:5]:
                print(f"    text at ({t[0]}) {t[1]} base=#{t[2]} tip=#{t[3]}")
            if not r["shape_same"] or r["verts"][0] != r["verts"][1]:
                print("    geometry differs (vertex counts / primitive layout)")
    print("RESULT:", "all surfaces colour-identical" if ok else "DIFFERENCES FOUND")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
