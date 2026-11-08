#!/usr/bin/env python3
"""Deterministic synthetic git tree for the ADR-126 S2 metadata latency bench.

Usage: adr126_s2_meta_gen.py <outdir> <profile>   profile = sansone | weftos | tiny
No real client files are read or copied; every byte comes from a seeded PRNG.
Profile shapes (tracked / untracked-not-ignored / .md files):
  sansone  9500 / 800 / 2400   (md titles, sizes 1-20 KB, ~300 files 50-200 KB, one 450 KB package-lock.json)
  weftos   6000 / 150 /  900
  tiny       40 /   5 /   10
The repo has branch master plus branch feat/bench-work with 3 unmerged commits
(edits, adds, deletes), a few uncommitted edits, and the untracked files.
"""
import os, random, subprocess, sys

PROFILES = {"sansone": (9500, 800, 2400), "weftos": (6000, 150, 900), "tiny": (40, 5, 10)}
WORDS = ("alpha beta gamma delta route handler session token schema query client server ticket order invoice "
         "shipment customer render layout config deploy cache index record update delete create status audit "
         "report policy review draft final import export worker queue retry limit window buffer stream event").split()
CODE_DIRS = ["src/app", "src/components", "src/lib", "src/server", "src/hooks", "src/utils", "tests", "scripts", "public", "config", "prisma"]
DOC_DIRS = ["docs", "docs/adr", "docs/guides", "docs/research", "planning", ".planning/phases", ".planning/notes", "docs/runbooks"]
EXTS = [".ts", ".tsx", ".js", ".json", ".css", ".svg", ".sql", ".yml"]


def block(rng, n):
    return " ".join(rng.choice(WORDS) for _ in range(n // 6 + 1))[:n]


def size(rng):
    r = rng.random()
    if r < 0.03:
        return rng.randint(50_000, 200_000)
    return int(1000 * (20 ** rng.random()))  # log-uniform 1..20 KB


def write(path, data):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        f.write(data)


def main():
    out, prof = sys.argv[1], sys.argv[2]
    tracked, untracked, md = PROFILES[prof]
    rng = random.Random(f"adr126-s2-meta-{prof}")
    pool = block(rng, 400_000)
    def body(i, kind, n):
        off = rng.randrange(0, len(pool) - n) if n < len(pool) else 0
        head = f"# Note {i}: {block(rng, 40).title()}\n\n" if kind == "md" else f"// file {i}\n"
        return head + pool[off:off + n] + "\n"
    files = []  # (relpath, kind)
    total = tracked + untracked
    md_tracked = md * tracked // total
    for i in range(total):
        is_md = i < md
        d = rng.choice(DOC_DIRS if is_md else CODE_DIRS)
        name = f"f{i:05d}" + (".md" if is_md else rng.choice(EXTS))
        files.append((f"{d}/{rng.choice(WORDS)}/{name}" if rng.random() < .5 else f"{d}/{name}", "md" if is_md else "code"))
    rng.shuffle(files)
    md_files = [f for f in files if f[1] == "md"]
    tracked_files = [f for f in files if f[1] != "md"][: tracked - md_tracked] + md_files[:md_tracked]
    untracked_files = [f for f in files if f not in set(tracked_files)]
    os.makedirs(out, exist_ok=True)
    for i, (p, k) in enumerate(tracked_files + untracked_files):
        write(os.path.join(out, p), body(i, k, size(rng)))
    if prof != "tiny":
        write(os.path.join(out, "package-lock.json"), '{"lockfileVersion":3,"packages":{' + ",".join(f'"node_modules/{rng.choice(WORDS)}{j}":{{"version":"1.{j}.0","resolved":"https://registry.example/{j}.tgz","integrity":"sha512-{block(rng, 64).replace(" ", "")}"}}' for j in range(2400))[:450_000] + "}}\n")
        tracked_files.append(("package-lock.json", "code"))
    write(os.path.join(out, ".gitignore"), "node_modules/\n.next/\n*.log\n")
    write(os.path.join(out, "node_modules/ignored/x.js"), "// ignored\n")
    g = lambda *a: subprocess.run(["git", "-C", out, "-c", "user.name=bench", "-c", "user.email=bench@example.invalid", "-c", "commit.gpgsign=false", *a], check=True, capture_output=True, text=True)
    g("init", "-q", "-b", "master")
    with open(os.path.join(out, ".git/info/exclude"), "a"): pass
    paths = [".gitignore"] + [p for p, _ in tracked_files]
    subprocess.run(["git", "-C", out, "add", "--pathspec-from-file=-"], input="\n".join(paths), text=True, check=True)
    g("commit", "-q", "-m", "initial")
    g("checkout", "-q", "-b", "feat/bench-work")
    edit = rng.sample(tracked_files, min(40, len(tracked_files)))
    for p, k in edit:
        with open(os.path.join(out, p), "a") as f: f.write("\n// edit 1\n")
    g("commit", "-q", "-am", "edit 1")
    for j in range(20):
        write(os.path.join(out, f"src/new/added{j}.ts"), f"export const v{j} = {j};\n")
    g("add", "src/new"); g("commit", "-q", "-m", "add 20")
    for p, k in tracked_files[-6:-1]:
        os.remove(os.path.join(out, p))
    g("commit", "-q", "-am", "delete 5")
    for p, k in edit[:5]:
        if os.path.exists(os.path.join(out, p)):
            with open(os.path.join(out, p), "a") as f: f.write("\n// uncommitted\n")
    n = lambda *a: len(g(*a).stdout.split("\0")) - 1
    print(f"{prof}: tracked={n('ls-files','-z')} untracked={n('ls-files','-o','--exclude-standard','-z')} "
          f"md={sum(1 for l in g('ls-files','-co','--exclude-standard').stdout.splitlines() if l.endswith('.md'))} "
          f"commits_ahead={g('rev-list','--count','master..HEAD').stdout.strip()}")


if __name__ == "__main__":
    main()
