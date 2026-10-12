#!/usr/bin/env node
/**
 * agents-catalog.mjs — AD-1 catalog generator.
 *
 * Regenerates `agents/catalog.json` from every validated package under
 * `agents/` (see agents-validate.mjs) and every `agents/teams/<team>/team.yaml`.
 * Deterministic: sorted output, no timestamps, no wall-clock or network
 * input — the same source tree always produces the same bytes.
 *
 * Usage:
 *   node scripts/agents-catalog.mjs           # regenerate and write
 *   node scripts/agents-catalog.mjs --check    # exit 1 if catalog.json is stale
 */
import { readFileSync, writeFileSync, existsSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";
import { runValidation } from "./agents-validate.mjs";
import { packageDigestHex, teamDigestHex } from "./lib/package-digest.mjs";

const __dirname = dirname(fileURLToPath(import.meta.url));
const ROOT = join(__dirname, "..");
const CATALOG_PATH = join(ROOT, "agents", "catalog.json");
const CHECK = process.argv.includes("--check");

function workspaceVersion() {
  const text = readFileSync(join(ROOT, "Cargo.toml"), "utf8");
  const wsIdx = text.indexOf("[workspace.package]");
  if (wsIdx === -1) {
    throw new Error("Cargo.toml has no [workspace.package] section");
  }
  const after = text.slice(wsIdx);
  const m = /\nversion\s*=\s*"([^"]+)"/.exec(after);
  if (!m) throw new Error("Cargo.toml [workspace.package] has no version");
  return m[1];
}

/** ADR-121 §3 dependencies: agents pinned by digest, skills/cogs/model roles by name. */
function catalogDependencies(pkg, digestById) {
  const deps = Array.isArray(pkg.dependencies) ? pkg.dependencies : [];
  return deps
    .map((d) => {
      if (d.agent) return { kind: "agent", name: d.agent, digest: digestById.get(d.agent) ?? null };
      if (d.skill) return { kind: "skill", name: d.skill };
      if (d.cog) return { kind: "cog", name: d.cog };
      return { kind: "model_role", name: d.model_role };
    })
    .sort((a, b) => `${a.kind}/${a.name}`.localeCompare(`${b.kind}/${b.name}`));
}

function buildCatalog(results, teams) {
  const version = workspaceVersion();
  const digestById = new Map(results.map((r) => [r.id, r.digest]));
  const agents = results
    .map((r) => {
      const pkg = r.pkg;
      const description = r.agentFrontmatter?.description ?? r.topSkillFrontmatter?.description ?? null;
      const role = r.agentFrontmatter?.role ?? null;
      const nickname = r.agentFrontmatter?.nickname ?? pkg.nickname ?? null;
      const skills = r.skillDirs.length > 0
        ? r.skillDirs
        : (r.topSkillFrontmatter?.name ? [r.topSkillFrontmatter.name] : []);
      return {
        id: r.id,
        name: pkg.name ?? r.id,
        nickname,
        kind: pkg.kind ?? null,
        role,
        description,
        trust_tier: pkg.trust_tier ?? null,
        capabilities: Array.isArray(pkg.capabilities) ? pkg.capabilities : [],
        hosts: ["claude", "grok", "codex"],
        skills,
        requires: Array.isArray(pkg.requires) ? pkg.requires : [],
        provenance: pkg.provenance ?? {},
        version,
        digest: r.digest,
        parent: pkg.parent ?? null,
        state: pkg.state ?? "active",
        dependencies: catalogDependencies(pkg, digestById),
        path: `agents/${r.path}`,
      };
    })
    .sort((a, b) => a.id.localeCompare(b.id));

  // Team digest: the team directory plus its active member packages. Active ids
  // follow package.rs team_members; only top-level, non-inactive packages pin.
  const topLevel = new Map(
    results.filter((r) => !r.path.includes("/") && (r.pkg.state ?? "active") === "active").map((r) => [r.id, r.digest.slice("sha256:".length)]),
  );
  const activeMemberIds = (team) => {
    const ids = [];
    const push = (item) => {
      if (item === null || item === undefined) return;
      const raw = typeof item === "string" ? item : (item.agent ?? item.id ?? item.package ?? item.skill);
      if (typeof raw !== "string" || (typeof item === "object" && item.active === false)) return;
      ids.push(raw.split("@")[0]);
    };
    for (const key of ["lead", "members", "shared_skills", "skills"]) {
      const v = team[key];
      if (Array.isArray(v)) v.forEach(push);
      else push(v);
    }
    return ids;
  };

  const teamEntries = teams
    .map((t) => ({
      id: t.id,
      name: t.team.name ?? t.id,
      lead: t.team.lead ?? null,
      shared_skills: Array.isArray(t.team.shared_skills) ? t.team.shared_skills : [],
      members: Array.isArray(t.team.members) ? t.team.members : [],
      ...(t.team.schema === 1
        ? {
            schema: 1,
            edges: t.team.edges ?? [],
            write_authority: t.team.write_authority ?? {},
            team_rules: t.team.team_rules ?? [],
            presets: t.team.presets ?? {},
            hosts: t.team.hosts ?? {},
          }
        : {}),
      version,
      digest: `sha256:${teamDigestHex(join(ROOT, t.path), activeMemberIds(t.team), topLevel)}`,
      path: t.path,
    }))
    .sort((a, b) => a.id.localeCompare(b.id));

  return {
    version,
    agents,
    teams: teamEntries,
  };
}

function serialize(catalog) {
  return JSON.stringify(catalog, null, 2) + "\n";
}

function main() {
  const { errorCount, results, teams } = runValidation();
  if (errorCount > 0) {
    console.error(`\nRefusing to generate agents/catalog.json: ${errorCount} package validation error(s). Run "node scripts/agents-validate.mjs" for details.`);
    process.exit(1);
  }

  const catalog = buildCatalog(results, teams);
  const next = serialize(catalog);

  if (CHECK) {
    const current = existsSync(CATALOG_PATH) ? readFileSync(CATALOG_PATH, "utf8") : null;
    if (current === next) {
      console.log(`agents/catalog.json is up to date (${catalog.agents.length} agent(s), ${catalog.teams.length} team(s)).`);
      process.exit(0);
    }
    console.error("agents/catalog.json is stale — run: node scripts/agents-catalog.mjs");
    if (current === null) {
      console.error("  (file does not exist yet)");
    }
    process.exit(1);
  }

  writeFileSync(CATALOG_PATH, next);
  console.log(`Wrote agents/catalog.json (${catalog.agents.length} agent(s), ${catalog.teams.length} team(s), version ${catalog.version}).`);
}

main();
