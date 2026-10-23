# npm audit residual risk (WEFT-598)

> 2026-10-18: `gui/` was removed with the Tauri shells (ADR-038). Its rows below are historical.

Last triage: 2026-10-05 on `target-0.8.3` (v0.8.3 release gate): new critical/high
cleared without breaking upgrades — root `proxy-addr` 2.0.7→2.0.8 (override; critical
GHSA-jqcg-44mw-7w3h via the pinned `@claude-flow/cli` → express), `clawft-ui` `seroval`
(critical GHSA-p6vx-979v-rg4c, high GHSA-jp82-f5mq-hwhp) and `source-map-js` <1.2.2
(high GHSA-68fv-2mgg-jv7q) in `clawft-ui`, `docs/src` and `gui` via `npm audit fix`.
The ruflo pin (3.42.4) and `agentic-flow` 2.1.x are unchanged; braces stays allowlisted.

Previous triage: 2026-09-28 on `0.8-metaharness` (v0.8.1 release gate). Prior triage:
2026-07-31 on `release/0.8-staging` (branch `fix/weft-598-npm-audit`) — see
"2026-07-31 triage" below for that round's detail.

## Gate policy

| Surface | Command | Fail threshold |
|---------|---------|----------------|
| Local / CI | `scripts/build.sh npm-audit` | critical + high |
| Phase gate | check #13 (`gate`) | same |
| CI job | `.github/workflows/pr-gates.yml` → `npm-audit` | same |

Override with `NPM_AUDIT_LEVEL` (`critical` \| `high` \| `moderate` \| …) or
`NPM_AUDIT_SOFT=1` (report only).

Audited lockfiles (when present): `clawft-ui/`, repo root, `docs/src/`, `gui/`.

## Post-triage scores (2026-09-28)

| Lockfile | Critical | High | Moderate | Low | Notes |
|----------|----------|------|----------|-----|-------|
| `clawft-ui/` | 0 | 0 | 0 | 0 | js-yaml/nanoid/@humanfs regressed since 07-31 (deps drifted); recleared via `npm audit fix` |
| root | 0 | 0 | 31 | 0 | Residual under ruflo pin (see below) — 29 OTEL-chain + 2 new (hono, qs) |
| `docs/src/` | 0 | 0 | 0 | 2 low | next 16.2.12→16.3.6 (in-range) + sharp override bump clear crit/high; esbuild low left (fumadocs-mdx pin) |
| `gui/` | 0 | 0 | 0 | 0 | js-yaml/nanoid/@humanfs recleared via `npm audit fix` |

### Fix counts (critical + high), this round

| Surface | Before (crit/high) | After | Fixed |
|---------|--------------------|-------|-------|
| clawft-ui | 0 / 2 | 0 / 0 | **2** |
| root | 0 / 10 | 0 / 0 | **10** |
| docs/src | 1 / 5 | 0 / 0 | **6** |
| gui | 0 / 2 | 0 / 0 | **2** |
| **Total crit+high cleared** | **20** | **0** | **20** |

Note: `clawft-ui` and `gui` were previously (07-31) triaged clean, but upstream
dependency releases between 07-31 and 09-28 reintroduced js-yaml, nanoid and
`@humanfs/node` findings (transitive via eslint tooling) — npm audit residual
risk is not a one-time fix, it drifts with every upstream release, hence the
gate runs every release.

### 2026-09-28: what changed

**root (`package.json` overrides)**

| Package | Old pin | New pin | Reason |
|---------|---------|---------|--------|
| `adm-zip` | `0.6.0` | `0.6.1` | High: symlink-follow extraction + uncontrolled memory allocation |
| `sharp` | `0.35.3` | `0.35.5` | High: libheif CVEs (GHSA-rgj7-g3m4-5g8c) |
| `brace-expansion` | (none) | `5.0.12` | High: DoS via unbounded intermediate arrays |
| `fast-uri` | (none) | `4.2.1` | High: host-confusion/SSRF cluster (4 advisories); no 3.x patch exists, verified no documented breaking API changes 3.x→4.x |
| `toml` | (none) | `4.3.0` | High: uncontrolled recursion (fixed 4.2.0) + prototype pollution (fixed 4.1.2); no documented breaking changes vs 3.x |

`agentic-flow` stays on `^2.1.0` (devDependency, unchanged) — it is direct
because the 3-tier model-routing / metaharness tooling shells out to it. Its
own `1.10.2` "fix" from `npm audit fix --force` is wrong-direction (major
downgrade) and was rejected, same as 07-31; the adm-zip/sharp overrides above
achieve the same CVE fix without touching agentic-flow's version, so the
`--force` path is no longer needed for anything in the crit/high band.

`@claude-flow/cli` / `ruflo` pin **unchanged at 3.42.4** (WEFT-684/669) — it
only ever showed up in `npm audit` as a rollup of the vulnerable packages
above; clearing those drops it (and `agentdb`, `agentic-flow`) back to
moderate-only.

**docs/src**

- `next` `16.2.12` → `16.3.6` (still within declared `^16.2.1`, `npm audit fix`
  picked the current in-range patch) — clears the critical (Windows RCE +
  AVIF image-optimization RCE).
- Override `sharp` `0.35.3` → `0.35.5` (same libheif CVE as root).
- `npm audit fix` (non-force) cleared browserslist, image-size, js-yaml,
  nanoid, postcss-selector-parser, baseline-browser-mapping.
- Verified with `npm run build` (Next.js 16.3.6 / Turbopack) — 97 static pages
  generated successfully, no errors.
- Residual: 2 low (esbuild, pulled in by `fumadocs-mdx` pinned to
  `14.2.7-14.2.11`; the only fix bumps `fumadocs-mdx` to `14.3.2`, outside the
  declared range — deferred since it's `low`, not gated).

**gui / clawft-ui**

- Both had drifted since 07-31: `js-yaml` (high), `nanoid` (high),
  `@humanfs/node` (moderate) via the eslint toolchain. `npm audit fix`
  (non-force) cleared all three in both trees — 0 vulnerabilities now in
  either lockfile.
- Verified with each project's build script (`tsc -b && vite build`) — both
  succeed.

### Operational note: `npm --allow-remote`

This environment's npm (12.0.2) defaults `allow-git`/`allow-remote` to `none`
(a supply-chain hardening default), which makes `npm install` / `npm audit
fix` refuse to re-fetch already-`resolved` registry tarballs during reify
(`EALLOWREMOTE`, since pacote reclassifies an exact `resolved` URL as spec
type `remote`). All fix commands in this triage were run with
`--allow-remote=all`; this is safe here because every fetch involved is a
plain npm-registry tarball for a package already pinned by exact version, not
an untrusted git/URL dependency. CI running a fresh `npm ci` is unaffected
(nothing to re-resolve).

The repo root's `node_modules/` also carries a stray `.pnpm` store from some
earlier `pnpm install` (unrelated to this task), which makes in-place `npm
install`/`npm audit fix` fail with `ENOTDIR` on rename. The root lockfile fix
above was done by regenerating `package-lock.json` in an isolated scratch
copy (`npm install --package-lock-only`) and copying it back, without
touching the live `node_modules/` tree. **Follow-up needed:** root
`node_modules/` is now stale relative to `package-lock.json` for adm-zip,
sharp, brace-expansion, fast-uri and toml — run a clean `npm install` (or
`rm -rf node_modules && npm ci`) locally before relying on those exact
versions at runtime. `docs/src`, `gui` and `clawft-ui` node_modules were
updated in place and are in sync.

---

## 2026-07-31 triage

(Exact Dependabot “142” total mixed severities/surfaces; this triage focused
critical + high on the product npm trees.)

## What was fixed (non-breaking)

### clawft-ui

- `vite` → `^7.3.6` (path traversal / dev-server highs)
- `@playwright/test` → `^1.62.1` (browser download cert verify)
- `npm audit fix` for `seroval` (critical), `postcss`, `js-yaml`
- Overrides: `brace-expansion@5.0.9`, `minimatch@^9.0.5` (eslint chain DoS)

### root (`package.json`)

Overrides (keep `ruflo` / `@claude-flow/cli` pin **3.32.38**):

| Package | Pin | Reason |
|---------|-----|--------|
| `protobufjs` | `7.6.5` | Critical RCE / nested onnx-proto 6.x |
| `undici` | `7.29.0` | High WebSocket / header issues |
| `adm-zip` | `0.6.0` | High memory allocation |
| `sharp` | `0.35.3` | High libvips CVEs |
| `@opentelemetry/propagator-jaeger` | `2.9.0` | High DoS on malformed header |

### docs/src

- `postcss` → `^8.5.25`
- Override `sharp@0.35.3`

### gui

- `esbuild` → `^0.28.1`
- Overrides: `brace-expansion`, `minimatch`, `esbuild`

## Accepted residual risk (root moderates)

31 **moderate** findings remain on the root lockfile as of 2026-09-28 (see
"2026-09-28 triage" above), almost entirely the OpenTelemetry resources/SDK
chain pulled by:

- `agentdb` → `@opentelemetry/*`
- `@claude-flow/cli@3.42.4` / `ruflo@3.42.4` (schema pin — **WEFT-684 / WEFT-669**)
- `agentic-flow@2.x`
- plus `hono` (new since 07-31, via `@modelcontextprotocol/sdk` / `fastmcp` /
  `@hono/node-server`) and `qs` (via `express`/`body-parser`) — same
  dev-tooling MCP-server chain, ReDoS/ACL-bypass moderates only.

### Why not force-fixed

1. **Ruflo pin is load-bearing** for `.swarm/agentdb-memory.db` schema ownership.
   Bumping `@claude-flow/cli` / `ruflo` outside the deliberate pin process risks
   silent AgentDB corruption.
2. npm `audit fix --force` proposes **major downgrades** of `agentic-flow` to
   `1.10.2`, which is wrong-direction and breaks the 2.x integration.
3. Moderates are DoS / resource issues in OTEL exporters used by **dev-time
   agent tooling**, not the shipped Rust daemon or clawft-ui production bundle.

### When to clear

- Next deliberate ruflo pin bump (see `package.json` → `weftos.rufloPinNote`).
- Track as follow-up if OTEL moderates are reclassified high, or if agent
  tooling is exposed on a network boundary.

## How to re-run

```bash
# All product npm trees, fail on ≥high
scripts/build.sh npm-audit

# Soft report
NPM_AUDIT_SOFT=1 scripts/build.sh npm-audit

# Single tree
(cd clawft-ui && npm audit --audit-level=high)
(cd . && npm audit --audit-level=high)
```

## UI build sanity

After bumps, `clawft-ui` must still build:

```bash
scripts/build.sh ui
# or: (cd clawft-ui && npm run build)
```

## 2026-09-30 follow-up (audit-remediation-2026-09-30)

- root: `undici` override 7.29.0 -> 7.30.0 (GHSA-3wwx-pv8p-q78v, GHSA-pmjh-fq2x-6v4x and
  others affect <= 7.29.0; reached via fastmcp and @ai-sdk/provider-utils). The ruflo pin is
  unchanged. Root now reports 0 high, 32 moderate (the existing OTEL-chain residual).
- `clawft-ui/`, `gui/`: `brace-expansion` override 5.0.9 -> 5.0.12 (advisory range 4.0.0 - 5.0.11).
  Both report 0 vulnerabilities.

## 2026-10-02: braces GHSA-vfj7-8cjw-p6xm (allowlisted residual)

- Advisory: `braces` <=3.0.3, stack-exhaustion DoS on deeply nested patterns (high, CWE-674).
- No patched release exists: `braces` latest is 3.0.3 (`npm view braces versions`), so no
  `overrides` entry can fix it. `npm audit` offers only `agentic-flow@1.10.2` (major
  downgrade, rejected as above) and `@claude-flow/cli@3.5.59` (downgrade across the WEFT-684
  pin, rejected).
- Reach: root lockfile only, `agentic-flow` -> `http-proxy-middleware` -> `micromatch` ->
  `braces`. Dev-time agent tooling; not in the shipped daemon or clawft-ui bundle. The root
  highs `@claude-flow/cli`, `agentic-flow`, `http-proxy-middleware`, `micromatch` are rollups
  of this single advisory.
- Gate handling: `NPM_AUDIT_ALLOW` in `scripts/build.sh` names this one advisory for the
  `root` lockfile with expiry 2026-12-31. The gate still fails on any other >=high advisory
  in any lockfile, and fails on this one once the expiry passes (verified by setting an
  expired date). There is no blanket soft mode involved.
- Clear when: a patched `braces` is published, or at the next deliberate ruflo pin bump.
- The advisories listed on the earlier "new highs" card (undici, hono, qs, ip-address,
  @opentelemetry/core) no longer report as high in the root lockfile; hono and qs are
  moderate residual as above.

## 2026-10-23: sprintf-js <= 1.1.3 (Dependabot #340, tolerable_risk)

- Advisory: `sprintf-js` <= 1.1.3, DoS. No patched release exists; 1.1.3 is the latest
  published version, so no `overrides` entry can fix it.
- Reach: root `package-lock.json` only, dev-only chain `agentic-flow` (root devDependency)
  -> `@huggingface/transformers` -> `global-agent` -> `roarr` -> `sprintf-js`. Not in the
  shipped daemon, clawft-ui, or docs bundles.
- Disposition: `tolerable_risk`. The dismissal on GitHub is made by the lead or owner, not
  recorded here as done.
- Revisit when: a patched `sprintf-js` is published (then `npm update sprintf-js`), when
  `roarr` or `global-agent` drops the dependency, or when the root `agentic-flow`
  devDependency is removed or re-pinned (see the usage check below).
- Usage check of the `agentic-flow` devDependency (2026-10-23): no `import`, `require` or
  `npx` use anywhere in tracked code, scripts, workflows or `.claude` / `.mcp.json`. The only
  tracked mentions are `package.json`, comments in `scripts/build.sh` and
  `scripts/metaharness/crosscut.mjs`, ignore-file comments, and research/planning docs.
  Removing it would also clear the `braces` residual above, but it is left in place pending
  an owner decision.

## 2026-10-23: toml <= 5.0.0 via pinned ruflo tooling (allowlisted residual)

New high advisories in `toml` (quadratic and CPU DoS in `toml.parse()`):
GHSA-4qx8-w7c3-pj3p, GHSA-9j65-hcg3-gc64 and GHSA-9qmh-m7c6-f92p. They reach the root
lockfile only through `ruflo` / `@claude-flow/cli` 3.42.4, which is pinned on purpose
(`package.json` `weftos.rufloPin`, WEFT-684: never `@latest`). It is developer
orchestration tooling, not shipped in any WeftOS artifact, and it parses only local,
trusted config files.

The fix requires `@claude-flow/cli` 3.60.0, which means moving the pin. That is an
owner decision, because the pin tracks a local branch with `team_*` support that the
published packages lack. Until then, the three advisories are allowlisted in
`NPM_AUDIT_ALLOW` (scripts/build.sh), with expiry 2026-12-31.

**Revisit when:** the ruflo pin moves, or by the expiry date.

Fixed in the same pass, with no allowlist needed:
- `npm audit fix` in the root (follow-redirects, qs, ip-address);
- clawft-ui vite 7.3.6 to 7.3.7 (launch-editor advisories).
