/**
 * Tests for the ADR-121 package checks: digest definition, state, parent,
 * dependency resolution, cycles and inactive packages. Each case builds a
 * small fixture tree and runs agents-validate.mjs against it
 * (WEFTOS_AGENTS_DIR), so refusals are asserted by exit code and message.
 * Run: node --test scripts/agents-validate.test.mjs
 */
import { test } from "node:test";
import { strict as assert } from "node:assert";
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { spawnSync } from "node:child_process";
import { packageDigest } from "./lib/package-digest.mjs";

const SCRIPT = join(dirname(fileURLToPath(import.meta.url)), "agents-validate.mjs");

function put(root, rel, text) {
  mkdirSync(dirname(join(root, rel)), { recursive: true });
  writeFileSync(join(root, rel), text);
}

/** One valid package; `extra` is appended to its weftos-package.yaml. */
function pkg(root, id, extra = "") {
  put(root, `${id}/AGENT.md`, `---\nname: ${id}\nnickname: ${id}\nrole: test\ndescription: "A test agent. Use when: testing."\ntools: [Read]\nmodel_hint: any\ntrust_tier: internal\nkind: specialist\n---\nBody.\n`);
  put(root, `${id}/evals/scenarios.md`, "## 1. a\n\n## 2. b\n\n## 3. c\n");
  put(root, `${id}/weftos-package.yaml`, `name: ${id}
kind: specialist
trust_tier: internal
capabilities: []
required_project_context: []
requires: []
provenance:
  source_registry: test
  source_url: null
  upstream_license: none
  upstream_commit: null
  adopted_at: "2026-10-11"
  verdict: build
${extra}`);
}

function run(build) {
  const root = mkdtempSync(join(tmpdir(), "agents-validate-"));
  try {
    build(root);
    const r = spawnSync("node", [SCRIPT, "--quiet"], { env: { ...process.env, WEFTOS_AGENTS_DIR: root }, encoding: "utf8" });
    return { code: r.status, err: r.stderr };
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

test("digest definition is pinned: sorted paths, NUL, per-file sha256, LF", () => {
  const root = mkdtempSync(join(tmpdir(), "agents-digest-"));
  try {
    put(root, "skills/a/SKILL.md", "x");
    put(root, "AGENT.md", "hello\n");
    put(root, ".DS_Store", "ignored");
    assert.equal(packageDigest(root), "sha256:d2c6f89a09598dbac01978bf417ce25c5a16472e5fa7a5de31721f5881aca3e9");
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test("a clean tree with an inactive dependency-free package passes", () => {
  const r = run((root) => {
    pkg(root, "a");
    pkg(root, "b", "state: inactive\n");
  });
  assert.equal(r.code, 0, r.err);
});

test("an unknown state is refused", () => {
  const r = run((root) => pkg(root, "a", "state: retired\n"));
  assert.equal(r.code, 1);
  assert.match(r.err, /state "retired"/);
});

test("a parent that matches no package digest is refused; a malformed one too", () => {
  const nine = "sha256:" + "9".repeat(64);
  let r = run((root) => pkg(root, "a", `parent: ${nine}\n`));
  assert.equal(r.code, 1);
  assert.match(r.err, /does not match the digest of any package/);
  r = run((root) => pkg(root, "a", "parent: abc\n"));
  assert.equal(r.code, 1);
  assert.match(r.err, /parent must be null or sha256/);
});

test("a dependency entry naming two kinds, or none, is refused", () => {
  let r = run((root) => pkg(root, "a", "dependencies:\n  - agent: a\n    cog: x\n"));
  assert.equal(r.code, 1);
  assert.match(r.err, /exactly one of/);
  r = run((root) => pkg(root, "a", "dependencies:\n  - relationship: x\n"));
  assert.equal(r.code, 1);
});

test("a dependency on an unknown agent or skill is refused", () => {
  let r = run((root) => pkg(root, "a", "dependencies:\n  - agent: ghost\n"));
  assert.equal(r.code, 1);
  assert.match(r.err, /ghost/);
  r = run((root) => pkg(root, "a", "dependencies:\n  - skill: ghost\n"));
  assert.equal(r.code, 1);
  assert.match(r.err, /dependencies\.skill "ghost"/);
});

test("a dependency cycle is refused and the cycle is named", () => {
  const r = run((root) => {
    pkg(root, "a", "dependencies:\n  - agent: b\n");
    pkg(root, "b", "dependencies:\n  - agent: a\n");
  });
  assert.equal(r.code, 1);
  assert.match(r.err, /dependency cycle: a -> b -> a/);
});

test("an active package may not depend on an inactive one", () => {
  const r = run((root) => {
    pkg(root, "a", "dependencies:\n  - agent: b\n");
    pkg(root, "b", "state: inactive\n");
  });
  assert.equal(r.code, 1);
  assert.match(r.err, /depends on inactive package "b"/);
});

test("an active team member may not be an inactive package", () => {
  const r = run((root) => {
    pkg(root, "a", "state: inactive\n");
    put(root, "teams/t/team.yaml", "name: t\nlead: a\nmembers:\n  - agent: a\n");
  });
  assert.equal(r.code, 1);
  assert.match(r.err, /inactive package/);
});
