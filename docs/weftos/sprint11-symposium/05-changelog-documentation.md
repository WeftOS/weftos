# Track 5: Changelog & Documentation Workmanship

**Chair**: doc-weaver
**Panelists**: doc-weaver, reviewer, researcher, api-docs
**Date**: 2026-03-27
**Branch**: `feature/weftos-kernel-sprint`

---

## Exercise 1: CHANGELOG Audit and Rebuild Plan

### Current State

The existing `CHANGELOG.md` covers a single release entry: `[0.1.0] - 2026-02-17`. It documents the initial workspace (9 crates at the time), CLI subcommands, 7 built-in tools, 3 channel plugins, 2 services, WASM stubs, security policies, and CI infrastructure. It claims 1,029 tests.

The format follows Keep a Changelog 1.1.0. The structure is clean. The prose is direct.

### What Is Missing

134 commits have landed since 2026-02-17. The CHANGELOG reflects none of them. Here is what the git log shows was delivered across Sprints 6-10 but is entirely absent from the changelog:

**Sprint 6-7: Kernel Foundation (K3-K6)**
- Complete K3-K6 kernel implementation (commit `de6a0d2`)
- Three-branch constitutional governance with genesis rules (commits `624edcf`, `9a1839f`, `a21ece8`)
- K5 application framework lifecycle (commit `f839539`)
- Mesh networking SPARC plan and implementation docs (commits `ceef8b7` through `ad18697`)
- Fumadocs documentation site (32 pages across two sections)

**Sprint 8: OS Gap-Filling (08a/08b/08c)**
- 1,183 tests passing (commit `980be90`)
- ECC Weaver v2 -- self-evolving cognitive modeler (commit `b5dbfb0`)
- 12 specialized agent team definitions (commit `f7be7a5`)
- ONNX embedding backend + sentence-transformer (commit `656a7e7`)
- Boot path tests (45 new, commit `ca5b11a`)
- ECC graph gap analysis traversing 408 nodes, 1,597 edges (commit `57f6a9e`)

**Sprint 9: ECC Weaver and Gaps**
- 1,328 tests, 20/20 gates (commit `6441d9f`)
- Cognitive tick integration with live git polling and file watching (commit `506e7be`)
- Weaver self-evolution (41/41 TODO items, commit `e6d2c0c`)
- Session log ingestion -- 29,029 nodes from 88 conversations (commit `769ee86`)
- Spectral analysis, community detection, predictive change analysis (commit `646819f`)

**Sprint 10: Operational Hardening + K8 GUI**
- Self-healing supervisor: RestartStrategy, budget, exponential backoff (18 tests)
- Persistence: SQLite ExoChain, CausalGraph save/load, HNSW persistence (12 tests)
- Observability: DLQ, MetricsRegistry, LogService, TimerService (8 tests)
- MeshRuntime: 2-node LAN communication demonstrated
- DEMOCRITUS continuous cognitive loop (12 tests)
- WASM shell execution in sandbox (6 tests)
- Tool signing with Ed25519 (6 tests)
- K8 GUI prototype: Tauri 2.0, 4 views, Cytoscape.js knowledge graph
- External codebase analysis validated on ruvector (109 crates, 16 gaps)
- 983 new tests (613 to 1,596 kernel tests; 5,040 total annotations)
- 10 new extended tools
- Config/auth services, mesh discovery

**Workspace growth not reflected:**
- 9 crates at changelog time, 22 crates now
- 1,029 tests at changelog time, 5,040 test annotations now
- 7 plugin crates added (git, cargo, oauth2, treesitter, browser, calendar, containers)
- `exo-resource-tree` crate added
- `weftos` standalone crate added
- Feature flags grew from a handful to 93 across 20 crates

### Rebuild Plan

**Step 1: Extract commit groups by sprint.**

```bash
# Sprint 6-7 (K3-K6 implementation)
git log --oneline --after="2026-02-17" --before="2026-03-01"

# Sprint 8 (OS gap-filling)
git log --oneline --after="2026-03-01" --before="2026-03-10"

# Sprint 9 (ECC Weaver)
git log --oneline --after="2026-03-10" --before="2026-03-18"

# Sprint 10 (Operational hardening)
git log --oneline --after="2026-03-18" --before="2026-03-27"
```

**Step 2: Cross-reference with sprint plans.**

Each sprint has a plan in `.planning/sparc/weftos/0.1/` with explicit exit criteria checkboxes. Use the checked items as the authoritative list of what shipped.

**Step 3: Structure entries by Keep a Changelog sections.**

For the unreleased section:
- **Added**: New crates, new kernel layers, new features, new tools
- **Changed**: Workspace expansion (9 to 22 crates), test growth, feature flag additions
- **Fixed**: Clippy fixes, gate validation fixes
- **Security**: Tool signing, governance genesis rules, WASM sandboxing

**Step 4: Decide versioning.**

The current changelog says `[0.1.0] - 2026-02-17` but no git tag `v0.1.0` exists. Two options:
- Option A: Retroactively tag 2026-02-17 as v0.1.0-alpha, treat current state as v0.1.0
- Option B: Treat the existing 0.1.0 entry as the pre-kernel baseline, add everything since as `[Unreleased]` heading, tag v0.1.0 when Sprint 11 release gates pass

Recommendation: Option B. The existing 0.1.0 entry describes the AI framework layer only. The kernel, ECC, mesh, governance, and GUI are all post-0.1.0 work. The `[Unreleased]` section should be comprehensive, then get a proper version tag when the release is cut.

**Step 5: Validate against code.**

For every Added entry, verify the feature exists in source (grep for struct names, function signatures, feature flags in Cargo.toml). Do not add changelog entries based solely on commit messages.

**Estimated effort**: 2-3 hours of careful cross-referencing.

---

## Exercise 2: README Audit

### Score: 7/10

The README was rewritten in commit `cd79582` (README rewrite -- lead with problems solved, deployment patterns). It is substantially better than a typical project README. Here is the detailed assessment.

### What It Does Well

1. **Opening line is strong.** "AI that remembers everything, runs anywhere, trusts no one, and never stops learning." -- This is a clear, memorable tagline.

2. **Problem-first structure.** The "Problems We Solve" section leads with pain points (context windows, hallucination, cloud trust, single points of failure, uncontrolled AI, portability). This is effective for both technical and business audiences.

3. **Architecture diagram is clear.** The four-layer ASCII stack (clawft / WeftOS Kernel / ECC Cognitive Substrate / Platform Layer) communicates the layering immediately.

4. **Platform support table is comprehensive.** Nine platforms with binary names, transports, mesh roles, and use cases.

5. **Deployment patterns section is excellent.** Five concrete scenarios (personal, team, edge+cloud, air-gapped, browser-first) help readers self-identify.

6. **Feature flags table is useful.** Maps features to crates and descriptions.

7. **Build instructions use `scripts/build.sh`.** Consistent with `CLAUDE.md` mandate.

### What Needs Work

**Issue 1: Does not explain the GTM product thesis.**

The README explains what WeftOS IS technically but does not communicate the product thesis: "understand client systems, document them via knowledge graph, plan automation." A business reader cannot tell what problem this solves for them. The "Problems We Solve" section is framed as generic AI infrastructure problems, not as the specific value proposition of analyzing and understanding existing codebases/systems.

Recommendation: Add a "Who This Is For" section after the tagline. Three audiences: (a) teams that need to understand large codebases they inherited, (b) consultancies that audit client systems, (c) organizations automating knowledge capture. Each with a one-sentence value statement.

**Issue 2: Quick Start section has a broken install command.**

```sh
cargo install clawft-cli
```

This implies the crate is published on crates.io. It is not. The "Building from Source" section lower down has the correct instructions. The Quick Start should lead with `git clone` and `scripts/build.sh native`.

**Issue 3: Test count is stale.**

Line 349 says "843 tests across all kernel features." The actual count is 5,040 test annotations. This number has been wrong for months.

**Issue 4: `cargo build` commands appear directly in the Feature Flags section.**

Lines 333-345 show raw `cargo build` commands. The project's own `CLAUDE.md` mandates `scripts/build.sh` for all builds. The README should use the build script or at minimum note that `scripts/build.sh` is the preferred entry point.

**Issue 5: No weave.toml documentation pointer.**

The README mentions `weave.toml` in the self-healing section's configuration example but there is no link to a configuration reference. The `weave.toml` file exists at the project root but is not documented in the README or Quick Start.

**Issue 6: Does not onboard AI agents effectively.**

There is no section explaining how to use WeftOS as an AI agent (Claude, GPT). No SKILL.md pointer, no MCP integration quick-start, no "give this to your agent" instructions. For a project whose primary users may be AI agents operating on codebases, this is a significant gap.

**Issue 7: Developer onboarding takes longer than 5 minutes.**

The Quick Start assumes Rust 1.93+ is installed. For many developers, installing Rust and building a 22-crate workspace from source is a 10-20 minute process. There are no pre-built binaries, no Docker quick-start, no `brew install`. The 5-minute onboarding target requires either published binaries or a Docker-based path.

**Issue 8: No clear CTA differentiation.**

The Contributing section is minimal (5 lines). There is no distinction between "I want to use this," "I want to build on this," "I want to contribute," and "I want to hire you." For an open-source project with a commercial thesis, CTAs for different audiences matter.

**Issue 9: GitHub URLs are inconsistent.**

- CHANGELOG links: `github.com/clawft/clawft`
- README badge: `github.com/WeftOS/weftos`
- Quickstart clone: `github.com/clawft/clawft`

Only one of these can be correct. This needs to be settled and made consistent.

### Rewrite Recommendations (Priority Order)

1. Fix the broken `cargo install` Quick Start -- replace with `git clone` + `scripts/build.sh`
2. Fix the stale test count (843 -> 5,040)
3. Add product thesis / "Who This Is For" section
4. Unify GitHub URLs
5. Add AI agent onboarding section (SKILL.md, MCP, `weftos init`)
6. Replace raw `cargo build` commands with `scripts/build.sh` equivalents
7. Add `weave.toml` configuration pointer
8. Differentiate CTAs for users vs. developers vs. contributors vs. clients

---

## Exercise 3: Documentation Gap Analysis

### What Exists

**Fumadocs site** (`docs/src/content/docs/`): 38 MDX pages across two sections.
- clawft section: 13 pages (getting-started, architecture, CLI reference, configuration, deployment, security, plugins, providers, channels, tools, skills, browser)
- WeftOS section: 25 pages (architecture, kernel-phases, boot-sequence, process-table, IPC, capabilities, ExoChain, governance, WASM sandbox, containers, app-framework, mesh-networking, discovery, clustering, ECC, security, decisions, kernel-guide, config-auth, democritus, gui, observability, persistence, self-healing)

**Standalone docs** (`docs/`): ~80+ markdown files across subdirectories.
- `docs/index.md` -- top-level index with guide links
- `docs/getting-started/quickstart.md` -- detailed quickstart
- `docs/guides/` -- 11 guide pages (configuration, channels, providers, routing, tool-calls, rvf, skills, workspaces, voice, build, MCP)
- `docs/architecture/` -- ADRs and architecture overview
- `docs/deployment/` -- Docker, WASM, release
- `docs/browser/` -- 5 pages for browser mode
- `docs/ui/` -- 5 pages for web dashboard
- `docs/reference/` -- CLI reference, tools reference
- `docs/development/` -- contributing, testing
- `docs/benchmarks/` -- benchmark results
- `docs/skills/` -- skill index

**WeftOS-specific docs** (`docs/weftos/`): 56 markdown files.
- Architecture, configuration, feature gates, install, vision
- 4 symposium series (K2, K3, K5, ECC) with 5-9 documents each
- Sprint 11 symposium (in progress)
- Kernel governance, kernel modules, k-phases
- Weaver analysis reports
- External analysis results
- Integration patterns

**SPARC plans** (`.planning/sparc/weftos/`): 11 plan documents covering phases 07-15.

### What Is Missing

**Critical gaps (blocks release):**

| Gap | Impact | Exists Anywhere? |
|-----|--------|-------------------|
| `weave.toml` schema reference | Users cannot configure the kernel | Partial -- `docs/weftos/CONFIGURATION.md` covers JSON config, not TOML |
| Complete feature flag guide | 93 flags across 20 crates, only kernel flags documented | `docs/weftos/FEATURE_GATES.md` covers kernel only |
| API reference (rustdoc) | No generated docs hosted or linked | README links to `docs.rs/clawft` which does not exist |
| Installation guide for end users | No pre-built binaries, no package manager | `docs/weftos/INSTALL.md` exists but needs verification |
| Migration guide (config.json -> weave.toml) | Two config formats coexist, no guidance | Does not exist |

**Significant gaps (blocks adoption):**

| Gap | Impact |
|-----|--------|
| Changelog for Sprints 6-10 | No record of what changed |
| Tutorials (build an app, analyze a codebase) | No guided learning path |
| Troubleshooting / FAQ | Common errors have no documentation |
| `weftos init` workflow documentation | The primary onboarding command is undocumented |
| ECC modes documentation (Act/Analyze/Generate) | Three operating modes mentioned in README, no standalone docs |
| Mesh networking quickstart | How to set up a 2-node cluster from scratch |
| Tool catalog (all 27+ tools) | `docs/reference/tools.md` may be stale given 10 new tools |

**Structural gaps:**

| Gap | Impact |
|-----|--------|
| Fumadocs and standalone docs are not unified | Two parallel doc trees with overlapping content |
| No search across docs | No search index, no algolia/pagefind integration |
| No versioning strategy for docs | Docs describe HEAD, no way to see docs for a specific release |
| Symposium docs are internal-facing | 4 symposium series (30+ docs) are valuable but not structured for external readers |
| Sprint plans are the de facto architecture docs | `.planning/` is doing double duty as both plans and reference |

### Quantitative Summary

| Category | Count | Assessment |
|----------|-------|------------|
| Fumadocs pages | 38 | Good coverage of kernel subsystems |
| Standalone guide pages | ~25 | Decent for clawft framework |
| WeftOS reference docs | 5 | Thin -- CONFIGURATION, FEATURE_GATES, INSTALL, VISION, architecture |
| Symposium documents | 30+ | Rich internal knowledge, not externalized |
| SPARC plans | 11 | Planning artifacts, not user docs |
| API reference pages | 0 | No hosted rustdoc |
| Tutorials | 0 | No guided walkthroughs |
| Troubleshooting | 0 | No FAQ or error guide |

---

## Exercise 4: De-Slopify Assessment

### Methodology

Sampled 6 documents: self-healing.mdx, democritus.mdx, persistence.mdx, observability.mdx, quickstart.md, README.md.

### Findings

**The documentation is largely slop-free.** This is a genuine strength. The Fumadocs MDX pages in particular are tight, technical, and free of padding. Specific observations:

**Good patterns found across all sampled docs:**
- Direct declarative sentences ("The self-healing supervisor provides Erlang-inspired fault tolerance")
- Concrete specifics instead of vague claims ("5 strategies," "100ms * 2^(restart_count - 1), capped at 30 seconds")
- Actual Rust signatures and struct definitions
- Tables for reference material (strategy selection, API methods, configuration)
- Consistent structure: feature flag, source file, phase, then content
- No exclamation marks, no "powerful," no "innovative," no "cutting-edge"

**Minor slop detected:**

1. **README "Problems We Solve" headings use negative framing that borders on marketing copy.** "No more context windows" / "No more hallucination without accountability" -- these are effective for a landing page but slightly breathless for a README. The content under each heading is factual and specific, so this is a style choice, not slop.

2. **README paragraph under ECC description uses "ephemeral signals that decay rather than accumulate"** -- this is the kind of phrase that sounds profound but may confuse a reader. What does "decay" mean concretely? (TTL expiry on impulse queue entries.) The Fumadocs page for DEMOCRITUS explains this precisely; the README could too.

3. **Quickstart guide is clean** but includes "clawft ships with seven built-in LLM providers" which reads like marketing in what should be a setup guide. A quickstart should focus on the one provider the reader chose, not advertise the count.

4. **One instance of over-explanation in persistence.mdx**: "making cold boot and warm restart use the same code path" -- this is a useful insight, not slop, but it is an implementation detail that belongs in a developer guide, not a user-facing reference.

**Patterns NOT found (good):**
- No "It's worth noting that..."
- No "In order to" (uses "to" throughout)
- No "It should be noted"
- No "This powerful and innovative"
- No "leverage" / "utilize" / "facilitate"
- No filler paragraphs restating what was just said
- No generic placeholder text
- No emojis in technical docs

**Overall de-slopify score: 9/10.** The documentation reads like it was written by someone who knows the code and respects the reader's time. The only area to watch is the README, which leans slightly toward marketing copy in its section headings.

---

## Exercise 5: Sprint 11 Documentation Work Items

### Priority 1 -- Blocks Release

| # | Work Item | Effort | Owner |
|---|-----------|--------|-------|
| 1 | Rebuild CHANGELOG.md with Sprint 6-10 entries | 3h | doc-weaver |
| 2 | Fix README: broken install command, stale test count, inconsistent GitHub URLs | 1h | doc-weaver |
| 3 | Generate and host rustdoc API reference | 2h | api-docs |
| 4 | Verify `weave.toml` schema and write configuration reference | 2h | doc-weaver |

### Priority 2 -- Blocks Adoption

| # | Work Item | Effort | Owner |
|---|-----------|--------|-------|
| 5 | Add "Who This Is For" product thesis to README | 1h | doc-weaver + reviewer |
| 6 | Add AI agent onboarding section to README | 1h | doc-weaver |
| 7 | Write `weftos init` workflow documentation | 2h | doc-weaver |
| 8 | Complete feature flag guide (all 93 flags, all 20 crates) | 3h | api-docs |
| 9 | Update tool catalog (27+ tools including 10 new) | 2h | api-docs |
| 10 | Write mesh networking quickstart (2-node setup) | 2h | doc-weaver |

### Priority 3 -- Strengthens Quality

| # | Work Item | Effort | Owner |
|---|-----------|--------|-------|
| 11 | Unify Fumadocs site and standalone docs strategy | 2h | doc-weaver + researcher |
| 12 | Write ECC three-modes tutorial (Act/Analyze/Generate) | 3h | researcher |
| 13 | Create troubleshooting / FAQ page | 2h | doc-weaver |
| 14 | Externalize symposium insights into user-facing architecture docs | 4h | researcher |
| 15 | Add Fumadocs search integration (pagefind or similar) | 2h | api-docs |

### Priority 4 -- Future

| # | Work Item | Effort | Owner |
|---|-----------|--------|-------|
| 16 | Tutorial: analyze an external codebase with WeftOS | 4h | researcher |
| 17 | Tutorial: build a WeftOS application (weftapp.toml) | 3h | doc-weaver |
| 18 | Docs versioning strategy (tag-based or branch-based) | 2h | doc-weaver |
| 19 | Video/screencast for 5-minute onboarding | 4h | external |

**Total estimated effort for P1+P2**: ~17 hours.

---

## Exercise 6: ECC Contribution

### New Causal Nodes

```
[N10] Documentation Baseline Assessed
      status: ACHIEVED
      evidence: CHANGELOG audit, README audit (7/10), gap inventory, de-slopify score (9/10)

[N11] CHANGELOG Rebuild Plan
      status: DEFINED
      depends_on: N10
      artifact: structured plan with 5 steps, 134 commits to process

[N12] README Rewrite Plan
      status: DEFINED
      depends_on: N10
      artifact: 8 specific issues identified, priority-ordered fix list

[N13] Documentation Gap Inventory
      status: ACHIEVED
      evidence: 5 critical gaps, 7 significant gaps, 5 structural gaps quantified

[N14] Documentation Sprint 11 Backlog
      status: DEFINED
      depends_on: N10, N11, N12, N13
      artifact: 19 work items, 4 priority tiers, effort estimates
```

### New Causal Edges

```
N1  --[Enables]-->   N10   Kernel completion provides the subject matter for docs audit
N10 --[Produces]-->  N11   Audit findings produce the CHANGELOG rebuild plan
N10 --[Produces]-->  N12   Audit findings produce the README rewrite plan
N10 --[Produces]-->  N13   Audit produces the gap inventory
N11 --[Enables]-->   N5    CHANGELOG must be rebuilt before v0.1.0 tag
N12 --[Enables]-->   N5    README must be fixed before v0.1.0 tag
N13 --[Enables]-->   N14   Gap inventory drives the documentation backlog
N14 --[Blocks]-->    N5    Priority 1 doc items block release
```

### Causal Chain Update

```
N1 (achieved) --> N10 (achieved) --> N11 (defined) --> N5 (blocked)
                                 --> N12 (defined) --> N5 (blocked)
                                 --> N13 (achieved) --> N14 (defined) --> N5 (blocked)
```

The release tag (N5) now has three documentation prerequisites in addition to the release strategy (N3) identified in the opening plenary. All three documentation nodes are well-defined with concrete work items and effort estimates.

---

## Summary

The documentation is technically well-written (9/10 de-slopify) but structurally incomplete. The CHANGELOG has a 134-commit gap spanning 5 sprints. The README has a broken install path and stale metrics. There are zero hosted API docs, zero tutorials, and zero troubleshooting pages. The Fumadocs site has strong kernel subsystem coverage (25 WeftOS pages) but the two doc trees (Fumadocs and standalone markdown) are not unified.

The highest-leverage Sprint 11 documentation work is: (1) rebuild the CHANGELOG, (2) fix the README's broken/stale content, (3) generate rustdoc, and (4) write the `weave.toml` configuration reference. These four items total roughly 8 hours and unblock the release.
