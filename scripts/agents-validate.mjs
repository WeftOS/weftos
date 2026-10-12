#!/usr/bin/env node
/**
 * agents-validate.mjs — AD-1 package standard gate.
 *
 * Validates every package directory under `agents/` that carries a
 * `weftos-package.yaml` against the standard in
 * docs/research/agent-directory/design.md §1.1/§1.2 and its ADR (Accepted,
 * D1-D6, D3 = versioned by weftos release — no per-package version field).
 *
 * Also validates each `agents/teams/<team>/team.yaml` team file; a team that
 * declares `schema: 1` is checked against team spec v1 (agents-team-validate.mjs,
 * agents/teams/SCHEMA.md, ADR-112).
 *
 * Legacy content that predates this standard (agents/clawft/,
 * agents/code-reviewer/, agents/weftos-ecc/, agents/weftos-kernel/,
 * agents/weftos-mesh/, agents/README.md) has no weftos-package.yaml. The
 * twelve agents under agents/weftos/<id>/ are packaged as originals and
 * marked `state: inactive` (ADR-121 decision 10).
 *
 * ADR-121 checks, across packages: `state` is active|inactive; `parent` is
 * null or the digest of a package in this tree; `dependencies` entries name
 * exactly one of agent|skill|cog|model_role and agent/skill ones resolve;
 * the agent dependency graph has no cycle; an active package or an active
 * team member never depends on an inactive package (inactive packages are
 * excluded from apply). Digests are computed (scripts/lib/package-digest.mjs),
 * never written by hand.
 *
 * Usage: node scripts/agents-validate.mjs [--quiet]
 * Exit 0 = every discovered package and team file is clean.
 * Exit 1 = at least one violation.
 */
import { readFileSync, readdirSync, statSync, existsSync } from "node:fs";
import { join, dirname, relative, basename } from "node:path";
import { fileURLToPath } from "node:url";
import { parseYaml, splitFrontmatter } from "./lib/yaml-lite.mjs";
import { validateTeamV1, workspaceVersion } from "./agents-team-validate.mjs";
import { packageDigest, DIGEST_RE } from "./lib/package-digest.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, "..");
// WEFTOS_AGENTS_DIR lets the tests validate a fixture tree.
const AGENTS_DIR = process.env.WEFTOS_AGENTS_DIR ?? join(ROOT, "agents");
const QUIET = process.argv.includes("--quiet");

const LEGACY_TOP_LEVEL = new Set([
  "clawft",
  "code-reviewer",
  "weftos-ecc",
  "weftos-kernel",
  "weftos-mesh",
]);
const KNOWN_KINDS = new Set(["specialist", "lane", "template", "skill"]);
const KNOWN_TRUST_TIERS = new Set(["core", "internal", "client", "external", "template"]);
const KNOWN_STATES = new Set(["active", "inactive"]);
// Schema file: agents/schemas/brain-metadata.schema.json.
const BRAIN_SCHEMA_ID = "weftos.brain-metadata/1";
const KNOWN_BRAIN_NOTES = new Set(["project", "none"]);
const DEP_KEYS = ["agent", "skill", "cog", "model_role"];
const KNOWN_VERDICTS = new Set(["adopt", "adapt", "pattern", "build"]);
const MAX_SKILL_LINES = 300;
const ABSOLUTE_USER_PATH = /\/(?:Users|home)\/[^\s'")\]]+/;

let errorCount = 0;
let packageCount = 0;

function log(...args) {
  if (!QUIET) console.log(...args);
}

function reportFail(pkgId, message) {
  errorCount++;
  console.error(`FAIL  ${pkgId}: ${message}`);
}

function reportPass(pkgId, message) {
  log(`PASS  ${pkgId}: ${message}`);
}

/** Find every directory under `agents/` that directly contains a
 *  `weftos-package.yaml`. Does not recurse into a directory once it is
 *  identified as a package (packages don't nest packages). Skips
 *  `agents/teams/` (handled separately) and legacy top-level dirs. */
function findPackageDirs() {
  const packages = [];
  function walk(dir, relPath) {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      if (!entry.isDirectory()) continue;
      if (relPath === "" && LEGACY_TOP_LEVEL.has(entry.name)) continue;
      if (relPath === "" && entry.name === "teams") continue;
      const childAbs = join(dir, entry.name);
      const childRel = relPath ? `${relPath}/${entry.name}` : entry.name;
      if (existsSync(join(childAbs, "weftos-package.yaml"))) {
        packages.push(childRel);
      } else {
        walk(childAbs, childRel);
      }
    }
  }
  walk(AGENTS_DIR, "");
  return packages.sort();
}

function findMarkdownLinks(text) {
  const links = [];
  const re = /\[[^\]]*\]\(([^)]+)\)/g;
  let m;
  while ((m = re.exec(text))) {
    links.push(m[1].trim());
  }
  return links;
}

function checkReferenceLinks(pkgId, pkgAbs) {
  function walk(dir) {
    let entries;
    try {
      entries = readdirSync(dir, { withFileTypes: true });
    } catch {
      return;
    }
    for (const entry of entries) {
      const abs = join(dir, entry.name);
      if (entry.isDirectory()) {
        walk(abs);
        continue;
      }
      if (!entry.name.endsWith(".md")) continue;
      // Only lint links inside a references/ directory, per the AD-1 brief.
      if (!abs.split("/").includes("references")) continue;
      const text = readFileSync(abs, "utf8");
      for (const link of findMarkdownLinks(text)) {
        if (/^https?:\/\//.test(link) || link.startsWith("#") || link.startsWith("mailto:")) {
          continue;
        }
        if (link.includes("../")) {
          reportFail(pkgId, `${relative(pkgAbs, abs)} links outside the package via "../": ${link}`);
          continue;
        }
        const target = join(dirname(abs), link.split("#")[0]);
        if (!existsSync(target)) {
          reportFail(pkgId, `${relative(pkgAbs, abs)} has an orphan reference link: ${link}`);
        }
      }
    }
  }
  walk(pkgAbs);
}

function skillSmellLint(pkgId, label, description, fullText) {
  if (typeof description !== "string" || !description.includes("Use when:")) {
    reportFail(pkgId, `${label} description is missing a "Use when:" trigger`);
  }
  if (ABSOLUTE_USER_PATH.test(fullText)) {
    const match = ABSOLUTE_USER_PATH.exec(fullText)[0];
    reportFail(pkgId, `${label} contains an absolute user path: ${match}`);
  }
}

function validateSkillMd(pkgId, skillPath, pkgAbs) {
  const text = readFileSync(skillPath, "utf8");
  const lineCount = text.split("\n").length;
  const label = `SKILL.md (${relative(pkgAbs, skillPath)})`;
  if (lineCount > MAX_SKILL_LINES) {
    reportFail(pkgId, `${label} is ${lineCount} lines, over the ${MAX_SKILL_LINES}-line cap`);
  }
  const { frontmatter } = splitFrontmatter(text);
  if (!frontmatter) {
    reportFail(pkgId, `${label} has no frontmatter block`);
    return null;
  }
  if (!frontmatter.name) reportFail(pkgId, `${label} frontmatter is missing "name"`);
  if (!frontmatter.description) reportFail(pkgId, `${label} frontmatter is missing "description"`);
  skillSmellLint(pkgId, label, frontmatter.description, text);
  return frontmatter;
}

function validateAgentMd(pkgId, agentPath, pkgAbs) {
  const text = readFileSync(agentPath, "utf8");
  const label = "AGENT.md";
  const { frontmatter } = splitFrontmatter(text);
  if (!frontmatter) {
    reportFail(pkgId, `${label} has no frontmatter block`);
    return null;
  }
  const required = ["name", "nickname", "role", "description", "tools", "model_hint", "trust_tier", "kind"];
  for (const key of required) {
    if (!(key in frontmatter) || frontmatter[key] === null || frontmatter[key] === "") {
      reportFail(pkgId, `${label} frontmatter is missing "${key}"`);
    }
  }
  skillSmellLint(pkgId, label, frontmatter.description, text);
  return frontmatter;
}

function findSkillDirs(pkgAbs) {
  const skillsRoot = join(pkgAbs, "skills");
  if (!existsSync(skillsRoot)) return [];
  return readdirSync(skillsRoot, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => e.name)
    .sort();
}

function countScenarios(text) {
  return (text.match(/^## /gm) || []).length;
}

/**
 * Validate one package directory. Returns a summary object used by the
 * cross-package pass (dependency resolution) and by the catalog generator.
 */
function validatePackage(pkgRel, allPackageIds) {
  packageCount++;
  const errorsBefore = errorCount;
  const pkgAbs = join(AGENTS_DIR, pkgRel);
  const id = basename(pkgRel);
  const pkgId = pkgRel;

  const yamlPath = join(pkgAbs, "weftos-package.yaml");
  let pkg;
  try {
    pkg = parseYaml(readFileSync(yamlPath, "utf8"));
  } catch (e) {
    reportFail(pkgId, `weftos-package.yaml failed to parse: ${e.message}`);
    return null;
  }
  if (!pkg || typeof pkg !== "object") {
    reportFail(pkgId, "weftos-package.yaml did not parse to a mapping");
    return null;
  }

  if ("version" in pkg) {
    reportFail(pkgId, 'weftos-package.yaml declares a "version" field — packages are versioned by weftos release (D3), not per-package');
  }

  const requiredYamlKeys = ["name", "kind", "trust_tier", "capabilities", "required_project_context", "requires", "provenance"];
  for (const key of requiredYamlKeys) {
    if (!(key in pkg)) reportFail(pkgId, `weftos-package.yaml is missing required field "${key}"`);
  }

  if (pkg.name && pkg.name !== id) {
    reportFail(pkgId, `weftos-package.yaml name "${pkg.name}" does not match its directory "${id}"`);
  }
  if (pkg.kind && !KNOWN_KINDS.has(pkg.kind)) {
    reportFail(pkgId, `weftos-package.yaml kind "${pkg.kind}" is not one of ${[...KNOWN_KINDS].join(", ")}`);
  }
  if (pkg.trust_tier && !KNOWN_TRUST_TIERS.has(pkg.trust_tier)) {
    reportFail(pkgId, `weftos-package.yaml trust_tier "${pkg.trust_tier}" is not one of ${[...KNOWN_TRUST_TIERS].join(", ")}`);
  }
  if (pkg.capabilities && !Array.isArray(pkg.capabilities)) {
    reportFail(pkgId, "weftos-package.yaml capabilities must be a list");
  }
  if (pkg.required_project_context && !Array.isArray(pkg.required_project_context)) {
    reportFail(pkgId, "weftos-package.yaml required_project_context must be a list");
  }

  // provenance
  if (pkg.provenance && typeof pkg.provenance === "object") {
    for (const key of ["source_registry", "source_url", "upstream_license", "upstream_commit", "adopted_at", "verdict"]) {
      if (!(key in pkg.provenance)) {
        reportFail(pkgId, `weftos-package.yaml provenance is missing "${key}"`);
      }
    }
    if (pkg.provenance.verdict && !KNOWN_VERDICTS.has(pkg.provenance.verdict)) {
      reportFail(pkgId, `weftos-package.yaml provenance.verdict "${pkg.provenance.verdict}" is not one of ${[...KNOWN_VERDICTS].join(", ")}`);
    }
    if (!pkg.provenance.adopted_at) {
      reportFail(pkgId, "weftos-package.yaml provenance.adopted_at must be set");
    }
  }

  // requires: skill: <name> must resolve to skills/<name>/SKILL.md, or to
  // this package's own top-level SKILL.md when it IS that skill.
  const requires = Array.isArray(pkg.requires) ? pkg.requires : [];
  for (const req of requires) {
    if (!req || typeof req !== "object" || !req.skill) {
      reportFail(pkgId, `requires entry is malformed: ${JSON.stringify(req)}`);
      continue;
    }
    const nested = join(pkgAbs, "skills", req.skill, "SKILL.md");
    const topLevel = join(pkgAbs, "SKILL.md");
    let resolved = existsSync(nested);
    if (!resolved && existsSync(topLevel)) {
      const { frontmatter } = splitFrontmatter(readFileSync(topLevel, "utf8"));
      resolved = frontmatter && frontmatter.name === req.skill;
    }
    if (!resolved) {
      reportFail(pkgId, `requires.skill "${req.skill}" does not resolve to a skill in this package`);
    }
  }

  // state / parent (ADR-121 §3).
  if ("state" in pkg && !KNOWN_STATES.has(pkg.state)) {
    reportFail(pkgId, `weftos-package.yaml state "${pkg.state}" is not one of ${[...KNOWN_STATES].join(", ")}`);
  }
  if (pkg.parent !== undefined && pkg.parent !== null && !DIGEST_RE.test(String(pkg.parent))) {
    reportFail(pkgId, `weftos-package.yaml parent must be null or sha256:<64 hex>, got ${JSON.stringify(pkg.parent)}`);
  }

  // brain (ADR-121 decision 9): which brain metadata schema entries follow and
  // whether the agent leaves project notes.
  if ("brain" in pkg) {
    const b = pkg.brain;
    if (!b || typeof b !== "object" || b.schema !== BRAIN_SCHEMA_ID) {
      reportFail(pkgId, `weftos-package.yaml brain.schema must be "${BRAIN_SCHEMA_ID}"`);
    } else if (!KNOWN_BRAIN_NOTES.has(b.notes)) {
      reportFail(pkgId, `weftos-package.yaml brain.notes "${b.notes}" is not one of ${[...KNOWN_BRAIN_NOTES].join(", ")}`);
    }
  }

  // dependencies: exactly one of agent|skill|cog|model_role per entry; agent
  // must resolve to a known package id (skill is resolved in the cross pass).
  const dependencies = Array.isArray(pkg.dependencies) ? pkg.dependencies : [];
  for (const dep of dependencies) {
    const keys = dep && typeof dep === "object" ? DEP_KEYS.filter((k) => k in dep) : [];
    if (keys.length !== 1 || typeof dep[keys[0]] !== "string" || dep[keys[0]] === "") {
      reportFail(pkgId, `dependencies entry must name exactly one of ${DEP_KEYS.join("|")}: ${JSON.stringify(dep)}`);
      continue;
    }
    if (keys[0] === "agent" && allPackageIds && !allPackageIds.has(dep.agent)) {
      reportFail(pkgId, `dependencies.agent "${dep.agent}" does not match any known agent package`);
    }
  }

  // Required files: AGENT.md or SKILL.md, evals/scenarios.md with >= 3.
  const agentMdPath = join(pkgAbs, "AGENT.md");
  const topSkillPath = join(pkgAbs, "SKILL.md");
  const hasAgentMd = existsSync(agentMdPath);
  const hasTopSkill = existsSync(topSkillPath);
  if (!hasAgentMd && !hasTopSkill) {
    reportFail(pkgId, "package has neither AGENT.md nor a top-level SKILL.md");
  }

  let agentFrontmatter = null;
  if (hasAgentMd) agentFrontmatter = validateAgentMd(pkgId, agentMdPath, pkgAbs);
  let topSkillFrontmatter = null;
  if (hasTopSkill) topSkillFrontmatter = validateSkillMd(pkgId, topSkillPath, pkgAbs);

  const evalsPath = join(pkgAbs, "evals", "scenarios.md");
  if (!existsSync(evalsPath)) {
    reportFail(pkgId, "package is missing evals/scenarios.md");
  } else {
    const n = countScenarios(readFileSync(evalsPath, "utf8"));
    if (n < 3) {
      reportFail(pkgId, `evals/scenarios.md has only ${n} scenario(s), need >= 3`);
    }
  }

  // Every nested skills/<name>/SKILL.md.
  const skillDirs = findSkillDirs(pkgAbs);
  for (const skillName of skillDirs) {
    const skillMdPath = join(pkgAbs, "skills", skillName, "SKILL.md");
    if (!existsSync(skillMdPath)) {
      reportFail(pkgId, `skills/${skillName}/ is missing SKILL.md`);
      continue;
    }
    validateSkillMd(pkgId, skillMdPath, pkgAbs);
  }

  // references/ link resolution.
  checkReferenceLinks(pkgId, pkgAbs);

  if (errorCount === errorsBefore) reportPass(pkgId, "clean");

  return {
    id,
    path: pkgRel,
    pkg,
    agentFrontmatter,
    topSkillFrontmatter,
    skillDirs,
  };
}

function validateTeams(allPackageIds, results = []) {
  const teamCtx = {
    version: workspaceVersion(),
    packages: new Map(
      results.map((r) => [
        r.id,
        {
          kind: r.pkg.kind,
          capabilities: Array.isArray(r.pkg.capabilities) ? r.pkg.capabilities : [],
          tools: Array.isArray(r.agentFrontmatter?.tools) ? r.agentFrontmatter.tools : null,
        },
      ]),
    ),
  };
  const teamsRoot = join(AGENTS_DIR, "teams");
  if (!existsSync(teamsRoot)) return [];
  const teamDirs = readdirSync(teamsRoot, { withFileTypes: true })
    .filter((e) => e.isDirectory())
    .map((e) => e.name)
    .sort();
  const teams = [];
  for (const teamName of teamDirs) {
    const teamId = `teams/${teamName}`;
    const errorsBefore = errorCount;
    const teamYamlPath = join(teamsRoot, teamName, "team.yaml");
    if (!existsSync(teamYamlPath)) {
      reportFail(teamId, "team directory is missing team.yaml");
      continue;
    }
    let team;
    try {
      team = parseYaml(readFileSync(teamYamlPath, "utf8"));
    } catch (e) {
      reportFail(teamId, `team.yaml failed to parse: ${e.message}`);
      continue;
    }
    if (team.schema !== undefined) {
      for (const msg of validateTeamV1(team, teamCtx)) reportFail(teamId, msg);
    }
    const members = Array.isArray(team.members) ? team.members : [];
    let activeCount = 0;
    for (const member of members) {
      if (!member || typeof member !== "object" || !member.agent) {
        reportFail(teamId, `member entry is malformed: ${JSON.stringify(member)}`);
        continue;
      }
      if (!allPackageIds.has(String(member.agent).split("@")[0])) {
        reportFail(teamId, `member.agent "${member.agent}" does not match any known agent package`);
      }
      if (member.active !== false) {
        activeCount++;
        const target = results.find((r) => r.id === String(member.agent).split("@")[0]);
        if (target?.pkg.state === "inactive") {
          reportFail(teamId, `active member "${member.agent}" is an inactive package (excluded from apply)`);
        }
      }
    }
    const sharedSkills = Array.isArray(team.shared_skills) ? team.shared_skills : [];
    for (const skillPkg of sharedSkills) {
      if (!allPackageIds.has(skillPkg)) {
        reportFail(teamId, `shared_skills entry "${skillPkg}" does not match any known agent package`);
      }
    }
    if (activeCount > 8) {
      reportFail(teamId, `team has ${activeCount} active members, over the cap of 8`);
    }
    if (errorCount === errorsBefore) reportPass(teamId, "clean");
    teams.push({ id: teamName, path: `agents/${teamId}`, team });
  }
  return teams;
}

/** Packages' own skill names, so a `skill` dependency can resolve. */
function skillNames(results) {
  const names = new Set();
  for (const r of results) {
    for (const d of r.skillDirs) names.add(d);
    if (r.topSkillFrontmatter?.name) names.add(r.topSkillFrontmatter.name);
  }
  return names;
}

/** Depth-first search for a cycle in the agent dependency graph; returns the path or null. */
function findCycle(graph) {
  const state = new Map();
  const stack = [];
  function visit(id) {
    state.set(id, 1);
    stack.push(id);
    for (const next of graph.get(id) ?? []) {
      if (state.get(next) === 1) return [...stack.slice(stack.indexOf(next)), next];
      if (!state.has(next)) {
        const c = visit(next);
        if (c) return c;
      }
    }
    stack.pop();
    state.set(id, 2);
    return null;
  }
  for (const id of [...graph.keys()].sort()) {
    if (!state.has(id)) {
      const c = visit(id);
      if (c) return c;
    }
  }
  return null;
}

/** ADR-121 cross-package checks; fills `digest` on every result. */
function crossChecks(results) {
  const byId = new Map(results.map((r) => [r.id, r]));
  const skills = skillNames(results);
  for (const r of results) {
    try {
      r.digest = packageDigest(join(AGENTS_DIR, r.path));
    } catch (e) {
      reportFail(r.path, e.message);
      r.digest = null;
    }
  }
  const digests = new Set(results.map((r) => r.digest));
  const graph = new Map();
  for (const r of results) {
    const state = r.pkg.state ?? "active";
    const deps = Array.isArray(r.pkg.dependencies) ? r.pkg.dependencies : [];
    graph.set(r.id, deps.filter((d) => d && d.agent).map((d) => d.agent).filter((a) => byId.has(a)));
    if (r.pkg.parent && !digests.has(r.pkg.parent)) {
      reportFail(r.path, `parent ${r.pkg.parent} does not match the digest of any package in this tree`);
    }
    for (const dep of deps) {
      if (dep && dep.skill && !skills.has(dep.skill)) {
        reportFail(r.path, `dependencies.skill "${dep.skill}" does not match any skill in the tree`);
      }
      if (dep && dep.agent && state === "active" && byId.get(dep.agent)?.pkg.state === "inactive") {
        reportFail(r.path, `active package depends on inactive package "${dep.agent}" (inactive packages are excluded from apply)`);
      }
    }
  }
  const cycle = findCycle(graph);
  if (cycle) reportFail("dependencies", `dependency cycle: ${cycle.join(" -> ")}`);
}

export function runValidation() {
  errorCount = 0;
  packageCount = 0;
  const pkgRels = findPackageDirs();
  const allPackageIds = new Set(pkgRels.map((p) => basename(p)));
  const results = [];
  const seenIds = new Set();
  for (const pkgRel of pkgRels) {
    if (seenIds.has(basename(pkgRel))) {
      reportFail(pkgRel, `package id "${basename(pkgRel)}" is used by more than one directory`);
    }
    seenIds.add(basename(pkgRel));
    const result = validatePackage(pkgRel, allPackageIds);
    if (result) results.push(result);
  }
  crossChecks(results);
  const teams = validateTeams(allPackageIds, results);
  return { errorCount, packageCount, results, teams };
}

function main() {
  const { errorCount: errs, packageCount: count } = runValidation();
  log(`\n${count} package(s) checked, ${errs} error(s).`);
  if (errs > 0) process.exit(1);
}

if (import.meta.url === `file://${process.argv[1]}`) {
  main();
}
