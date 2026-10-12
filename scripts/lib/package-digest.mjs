/**
 * package-digest.mjs — the one definition of an agent package digest
 * (ADR-121 §3).
 *
 *   digest = "sha256:" + hex(sha256(manifest))
 *   manifest = for each regular file under the package directory, sorted by
 *              its POSIX path relative to the package root in byte order:
 *                <relative path> NUL <hex sha256 of the file bytes> LF
 *
 * Every file in the bundle counts (AGENT.md, SKILL.md, skills/, evals/,
 * references/, weftos-package.yaml, ...) except `.DS_Store`. No timestamps,
 * modes or ownership. A symlink inside a package is refused: a digest must
 * not depend on what a link happens to point at.
 *
 * The Rust side (a node refusing a tampered installation, ADR-121 §4) must
 * implement exactly this; `scripts/agents-validate.mjs` pins the vectors.
 */
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, lstatSync } from "node:fs";
import { join } from "node:path";

const IGNORED = new Set([".DS_Store"]);

function collect(dir, rel, out) {
  for (const name of readdirSync(dir)) {
    if (IGNORED.has(name)) continue;
    const abs = join(dir, name);
    const relPath = rel ? `${rel}/${name}` : name;
    const st = lstatSync(abs);
    if (st.isSymbolicLink()) {
      throw new Error(`package contains a symlink (digest refuses it): ${relPath}`);
    }
    if (st.isDirectory()) collect(abs, relPath, out);
    else if (st.isFile()) out.push([relPath, abs]);
  }
}

export function packageDigest(pkgAbs) {
  const files = [];
  collect(pkgAbs, "", files);
  files.sort((a, b) => Buffer.compare(Buffer.from(a[0]), Buffer.from(b[0])));
  const h = createHash("sha256");
  for (const [rel, abs] of files) {
    const fileHash = createHash("sha256").update(readFileSync(abs)).digest("hex");
    h.update(`${rel}\0${fileHash}\n`);
  }
  return `sha256:${h.digest("hex")}`;
}

export const DIGEST_RE = /^sha256:[0-9a-f]{64}$/;
