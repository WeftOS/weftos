# Agent Directory for weftos-dashboard: design

Status: decisions D1–D6 recorded 2026-09-28 (§8). Nothing here is implemented.
ADR: [`adr-draft-agent-directory.md`](./adr-draft-agent-directory.md) (Accepted).
Inputs: the private harness agent inventory (kept locally, not in this public repo); in `../agent-skills-design/`: `higgsfield-format-standard.md`, `paper-scivis-agent-skills.md`, `image-analysis-skills-survey.md`, `voltagent-shortlist.md`, `nexus-map.md`; the `media-pipeline` plugin (guinacio/claude-image-gen, MIT); coreyhaines31/marketingskills (MIT).

## 0. The request and the constraints that shape it

The request: formalize the strong client-engagement agents as a WeftOS **base team**, keep
an **agent directory** (a tab on the AI org chart) that maintains teams, apply a
team or single agent to any project with everything it needs (skills, helpers,
sub-agents, hooks, code changes), and route every project change through a
**review-all-changes modal** whose changes can be queued and batched.

| Fact | Source | Consequence |
|---|---|---|
| One owner per workspace; all RLS is `owns_workspace(workspace_id)` | `supabase/migrations/202609260001_control_board.sql` | The dashboard records who triggered a change; git records who approved it (D4) |
| Machines reach data only through token-checked `security definer` RPCs granted to `anon`, wrapped by validated routes | `…0004`, `…0008`, `app/api/harness/*` | Agent endpoints reuse the `wfb_` project-scoped credential and the same route shape |
| A project owns its repo and write authority; Vercel stores no source-board key | `docs/control-board.md` | The dashboard holds no GitHub write token. Changes land as a PR opened from the project's own checkout |
| The steward path is conditional and idempotent (action key, expected revision, replay) | `202609270002_harness_steward_action.sql` | Change sets are content-addressed; delivery checks an expected lock digest |
| `ticket_events` is the trigger-written audit trail; `app/page.tsx` holds state and views are small components; Next.js 16 docs must be read first | `…0001`, `…0004`, `AGENTS.md`, dashboard at `fae5013` | `agent_events` mirrors it; new views are new components with their own data hook; UI tickets start by reading `node_modules/next/dist/docs/` |
| Host formats differ: Claude `.claude/agents/*.md`, Grok `.grok/agents/*.md` (+ `prompt_mode`, `permission_mode`), Codex `.codex/agents/*.toml`; hooks in three different files | weftos repo; `skill-3d/weftos-adaptation-plan.md` §6 | Author once in the WeftOS layout; `weftos init --claude \| --grok \| --codex` renders each host (D1, D2) |
| `weftos init` already exists (creates `weave.toml` and `.weftos/`) | `crates/weftos/src/main.rs`, `crates/weftos/src/init.rs` | Host flags extend it; no separate applier CLI |
| Codex has no native skill loader; the working pattern copies `SKILL.md` into the project and adds pointer lines to `AGENTS.md` | SciVis read §6(b)3 | The Codex renderer generates that step |
| The weftos repo is public | D1 | A leak check gates every agent version; client content never enters `agents/` |
| About 26% of community skills are vulnerable; focused skills beat bundles; skill benefit and token cost vary by host | `skills-memory.md` refs 78, 30; SciVis Tables 1, 3 | Trust tiers, risk flags, and a per-host eval gate before a version can join a team |

## 1. Domain model

Two bounded contexts: **Directory** (catalog, teams) and **Delivery**
(assignments, change sets, triggers). The board context is unchanged.

```
Agent 1──* AgentPackageVersion (agents/<id>/ at a weftos commit)
Team  1──* TeamMember(position, agent, pin) ; Team = agents/teams/<team>/team.yaml + shared skills
Project 1──* ProjectAssignment(team | agent, pin, hosts, overrides)
ProjectAssignment 1──* ChangeSet 1──* ChangeSetItem(path, host, piece, op, diff, risk, drift, included)
ChangeSet *──1 PR (the approval of record) ; Project 1──1 ProjectAgentLock ; AgentEvent (audit)
```

- **Agent** (identity only): slug, nickname, aliases ("Gus" for Doc), role, `kind`
  (`lead_doctrine`/`specialist`/`lane`/`template`), `reports_to`, trust tier
  (`core`/`internal`/`client`/`external`), origin project, and the source client's status
  axes: build (`built`/`partial`/`buildable`/`future`) × integration (`live`/`pending`/`illustrative`).
- **AgentPackageVersion**: `agents/<id>/` at a weftos commit, with digest,
  license, per-host coverage (`native`/`rendered`/`unsupported`), gate status and
  eval receipt (§1.2). Its version is the weftos release it ships in (D3).
- **Pieces** (`persona`, `skill`, `subagent`, `hook`, `script`, `mcp_server`,
  `optional_workflow`, `code_change`, `config` with secret *placeholders* only,
  `domain_pack`), each with `hosts`, `requires` and `capabilities` (`executes_code`,
  `network_egress[]`, `reads_secrets[]`, `writes_outside_host_dirs`, `permission_change`).
- **Team**: ordered members with pins, one lead, shared skills; no two members
  own the same rendered path; a bounded active cap (8) with retirement of
  never-exercised members, against library drift (2605.19576).
- **ProjectAssignment**: "project P runs team T at pin X on hosts H", with
  overrides (excluded pieces, domain packs, enabled optional workflows, bindings
  such as the gate command); `draft → preview → triggered → pr_open → installed |
  closed | superseded | removed`.
- **ChangeSet / ChangeSetItem**: one assignment rendered against one project's
  lock, grouped by host and piece; items are `create`/`update`/`delete`/`merge`
  file operations with before/after hashes, capped diff, risk flags, drift, and an
  `included` flag the reviewer sets before triggering.
- **Approval of record** (D4): for an agent version, its merge into weftos; for a
  project, the merge of the PR that `weftos init` opened. The dashboard stores the
  PR URL and merge result, not a separate approval.
- **ProjectAgentLock**: the project's `.weftos/agents.lock.json` as reported by
  its harness; what is really installed. The dashboard never writes it.

### 1.1 Where content lives: `agents/` in the weftos repo

All package content lives in the **public weftos repo** under `agents/<id>/`
(D1). The WeftOS authoring layout is canonical; no host format is (D2). Supabase
holds only **metadata and pointers**: identity, commit, digest, the parsed
manifest (≤ 64 KiB), risk profile, gate status and eval receipt. Review diffs are
stored transiently (capped) and pruned 30 days after the PR closes.

```
weftos/agents/
  catalog.json                              # generated by CI from every dir with a weftos-package.yaml
  stew/                                     # one package per agent
    AGENT.md                                # persona, WeftOS layout (canonical)
    weftos-package.yaml                     # capabilities, requires, hosts, trust, pins, mcp, workflows, provenance
    skills/board-steward/SKILL.md  references/  scripts/  agents/openai.yaml
    scripts/  hooks/  code/                 # helpers; hook specs; patches or pinned generators
    mcp/                                    # bundled MCP server: source, built artifact, version-consistency test
    optional-workflows/<name>/SKILL.md      # opt-in only; off unless the assignment enables it
    context/project-context.template.md     # fields this agent reads from the project context file (§2.2)
    evals/scenarios.md  evals/receipts/<commit>.json  CHANGELOG.md  LICENSE
  teams/weftos-core/team.yaml
  packs/episteme/ …                         # non-confidential domain packs only
```

Legacy files in `agents/` (`agents/weftos/*.md`, `agents/code-reviewer/agent.toml`)
stay; only directories with a `weftos-package.yaml` enter the catalog.

Each `SKILL.md` follows Higgsfield standard v0 (`name` = folder; `description` with
`Use when:`/`Chain with:`/`NOT for:`; `argument-hint`, `allowed-tools`; body < 300
lines, detail in `references/`; self-contained; two-tier errors; real commands).
Host plugin manifests (`.claude-plugin/`, `.codex-plugin/`, `marketplace.json`)
are **rendered by `weftos init`**, not authored.

`weftos-package.yaml` carries what Higgsfield lacks: capabilities, code changes,
config placeholders, trust tier, wrapped-tool version pins (SciVis),
`mcp_servers[]` (bundled path, command, pinned artifact digest, egress),
`optional_workflows[]` (name, requirements, egress, default `off`), and a
**`provenance`** block: `{source_registry, source_url, upstream_license,
upstream_commit, adopted_at, verdict: adopt | adapt | pattern | build}`.

Why git: prompts are code (diffs, review, revert); every consumer reads from disk;
the dashboard points at sources, as for subscriptions; `(commit, digest)` pins a
render; and the merge is the approval (D4). The repo is public, so **client packs
never enter it**: they stay in the client repo (`git_url + path + commit`),
enforced by the leak check (§1.2).

**Sourcing first.** Before building, the author searches the registries in
`~/.claude/skills/skill-builder/references/skill-sources.md` (skills.sh,
VoltAgent, awesomeskill.ai, anthropics/skills, openai/skills and others) and
records the result as `provenance.verdict`. **Import from a registry** is a PR to
weftos, reviewed in the same modal. It has extra review checks: **injection
stripping** (hidden or HTML-comment instructions, self-citation directives,
over-broad proactive triggers such as "ALWAYS invoke … IMMEDIATELY", fetch at
install time); **install pinning** (upstream commit SHA, no `@latest`, hashed
dependencies, digest-pinned MCP artifacts); **license fit** (MIT or Apache
vendored; CC-BY-SA reimplemented; GPL or no license = `pattern` only); and brand
assets removed.

### 1.2 The version gate: CI validation, leak check and eval scenarios

An agent version is approved by **merging it into weftos** (D4); the gate is a
required check, and the catalog marks `gate_status = passed` only from green CI on
the merged commit. Three parts:

**1. Validation** (a step in `scripts/build.sh gate` and a CI workflow). It runs
the five Higgsfield checks (frontmatter validity; version sync where versions
exist, see D3; the team file lists every member; every `references/` link
resolves with no orphans; no `../` paths). WeftOS adds: a license is present;
every hook, script, MCP server or code change declares its capabilities; a secret
scan; a benchmark-leakage lint (no eval expected output in a skill); a render
smoke test per host; the 300-line cap; and a skill-smell lint (2607.01456 found
smells in >99% of real `SKILL.md` files).

**2. Leak check** (`scripts/agents-leak-check.sh`, local and CI): any client name,
engagement id, client path, roster entry, meeting text or credential shape in
`agents/` fails the gate, because the repo is public.

**3. Eval round.** `evals/scenarios.md` (Higgsfield's manual protocol: request →
expected behavior → Pass/Partial/Fail) runs on **all three hosts** (Claude, Grok,
Codex); the receipt records score, completion rate and token cost **per host** as
separate axes (SciVis §4). A >15% score drop or >2× time/tokens blocks the gate; a
host-specific regression passes with a **host-scoped warning** shown in the
modal. From Phase 2 the paired with/without × host × n≥3 recipe replaces the
manual round where a deterministic floor exists.

## 2. Apply mechanism

### 2.1 Flow

```
Dashboard (owner)              Project harness / human (project checkout, wfb_)      Project repo
assign team → draft ──GET /api/harness/agents/assignments──►
                               weftos init --plan --claude --grok --codex
                               (render, diff, risk, drift against the lock)
        ◄──POST /api/harness/agents/changesets (preview + capped diffs)──
review, include/exclude,
batch → Trigger ──GET /api/harness/agents/triggered──►
                               weftos init --apply (included items only) on branch
                               agents/<team>-<digest8>; run project gate; open PR ──►  PR = approval
        ◄──POST …/changesets/:id/delivery {pr_url, head_sha, expected_lock_digest}──       of record
        ◄──POST /api/harness/agents/report {lock, drift} (after merge, and on heartbeat)──
```

The dashboard **reviews, batches and triggers**; the project PR, under its own
review, CI and branch protection, is the **approval of record** (D4, D5). MVP: a
human runs `weftos init --claude --grok --codex` and opens the PR. Later the
project harness runs plan and apply on the heartbeat loop.

### 2.2 Rendering: `weftos init --claude | --grok | --codex`

The renderer is a module in the `weftos` crate (`crates/weftos`), reached by new
host flags on the existing `weftos init`. Flags combine, and the MVP ships all
three hosts at once. `render(agents@commit, team, overrides, host) → [{path,
bytes, merge}]` is pure: no clock, no network, sorted output. `--plan` prints or
uploads the change set without writing; `--apply` writes files and the lock.
Where the agent sources come from (embedded in the release binary, or fetched at
`--from <weftos ref>`) depends on D3.

- **Claude**: persona → `.claude/agents/<slug>.md`; skills → `.claude/skills/`
  verbatim; hooks → merge into `.claude/settings.json`; scripts →
  `.claude/helpers/<pkg>/`; MCP → `.mcp.json` (`weft` on PATH, per WEFT-684);
  optional `.claude-plugin/` and `marketplace.json` for the team.
- **Grok**: skills verbatim to `.grok/skills/`; persona gets Grok frontmatter and
  host-specific coordination wording (Task/SendMessage ↔ `spawn_subagent`/team
  bus); hooks → `.grok/hooks/<pkg>.json`. `PostToolUse` is unverified, so
  hook-dependent pieces are `unsupported` on Grok until proven.
- **Codex**: persona → `.codex/agents/<slug>.toml`; skills → project skills dir
  **plus generated `AGENTS.md` pointer lines** (no native loader);
  `agents/openai.yaml` → plugin `interface`; hooks → `.codex/hooks.json`
  (trust-hashed, so any hook change is high risk).
- **Merged files** (`settings.json`, `.mcp.json`, `config.toml`, `hooks.json`) are
  edited structurally; the lock records the keys WeftOS owns, and others are never
  rewritten. **Bundled MCP servers** point at the digest-pinned artifact;
  **optional workflows** render only when the assignment enables them.
- **Code changes**: patches apply with a 3-way merge; pinned generators run in a
  temporary worktree and their diff is captured. Review never runs code.
- **Project context file** (marketingskills pattern): init writes one
  `.agents/project-context.md` (fallback `.claude/project-context.md`) from the
  project's domain pack and bindings. Every team agent reads it first and asks
  only for what is missing, so client knowledge lives in the project, never in an
  agent. Later edits to it are drift, usually resolved "keep local".

This replaces both the separate applier CLI and the planned `render`/`--codex`
extension of the global `grok-claude-sync.cjs` helper, and it settles
adaptation-plan §6.3: neither `.grok` nor Claude is canonical for agents; the
WeftOS layout is, and `weftos init` adapts it per host (D2).

### 2.3 Idempotency and the lock

`.weftos/agents.lock.json` records the schema, weftos commit, assignment, team,
each agent's `{commit, digest}`, each managed file's `{sha256, piece, host}`, a
transitive SBOM-style inventory of every skill, script and MCP artifact pulled in
via `requires` (2607.01136), owned merge keys, excluded pieces, local overrides,
and the renderer version.

- Digest = SHA-256(from-lock digest, to-lock digest, items); resubmitting it
  returns the existing row (steward `action_key` replay). Equal locks render empty.
- Delivery carries `expected_lock_digest`; a moved lock returns `stale_lock` and
  the harness re-plans (steward `stale_revision`).
- Excluded pieces are kept in the lock and shown as "previously excluded", never
  silently re-proposed.

### 2.4 Drift

Drift is a managed file whose hash differs from the lock, a missing managed file,
or a changed owned merge key. `report` includes drift, and items that would
overwrite drift are high risk and shown as a three-way diff (weftos old → local →
weftos new). The reviewer can **overwrite**; **keep local** (records a local
override, later renders skip that path, and the directory shows the project as
"forked"); or **upstream it** (a board ticket with the local diff as evidence, so
a good local edit becomes a weftos PR, following Doc's "graft" doctrine).

### 2.5 Upgrade, rollback, removal

- **Upgrade**: a newly merged, gated version marks compatible assignments "update
  available"; the re-plan shows only the delta, CHANGELOG and host-scoped eval
  warnings. Breaking changes never auto-queue.
- **Rollback**: re-pin and re-plan as an ordinary change, or revert the project
  PR; state follows the next reported lock, never the reverse.
- **Removal**: an empty target turns owned files into deletes and removes owned
  merge keys; drifted files become "keep or delete" choices; shared team pieces
  stay until no member needs them.

## 3. Review and trigger UX

### 3.1 Review modal (`components/ChangeSetReview.tsx`, `DiffPane.tsx`)

A `<dialog className="edit-modal wide">` in the `CompanyModal` pattern. **Header**:
target project (or weftos, for imports and team edits), team/agent, commit
from → to, risk, counts, drift, gate status, host-scoped eval warnings,
provenance verdict, and the PR link once opened. **Left rail**: items by
**piece**, then host (`Stew › persona › Claude | Grok | Codex`), with op, risk
chips and include state. **Main pane**: unified diff (three-way when drifted),
declared capabilities, and the manifest excerpt; host variants of one piece can
be diffed against each other.

| Risk flag | Trigger | Level |
|---|---|---|
| `executes_code` | hook, script, generator, MCP server command | high |
| `code_change` | path outside `.claude/ .grok/ .codex/ .weftos/ .agents/ skills/ AGENTS.md` | high |
| `secrets_required` | `reads_secrets` is not empty (names shown, never values) | high |
| `permission_change` | allow lists, `permission_mode`, `sandbox_mode`, Codex hook trust | high |
| `overwrites_local_edit` | drift on the target | high |
| `trust_tier` below `internal` | `client` or `external` packages | high |
| `composition` | set-level, over the whole rendered team: e.g. one piece reads secrets and another has egress, or a proactive trigger reaches `executes_code`; individually safe skills can compose unsafely (2606.00448) | high |
| `network_egress` | declared egress or an MCP URL | medium (high if not allow-listed) |
| `delete` | removal of a managed file | medium |
| `proactive_trigger`, `optional_workflow_enabled` | description that tells the host to auto-invoke; an opt-in workflow turned on | medium |
| none | persona, skill text, references, docs | low |

Per item: include or exclude, with a note (required to include high risk; keys
`j/k/i/x`). Footer: **Include all low-risk**, **Trigger PR** (disabled until every
high-risk item is decided). Triggering is not approval; the project PR merge is.

### 3.2 Change queue (`components/ChangeQueueView.tsx`)

The queue lists every `preview` and `pr_open` change set across projects,
filterable by project, team, agent, risk, drift and age, with row selection.
Actions: open one in the modal; **batch-trigger low-risk** (triggers change sets
whose included items are all low risk, grouped under one `trigger_batch_id`);
and **collapse identical pieces** (the same `stew` persona change across five
projects appears once as "applies to 5 projects", is reviewed once, and fans out).
PR state (open, merged, closed) comes from `delivery` and `report`.

**Audit trail**: `agent_events(workspace_id, subject_kind, subject_id, actor_id,
actor_label, kind, body, ref jsonb, created_at)` mirrors `ticket_events`. Kinds:
`catalog_synced, gate_passed, assigned, planned, reviewed, triggered, pr_opened,
pr_merged, pr_closed, installed, drift_detected, upgraded, rolled_back, removed`.
Triggers write it, and harness RPCs set `app.harness_actor` so machine events
carry the credential label.

**Who approves**: git (D4). Each repo's branch protection and CODEOWNERS decide
who may merge; the weftos repo does so for agent versions, each project for its
PR. The dashboard records who triggered. A harness credential never triggers.

## 4. UI

Each view is one component under about 150 lines. Data loads in
`lib/useAgentDirectory.ts`, a single `Promise.all` like `refresh()`, so
`app/page.tsx` gains only `View` values and one render line per view.

| View | Component | Contents |
|---|---|---|
| Nav "AI org" (`agents`), with tabs like `CompaniesView` | `AgentOrgView.tsx` | **Org chart** tab: the human lead, then the lead doctrine, then members by `reports_to`, as a nested CSS grid (no chart library). Each card shows nickname, role, commit, host dots, build × integration status, project count and drift badge. Tabs: **Directory**, **Teams** |
| Directory | `AgentDirectory.tsx`, `AgentDetail.tsx` (drawer like `TicketDrawer`) | Filters: kind, host, trust, gate. Detail: persona, pieces with risk chips, history with CHANGELOG and per-host eval receipts, projects with installed vs pinned and drift, `agent_events`. The roster comes **only from `agents/catalog.json` in weftos** and is never hand-edited (the client-engagement roster-from-scan rule) |
| Team editor | `TeamEditor.tsx` (modal) | Lead, ordered members (gated versions only), shared skills, path-conflict check. Saving produces a `team.yaml` change that a WeftOS harness opens as a weftos PR; the dashboard keeps the draft |
| Project Agents panel | `ProjectAgentsPanel.tsx`, in the board header when one project is filtered | Team, installed vs pinned per agent, drift, pending change sets and PRs; actions: assign team, add agent, upgrade, remove, re-plan |
| Changes (nav, with a count badge like Companies) | `ChangeQueueView.tsx` | §3.2 |
| Review modal | `ChangeSetReview.tsx`, `DiffPane.tsx` | §3.1 |

The overview gains a "Pending changes" `Stat` tile.

## 5. Data model and migrations

Migration `2026MMDDxxxx_agent_directory.sql`, in the style of `tickets` (uuid PKs,
cascading `workspace_id` FK, length and enum checks).

| Table | Key columns and checks |
|---|---|
| `agents` | `slug ^[a-z0-9][a-z0-9-]*$` unique per workspace; `nickname` 1–60; `aliases text[]`; `role` ≤ 240; `kind` enum; `reports_to` self-FK; `trust_tier` enum; `build_status`, `integration_state` enums; `origin_project_id` |
| `agent_versions` | `agent_id`; `commit_sha ^[0-9a-f]{40}$`; `version` the weftos release tag (D3); `digest ^[a-f0-9]{64}$`; `manifest jsonb ≤ 64 KiB`; `license` not null; `hosts jsonb`; `risk_flags text[]`; `provenance jsonb` (verdict enum checked); `gate_status in (pending, passed, failed)`; `eval_receipt jsonb ≤ 16 KiB`; `yanked_at`; unique `(agent_id, digest)` |
| `agent_teams`, `agent_team_members` | team `slug`, `commit_sha`, `lead_agent_id`, `shared_pieces jsonb`; member `(team_id, agent_id)` PK, `position`, `pin` |
| `project_agent_assignments` | `project_id` (cascade); exactly one of `team_id`/`agent_id` (check); `pin`; `hosts text[]` (subset of claude, grok, codex); `overrides jsonb`; `state` enum; `created_by` |
| `agent_changesets` | `assignment_id`; `digest` unique per workspace; `from_lock_digest`, `to_lock_digest`; `renderer_version`; `status in (preview, triggered, pr_open, merged, closed, stale, superseded)`; `risk_level`; `triggered_by`, `triggered_at`, `trigger_batch_id uuid`; `pr_url ^https://`; `head_sha`; `submitted_by_label` |
| `agent_changeset_items` | `path` ≤ 400 and `!~ '(^/\|\.\./)'`; `host in (claude, grok, codex, shared, project)`; `piece`; `op` enum; `before_sha256`, `after_sha256`; `diff` ≤ 64 KiB; `risk_flags text[]`; `drift in (none, local_edit, missing)`; `included boolean`; `review_note` ≤ 4000 |
| `project_agent_locks` | `project_id` PK; `lock jsonb ≤ 256 KiB`; `lock_digest`; `drift jsonb`; `reported_by_label`; `reported_at` |
| `agent_events` | §3.2 audit trail |

**RLS**, consistent with the existing migrations: directory tables get an owner
`for all` policy on `owns_workspace(workspace_id)`. Change sets, locks and events
are **select-only** for the owner (as `ticket_steward_actions` is); only RPCs and
triggers write them. Items allow an owner update of `included` and `review_note`
only while the parent is `preview` (the `event_insert` pattern), and an owner RPC
`trigger_changesets(ids[])` sets `triggered_by = (select auth.uid())`. Integrity
triggers in the style of `check_harness_credential_project` enforce one workspace
per row graph, no `reports_to` cycles, and a `passed` pinned version for every
team member. Indexes cover `(workspace_id, status)` and `project_id`.

**Harness RPCs** are `security definer` with `set search_path`. They check the
`wfb_` regex, look up the SHA-256 digest, and take the project scope from the
credential. Each gets `revoke … from public, authenticated; grant … to anon` and
is wrapped under `app/api/harness/agents/` with size caps and validation like
`steward/route.ts`. **Workspace-scoped credentials are refused**.

| Route | RPC | Access | Purpose |
|---|---|---|---|
| `GET /api/harness/agents/assignments` | `harness_agent_assignments` | read | Assignments for the project, with pin, weftos commit and overrides |
| `POST /api/harness/agents/changesets` | `harness_submit_changeset` | write | Preview ≤ 1 MB and ≤ 500 items; replay by digest; `stale` if `from_lock_digest` differs from the reported lock |
| `GET /api/harness/agents/triggered` | `harness_triggered_changesets` | read | Triggered change sets with their included item ids |
| `POST /api/harness/agents/changesets/:id/delivery` | `harness_changeset_delivery` | write | PR URL, head SHA, PR state, `expected_lock_digest` |
| `POST /api/harness/agents/report` | `harness_agent_report` | write | Lock and drift; moves the assignment to `installed` when the lock digest equals `to_lock_digest` |

**Catalog sync** (owner action) fetches `agents/catalog.json` from public weftos at
a chosen commit (no token), validates it, and upserts agents, versions and teams.

## 6. Base team proposal: `weftos-core`

From the client-engagement inventory (verdict `adapt`); port rights confirmed (D6). An
agent-porter is creating them under `weftos/agents/<id>/`. The client engagement had no Grok
versions, and **Liber and Mo existed only on disk**, so both need capture first.

| Member | Source | Generalization work | What stays in the client pack |
|---|---|---|---|
| **Lead doctrine** (shared skill `lead-doctrine`, not a spawnable agent) | `client-lead` | Lift the "five ways the lead seat is wrong" and the trustworthy-lane rules into a skill loaded by the main session; keep the rule that the lead is never spawned | Handoff paths, persona names |
| **Stew** (`board-steward`) | `product-board-steward` | Add a **board-adapter interface** (`scripts/board-adapter.mjs`) with a dashboard adapter first, over `scripts/dashboard-board.mjs` and `/api/harness/steward`. Keep the intake contract as a script, dedupe against live state, dry-run → shown plan → confirm → apply, the disposition report, and the before/after row hashing that caught the body-doubling defect | Intake manifest, card schema, people roster, board-sync script, the client ADRs |
| **Doc** (alias Gus; `doc-gardener`) | `doc-gardener` | Port near verbatim. Paths (ADR dir, docs root, brain) become config bindings, and `citations.mjs` becomes a package script. Keep Path A/B, the queue, graft/prune/plant/correct, retraction rules and the flywheel mode **`off` → `shadow` → `on`**, shipped `off` | `.brain/` paths, the client ADRs, `docs/gates/` specifics |
| **Liber** (`liber`) | `liber` | Make the destination table a per-project binding (for WeftOS: `docs/brain`, `MEMORY.md` file memory, ruvector namespaces, AgentDB). Keep the four marks (packages, provenance, carryability, durability) and the refusal/delegation rules. Abscission and downward flow are out | Client-deal refusal class, package registry |
| **Mo** (`mo`) | `mo` | Only the consult/read-back contract (`MO READ-BACK`: purpose, voice, findings) and the non-blocking stand-down. **Darwin is excluded** because it is unbuilt | Card-shape gauge path |
| **Developer / Reviewer / Tester** lanes | `client-{developer,reviewer,tester}` | Rename to `developer`, `reviewer`, `tester`. The gate command is a binding (`scripts/build.sh gate` for WeftOS). Keep structural tool separation: the reviewer has no edit tools, and the tester plants the defect first. CodeGraph MCP is an optional piece | Playwright/prod-DB ban wording, gate-runner path |

Phase 2: **Documenter** (per-PR lane for Doc Path A), **Measurer** (read-only, per-project MCP).

**Directory templates** (`kind: template`, not team members): `domain-expert`
(grounds answers in a repo-owned cited brain, grades them documented, inferred
or unknown, keeps content out of the agent file; from `client-domain-expert` and
`proforma-domain-expert`) and `granted-system-reader` (read-only transport wall
that turns write requests into findings for a named owner; from `smartsheet-expert`).

**Image-analysis skill candidates**: none enter the base team.
`geolocate-from-pixels` (MIT) is adapted into an **Urth pack** for `world-builder`,
keeping its confidence grades, mandatory radius and doxxing gate. `omero-integration`,
`pathml` and `pydicom` come only from the canonical K-Dense-AI source, in the
**Episteme pack** (the `davila7` copies drop pathml's PHI/consent boundary).
`ctf-osint` is a pattern only. `processing-computer-vision-tasks`, `fal-vision` and
`qwencloud-vision` go in `agents/REJECTED.md` with reasons (non-functional; a dead
pointer that fetches code at install time; a paid dependency the local Qwen3-VL,
Grounding DINO and DA3 stack already covers).

**Client domain pack** (`client-pack`, trust `client`) stays in the client
repo, referenced by `git_url + path + commit`, and assigned only to that client; the
leak check keeps it out of public weftos. Its core is the **Client project context
file** (brain paths, board schema, roster pointers, ADR list) that Stew, Doc and
Liber read, plus `client-domain-expert`, `proforma-domain-expert`,
`smartsheet-expert` and the measurer's MCP bindings. **Excluded**:
`client-roadmap-architect` (third-party IP), `weftos-core` (ours). Future scope: a
**marketing team** from coreyhaines31/marketingskills (45 MIT skills with evals).

## 7. Phased build plan

Tickets go on the dashboard board (`scripts/dashboard-board.mjs create …`) citing this document.

**MVP (Phase 0): the directory, one team, and manual apply through a PR, on all three hosts.**

| Ticket | Deliverable | Acceptance criteria | Depends on |
|---|---|---|---|
| AD-1 | `agents/` package standard in weftos: `weftos-package.yaml` schema, `team.yaml` schema, `agents/catalog.json` generator, validation step in `scripts/build.sh gate` and CI | Each seeded violation (name mismatch, orphan reference, `../`, missing license, undeclared hook capability, a >300-line SKILL) fails the gate; a clean package appears in `catalog.json`; legacy files in `agents/` are ignored | — |
| AD-2 | Leak check `scripts/agents-leak-check.sh` wired into the gate and CI | Seeded client name, client path, roster entry and credential shape each fail it; the ported base team passes | AD-1 |
| AD-3 | Port the lead doctrine, Stew, Doc and the three lanes (and capture Liber and Mo) into `agents/<id>/` with `evals/scenarios.md` (≥ 3 per member) | Gate and leak check green; eval receipts on **Claude, Grok and Codex**; merged into weftos | AD-1, AD-2 |
| AD-4 | `weftos init --claude \| --grok \| --codex` renderer in `crates/weftos` with `--plan`, the lock file and drift detection | One run renders all three hosts; output is deterministic; a second run is empty; a local edit is reported as drift; `scripts/build.sh test` and `clippy` pass | AD-1 |
| AD-5 | Dashboard migration: `agents`, `agent_versions`, teams, assignments, `agent_events`, `project_agent_locks`, `harness_agent_report` | RLS tests: another user and anon read nothing; a workspace-scoped `wfb_` is refused; tsc, lint and build pass | read `node_modules/next/dist/docs/` |
| AD-6 | UI: AI org tab (org chart, directory, teams read-only) reading `agents/catalog.json` from weftos; Project Agents panel (read) | Sync shows every catalog entry and no hand-entered rows; installed commits and drift appear after `report` | AD-3, AD-5 |
| AD-7 | First delivery: `weftos-core` → WeftOS itself, then Shasta, through `weftos init` and a PR, all three hosts | PR merged after project CI; the dashboard shows `installed` from the lock; reverting the PR is reflected on the next report | AD-4, AD-6 |

| Phase | Ticket: deliverable (acceptance) | Depends on |
|---|---|---|
| 1 | AD-8: change-set preview tables and RPCs (replay by digest; `stale_lock` on a moved lock) | AD-5 |
| 1 | AD-9: review modal (include/exclude; Trigger blocked until every high-risk item is decided) | AD-8 |
| 1 | AD-10: harness trigger mode (`weftos init --apply` of included items only; PR state reported) | AD-4, AD-9 |
| 1 | AD-11: change queue (low-risk batch trigger, identical-piece collapse, batch audit row) | AD-9 |
| 1 | AD-12: team editor (drafts; weftos PR; gated versions only); AD-12b: import from a registry as a weftos PR (provenance, injection-stripping, install-pinning, license checks) | AD-6, AD-9 |
| 2 | AD-13 three-way drift view, keep-local, upstream-it; AD-14 upgrade notices and removal | AD-10 |
| 2 | AD-15 Liber and Mo v1.0 with WeftOS bindings; AD-16 Documenter and Measurer | AD-3 |
| 2 | AD-17: paired eval harness (with/without × host × n≥3; token and completion axes) feeding `eval_receipt` | AD-1 |
| 2 | AD-18 scheduled plan/apply from the project harness; AD-19 verify Grok `PostToolUse` (unblocks Grok hooks) | AD-10 |
| 3 | AD-20 Episteme pack; AD-21 Urth pack; AD-22 `client-pack` in the client repo; AD-23 CODEOWNERS for `agents/` | AD-17 |

## 8. Decisions (recorded 2026-09-28)

| # | Decision | Effect on this design |
|---|---|---|
| D1 | The registry is `agents/` in the public weftos repo; install is `weftos init --claude \| --grok \| --codex` | §1.1, §2.2; no separate repo, no separate applier CLI, no change to the global `grok-claude-sync` helper; leak check added to the gate |
| D2 | Author in weftos; `weftos init` adapts per host | The WeftOS layout is canonical; host manifests are rendered. This resolves the conflict with adaptation-plan §6.3 (which made `.grok` canonical) |
| D3 | Version by weftos release | No per-package version numbers; the lock records the weftos release and commit per agent; `agent_versions.version` holds the release tag |
| D4 | "Published in repo is approval"; git records it | Merging into weftos approves an agent version; merging the project PR approves a project change. The dashboard reviews, batches and triggers; decision and batch tables removed |
| D5 | PR-based delivery | Every project change is a PR from the project's own checkout |
| D6 | Port rights confirmed; the agents are the user's own harness | AD-3 proceeds; client content still stays in `client-pack` |

**D3 in plain terms** (three lines):
1. A "package" is one agent's folder, `agents/<id>/`: its persona, skills, scripts, hooks, MCP servers and evals, plus `weftos-package.yaml` saying what it needs.
2. D3 asks whether each package gets its own version number (Stew can move to 1.3 while Doc stays at 1.0) or everything is versioned by the weftos commit or release it shipped in.
3. Decided 2026-09-28: by weftos release. `weftos init` installs the agent set from the release in use.
