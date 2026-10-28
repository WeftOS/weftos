# ADR-XXX: Agent Directory: agents authored in weftos, installed by `weftos init`, approved by git

- **Status**: Accepted (decisions D1–D6 recorded 2026-09-28). **Updated 2026-09-28:** AD-1, AD-2 and AD-4 implemented in weftos (`scripts/agents-*.mjs`, gate checks 17-19, `weftos init --claude|--grok|--codex`); AD-5 and AD-6 implemented on weftos-dashboard `feat/agent-directory`; AD-3 evals and AD-7 delivery in progress.
- **Date**: 2026-09-28
- **Deciders**: Workspace owner (Mathew Beane)
- **Related**: ADR-075/076 (WeftOS MCP server and profiles), ADR-096 (MetaHarness
  foundation: evaluate → receipt → promote), `docs/guides/ticket-steward-workflow.md`,
  weftos-dashboard `docs/control-board.md`
- **Source**: [`design.md`](./design.md), the private harness agent inventory (kept locally, not in this public repo),
  [`../agent-skills-design/higgsfield-format-standard.md`](../agent-skills-design/higgsfield-format-standard.md),
  [`../agent-skills-design/paper-scivis-agent-skills.md`](../agent-skills-design/paper-scivis-agent-skills.md),
  [`../agent-skills-design/image-analysis-skills-survey.md`](../agent-skills-design/image-analysis-skills-survey.md),
  [`../agent-skills-design/nexus-map.md`](../agent-skills-design/nexus-map.md),
  [`../skill-3d/weftos-adaptation-plan.md`](../skill-3d/weftos-adaptation-plan.md) §6

## Context

Our best agents (Stew, Doc/Gus, Liber, Mo and the developer, reviewer and tester
lanes) were built inside one client engagement repo. They cover Claude and Codex
but have no Grok versions, and two of them (Liber, Mo) were never committed. Each
project copies and edits agent files by hand, so the copies drift and nothing
records which version a project runs.

The weftos-dashboard is the portfolio control plane. It has project-scoped
harness credentials (`wfb_`), a source-first steward write path, and a rule that
each project owns its repo and write authority. It has no model for agents.

Three constraints shape the answer:
- Host formats differ (Claude, Grok and Codex agents, hooks and MCP
  configuration), and Codex has no native skill loader.
- Agent packages execute code, and roughly a quarter of community skills carry
  vulnerabilities.
- Whether a skill helps, and what it costs, varies by host.

## Decision

1. **Agents are authored in the public weftos repo** under `agents/<id>/` (D1).
   Each package holds `AGENT.md`, skills in the Higgsfield standard v0 layout,
   `scripts/`, optional hooks, code changes, a bundled MCP server and opt-in
   optional workflows, plus `evals/`. A `weftos-package.yaml` declares
   capabilities, requirements, config placeholders, trust tier, wrapped-tool pins
   and a `provenance` block (source registry, upstream license, upstream commit,
   verdict adopt/adapt/pattern/build). Teams live in `agents/teams/<team>/team.yaml`.
   CI generates `agents/catalog.json`.
2. **The WeftOS authoring layout is canonical** (D2). No host format is
   canonical. This settles the conflict with adaptation-plan §6.3, which made
   `.grok` canonical.
3. **`weftos init --claude | --grok | --codex` installs and adapts per host.**
   These are new flags on the existing crate command. It renders personas,
   skills, hooks, MCP configuration, the Codex `AGENTS.md` pointer lines and the
   host plugin manifests. It also writes one project context file
   (`.agents/project-context.md`) from the project's domain pack, and writes
   `.weftos/agents.lock.json`. It replaces a separate applier CLI and any change
   to the global `grok-claude-sync` helper. All three hosts ship together in the
   MVP.
4. **Git is the approval of record** (D4).
   - An agent version is approved by merging it into weftos. The gate is a
     required check with three parts:
     - validation: the Higgsfield checks, license, declared capabilities, secret
       scan, leakage lint, per-host render test, the 300-line cap and a
       skill-smell lint;
     - the leak check `scripts/agents-leak-check.sh`, required because the repo is
       public;
     - a recorded eval round on all three hosts, with score, completion rate and
       token cost kept separate.
   - A project change is approved by merging the PR that `weftos init` opened in
     that project (D5).
   - The dashboard reviews, batches and **triggers** these changes. It stores no
     separate approval decisions, and it never holds a GitHub write token.
5. **Change sets are content-addressed and risk-scored.** Delivery checks an
   expected lock digest, reusing the steward endpoint's replay and stale-revision
   semantics. Risk is scored per item and across the whole rendered set, because
   individually safe skills can combine unsafely (2606.00448). The lock records
   indirect dependencies SBOM-style (2607.01136).
6. **The project's reported lock is the source of truth for what is installed.**
   Drift, upgrade, rollback (by re-pinning or by reverting the PR) and removal
   all derive from it.
7. **Sourcing comes before building.** Authors search the registries in
   `skill-builder/references/skill-sources.md` first. Imports are weftos PRs with
   four checks: injection stripping, install pinning, license fit and brand
   removal.
8. **Client knowledge never enters weftos.** Client domain packs, such as
   `client-pack`, stay in the client repo and are referenced by
   `git_url + path + commit`. The leak check enforces this.
9. **The base team `weftos-core`** contains the lead doctrine (a skill, never a
   spawnable agent), Stew (behind a board adapter), Doc, Liber, Mo (consult
   contract only) and the developer, reviewer and tester lanes. The port rights
   are confirmed: these agents are the user's own harness (D6).
10. **The MVP** is the `agents/` standard, the leak check, the ported base team,
    the `weftos init` renderer for all three hosts, a dashboard directory that
    reads `agents/catalog.json`, and a first delivery by PR to WeftOS and then
    Shasta. The review modal and change queue follow in Phase 1.

**D3 (versioning): by weftos release.** Packages carry no version numbers of their
own. An agent's version is the weftos release it ships in; the lock file records that
release (and its commit) for each installed agent, and git history holds each agent's
change log.

## Consequences

**Positive**
- One authoring source, in the repo that already has review, CI and release
  tooling. Hosts are rendered, so Claude, Grok and Codex stop drifting apart.
- Approval uses mechanisms that already exist (merges, branch protection,
  CODEOWNERS), so the dashboard needs no approval tables and no GitHub write
  token.
- The gate turns "this skill helps" into a per-host, recorded claim, in line with
  ADR-096.
- Every package records where it came from and why it was adopted, adapted or
  built.
- Drift becomes visible, and good local edits have a path upstream.

**Negative / costs**
- Agent releases are tied to the weftos repo and its CI. If D3 chooses release
  versioning, they are also tied to its release cadence.
- A public repo raises the cost of any leak. The leak check has to stay current
  as new clients are added.
- Eval rounds are manual at first, and every agent needs Grok rounds.
- Review diffs held in Supabase are a transient copy of repo content. They are
  capped and pruned.

**Risks**
- Grok hook support (`PostToolUse`) is unverified. Pieces that depend on hooks are
  unsupported on Grok until it is proven.
- Structural merges into host config files must never rewrite keys WeftOS does not
  own. The lock records exactly which keys WeftOS owns.

## Alternatives considered

- **A separate private registry repo.** This was the design's earlier
  recommendation, rejected by D1. It kept client packs possible in the registry,
  but meant a second repo, CI and applier. The leak check plus client-repo packs
  achieve the same separation.
- **Package blobs in Supabase.** Rejected: it duplicates git review and history,
  and renders are less reproducible.
- **The dashboard writes to project repos (GitHub App).** Rejected: it would put
  project write authority in Vercel.
- **Dashboard approval tables as the record.** Rejected by D4: git already records
  approval.
- **Claude-first or Grok-canonical authoring, synced by `grok-claude-sync`.**
  Rejected by D2: one WeftOS layout with per-host rendering is simpler and covers
  Codex.
