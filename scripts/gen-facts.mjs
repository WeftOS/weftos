#!/usr/bin/env node
// Generates docs/src/public/facts.json: the numbers the docs-site home page, the
// root README and the WeftOS org profile show. Deterministic: sorted keys, no
// timestamps. No dependencies.
//
//   node scripts/gen-facts.mjs                  write facts.json
//   node scripts/gen-facts.mjs --update-readme  also rewrite the README facts block
//   node scripts/gen-facts.mjs --check          fail if facts.json or the README block is stale
//   node scripts/gen-facts.mjs --offline        do not fetch the Cogs repo (reuse the committed values)
//
// Run it through `scripts/build.sh gen-facts [--check|--update-readme]`.
//
// Sources:
//   version, crates, published   Cargo.toml and each member's Cargo.toml
//   adrs                         docs/adr/adr-*.md
//   weaver_commands, weft_commands
//                                docs/reference/cli.md (generated from the built
//                                binaries by scripts/gen-cli-reference.sh)
//   cogs, catalog.*              WeftOS/weftos-cogs at the rev pinned in Cargo.toml,
//                                read through raw.githubusercontent.com / the git
//                                trees API (read-only, optional GITHUB_TOKEN)
//   generated_from               last commit that touched the inputs above
//
// --check ignores `generated_from` (a squash merge changes it) and, when the Cogs
// repo cannot be reached, the Cogs numbers (the run says so).

import { execFileSync } from 'node:child_process';
import { existsSync, readFileSync, readdirSync, writeFileSync, mkdirSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const ROOT = resolve(dirname(fileURLToPath(import.meta.url)), '..');
const FACTS = join(ROOT, 'docs/src/public/facts.json');
const README = join(ROOT, 'README.md');
const START = '<!-- facts:start -->';
const END = '<!-- facts:end -->';

const args = new Set(process.argv.slice(2));
const CHECK = args.has('--check');
const UPDATE_README = args.has('--update-readme');
const OFFLINE = args.has('--offline');

const read = (p) => readFileSync(join(ROOT, p), 'utf8');

function workspace() {
  const toml = read('Cargo.toml');
  const members = toml.match(/^members\s*=\s*\[([\s\S]*?)^\]/m);
  if (!members) throw new Error('Cargo.toml: no [workspace] members array');
  const paths = [...members[1].replace(/#.*$/gm, '').matchAll(/"([^"]+)"/g)].map((m) => m[1]);
  const version = toml.match(/^\[workspace\.package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1];
  if (!version) throw new Error('Cargo.toml: no [workspace.package] version');
  let published = 0;
  for (const p of paths) {
    const manifest = read(`${p}/Cargo.toml`);
    const pkg = manifest.split(/^\[package\]\s*$/m)[1]?.split(/^\[/m)[0] ?? '';
    if (!/^publish\s*=\s*(false|\[\s*\])\s*(#.*)?$/m.test(pkg)) published += 1;
  }
  return { version, crates: paths.length, published, toml };
}

function cogsPin(toml) {
  const m = toml.match(/cog-sources\s*=\s*\{[^}]*git\s*=\s*"https:\/\/github\.com\/([^"/]+\/[^"/]+?)"[^}]*rev\s*=\s*"([0-9a-f]{40})"[^}]*\}\s*#\s*(v[\w.\-]+)/);
  if (!m) throw new Error('Cargo.toml: cannot find the weftos-cogs rev pin on cog-sources');
  return { repo: m[1], rev: m[2], tag: m[3] };
}

function cliCommands() {
  const md = read('docs/reference/cli.md');
  const count = (bin) => {
    const lines = md.split('\n');
    let i = lines.indexOf(`# ${bin}`);
    if (i < 0) throw new Error(`cli.md: no "# ${bin}" section`);
    while (i < lines.length && lines[i] !== '## Commands') i += 1;
    let n = 0;
    for (i += 1; i < lines.length && !lines[i].startsWith('## '); i += 1) {
      if (lines[i].startsWith(`- [\`${bin} `)) n += 1;
    }
    return n;
  };
  return { weaver_commands: count('weaver'), weft_commands: count('weft') };
}

function adrCount() {
  return readdirSync(join(ROOT, 'docs/adr')).filter((f) => /^adr-.*\.md$/.test(f)).length;
}

async function fetchJson(url) {
  const headers = { 'user-agent': 'weftos-gen-facts' };
  if (process.env.GITHUB_TOKEN && url.startsWith('https://api.github.com')) {
    headers.authorization = `Bearer ${process.env.GITHUB_TOKEN}`;
  }
  const res = await fetch(url, { headers, signal: AbortSignal.timeout(20000) });
  if (!res.ok) throw new Error(`${url}: HTTP ${res.status}`);
  return res.json();
}

async function cogsFacts(pin) {
  const catalog = await fetchJson(
    `https://raw.githubusercontent.com/${pin.repo}/${pin.rev}/crates/cog-market/catalog/catalog.json`,
  );
  const tree = await fetchJson(`https://api.github.com/repos/${pin.repo}/git/trees/${pin.rev}?recursive=1`);
  if (tree.truncated) throw new Error('cogs tree listing truncated');
  const cogs = tree.tree.filter((t) => /^src\/cogs\/[^/]+\/cog\.toml$/.test(t.path)).length;
  return {
    cogs,
    catalog: { modules: catalog.modules.length, chips: catalog.chips.length },
  };
}

function generatedFrom() {
  try {
    return (
      execFileSync(
        'git',
        ['log', '-1', '--format=%h', '--', 'Cargo.toml', 'crates', 'docs/adr', 'docs/reference/cli.md'],
        { cwd: ROOT, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'] },
      ).trim() || 'unknown'
    );
  } catch {
    return 'unknown';
  }
}

const sorted = (v) =>
  Array.isArray(v)
    ? v.map(sorted)
    : v && typeof v === 'object'
      ? Object.fromEntries(Object.keys(v).sort().map((k) => [k, sorted(v[k])]))
      : v;

const readPrevious = () => (existsSync(FACTS) ? JSON.parse(readFileSync(FACTS, 'utf8')) : null);

function readmeBlock(f) {
  return [
    `- ${f.crates} workspace crates, ${f.published} of them published to crates.io`,
    `- ${f.adrs} architecture decision records`,
    `- ${f.cogs} cogs in the Cogs repository, and ${f.catalog.modules} modules in the hardware catalog`,
  ].join('\n');
}

function spliceReadme(text, block) {
  const a = text.indexOf(START);
  const b = text.indexOf(END);
  if (a < 0 || b < a) throw new Error(`README.md: ${START} / ${END} markers not found`);
  return `${text.slice(0, a + START.length)}\n${block}\n${text.slice(b)}`;
}

async function main() {
  const ws = workspace();
  const pin = cogsPin(ws.toml);
  const previous = readPrevious();

  let cogs;
  let offline = false;
  if (OFFLINE) {
    offline = true;
  } else {
    try {
      cogs = await cogsFacts(pin);
    } catch (e) {
      offline = true;
      console.error(`gen-facts: Cogs repo not reachable (${e.message}); reusing committed values`);
    }
  }
  if (offline) {
    if (!previous?.cogs) throw new Error('offline and no committed facts.json to reuse the Cogs numbers from');
    cogs = { cogs: previous.cogs, catalog: previous.catalog };
  }

  const facts = sorted({
    adrs: adrCount(),
    catalog: cogs.catalog,
    cogs: cogs.cogs,
    cogs_release: pin.tag,
    cogs_source_offline: offline,
    crates: ws.crates,
    generated_from: generatedFrom(),
    latest_release: `v${ws.version}`,
    published: ws.published,
    version: ws.version,
    ...cliCommands(),
  });

  const volatile = (f) => {
    const { generated_from, ...rest } = f;
    return rest;
  };
  const same = (a, b) => JSON.stringify(sorted(a)) === JSON.stringify(sorted(b));

  const body = `${JSON.stringify(facts, null, 2)}\n`;
  const readmeNow = readFileSync(README, 'utf8');
  const readmeWant = spliceReadme(readmeNow, readmeBlock(facts));

  if (CHECK) {
    const a = previous ? volatile(previous) : null;
    const b = volatile(facts);
    if (offline) {
      // Cogs numbers are unverifiable offline; compare everything else.
      for (const f of [a, b]) if (f) { delete f.cogs; delete f.catalog; delete f.cogs_source_offline; }
    }
    const problems = [];
    if (!a || !same(a, b)) problems.push('docs/src/public/facts.json is stale or missing');
    if (readmeNow !== readmeWant && !offline) problems.push('README.md facts block is stale');
    if (readmeNow !== readmeWant && offline) {
      const block = readmeNow.slice(readmeNow.indexOf(START), readmeNow.indexOf(END));
      if (!block.includes(`${facts.crates} workspace crates`) || !block.includes(`${facts.adrs} architecture`)) {
        problems.push('README.md facts block is stale');
      }
    }
    if (problems.length) {
      console.error(`gen-facts --check: ${problems.join('; ')}.\nRun: scripts/build.sh gen-facts --update-readme`);
      process.exit(1);
    }
    console.log(`gen-facts --check: ok${offline ? ' (Cogs numbers not verified: offline)' : ''}`);
    return;
  }

  mkdirSync(dirname(FACTS), { recursive: true });
  writeFileSync(FACTS, body);
  console.log(`wrote ${FACTS.replace(`${ROOT}/`, '')}`);
  if (UPDATE_README) {
    writeFileSync(README, readmeWant);
    console.log('updated README.md facts block');
  }
}

main().catch((e) => {
  console.error(`gen-facts: ${e.message}`);
  process.exit(1);
});
