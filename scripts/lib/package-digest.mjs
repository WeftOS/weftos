/**
 * package-digest.mjs — JS twin of crates/weftos/src/init/digest.rs, the one
 * definition of a bundle digest (ADR-121 §3).
 *
 *   package = hex(sha256(lines)), lines = for each file of the package in the
 *             `weftos init` source (any depth; names starting "." and `evals`
 *             directories are left out, exactly as the init embed filter does),
 *             sorted by path relative to the package in byte order:
 *               "file <relative path> <hex sha256 of the bytes>\n"
 *   team    = hex(sha256(lines)), the same "file" lines over teams/<team>/,
 *             then "member <id> <package digest>\n" for each active member
 *             package, sorted by id (active: lead, members, shared_skills,
 *             skills with `active` not false; only top-level, non-inactive
 *             packages count, as in AgentSource::package_ids).
 *
 * The catalog stores a digest as "sha256:" + that hex. A symlink inside a
 * bundle is refused. Keep this in step with digest.rs.
 */
import { createHash } from "node:crypto";
import { readFileSync, readdirSync, lstatSync } from "node:fs";
import { join } from "node:path";

const sha = (data) => createHash("sha256").update(data).digest("hex");

function collect(dir, rel, out) {
  for (const name of readdirSync(dir)) {
    if (name.startsWith(".") || name === "evals") continue;
    const abs = join(dir, name);
    const relPath = rel ? `${rel}/${name}` : name;
    const st = lstatSync(abs);
    if (st.isSymbolicLink()) {
      throw new Error(`bundle contains a symlink (digest refuses it): ${relPath}`);
    }
    if (st.isDirectory()) collect(abs, relPath, out);
    else if (st.isFile()) out.push([relPath, abs]);
  }
}

function fileLines(dirAbs) {
  const files = [];
  collect(dirAbs, "", files);
  files.sort((a, b) => Buffer.compare(Buffer.from(a[0]), Buffer.from(b[0])));
  return files.map(([rel, abs]) => `file ${rel} ${sha(readFileSync(abs))}\n`).join("");
}

/** Hex digest of one package directory. */
export function packageDigestHex(pkgAbs) {
  return sha(fileLines(pkgAbs));
}

/** Hex digest of a team directory plus its active member packages
 *  (`memberHex`: Map of package id -> package hex digest). */
export function teamDigestHex(teamAbs, memberIds, memberHex) {
  let text = fileLines(teamAbs);
  for (const id of [...new Set(memberIds)].sort()) {
    if (memberHex.has(id)) text += `member ${id} ${memberHex.get(id)}\n`;
  }
  return sha(text);
}

export const packageDigest = (pkgAbs) => `sha256:${packageDigestHex(pkgAbs)}`;
export const DIGEST_RE = /^sha256:[0-9a-f]{64}$/;
