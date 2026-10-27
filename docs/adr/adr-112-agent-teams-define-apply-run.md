# ADR-112: Agent teams: define them once, apply them to projects, and run them

- **Status**: Accepted (2026-10-07; owner decisions in §7). TM1-TM3 start now.
- **Deciders**: owner
- **Extends**: the Agent Directory decision
  ([`docs/research/agent-directory/adr-draft-agent-directory.md`](../research/agent-directory/adr-draft-agent-directory.md),
  Accepted 2026-09-28; tickets AD-1..AD-23). This ADR does not replace it; it sets the team
  format, the apply path and the runtime that the directory left open.
- **Builds on**: ADR-108 (projects installed on many machines; node actions over the
  heartbeat), ADR-109 (agent environment profiles and the local doctor), ADR-110 (transcript
  collection), ADR-090-style decoupling for anything a team writes into the kernel.
- **Inputs**: an analysis of OpenRig (github.com/mvschwarz/openrig, Apache-2.0, v0.6.6) and a
  structural analysis of the owner's own agent team (Stew, Doc/Gus, Liber, Mo, the lead
  doctrine and the build lanes), which the owner built and runs while working on a client
  engagement. The team is the owner's; nothing from the client project is reproduced here.

## Context

The owner wants to build agent teams and apply them to projects, the way OpenRig lets a
person launch a team against a repository.

**What WeftOS already has (Agent Directory, built 2026-09-28):**

- Packages in `agents/<id>/` (`AGENT.md`, skills, evals, `weftos-package.yaml` with kind,
  trust tier, capabilities, `required_project_context`, declared secrets, provenance), a team
  file (`agents/teams/weftos-core/team.yaml`: members, nicknames, positions, active flags,
  shared skills, no spawnable lead), a generated `agents/catalog.json`, gate checks and a
  leak check.
- `weftos init --claude|--grok|--codex --team <team> --plan|--apply` renders packages per
  host, with a lock file and drift detection.
- Dashboard tables and an AI org tab (AD-5, AD-6). Not built: evals on three hosts (AD-3),
  the first reviewed delivery (AD-7), change-set review and apply from the dashboard
  (AD-8..11), the team editor (AD-12), drift and upgrades (AD-13..14).

**What the owner's team shows a real team contains** (patterns only, as run on one project):

- Layers: build lanes (developer, tester, reviewer, measurer, documenter), specialist lanes
  (Stew, the steward that alone writes the board; Doc/Gus, the docs owner that alone writes
  per-card history; Liber, the memory router; Mo, a consult-only purpose advisor), plus
  project-specific domain experts; and a lead that is the main session and is deliberately
  not spawnable. Most of these are already packaged in `agents/` as the `weftos-core` team.
- **Write authority is partitioned:** every shared resource (board rows, card history files,
  production reads, code, docs) has exactly one holder; read-only roles get no edit tools.
- **Model policy** is part of the team: spawned lanes run a mid-tier model on one bounded
  task and are recycled; the steward gets a larger model; the lead stays on the top tier.
- **Edges:** producers hand staged manifests to the steward, findings to the docs owner,
  lessons to the memory router; advisors are consult-only and never block.
- **Team rules:** "the board is for humans", pickup is a claim (move and assign before
  dispatch), one worktree per lane, every result opens with lane / tree / branch / HEAD,
  refuse rather than return empty, deliberate stand-down, a handoff document read at the
  start and rewritten at the end of every session, a prompt-injection stop-the-line hold.
- **Project facts stay in the project:** one keyed `project-context.md` (gate path, board
  commands, docs root, memory store, confidentiality tiers) that agents ask to fill and that is
  never overwritten. Agents hold method; the project holds facts.
- **Memory belongs to the project, not the agent.** Liber routes lessons into the project's
  own stores; nothing an agent learned on one project travels with it.
- A separate engagement factory (intake interview → document register → workflow map →
  roster → assembly) exists for consulting work; it is out of scope here, except for its
  "spawn into an existing repo" rule: copy only what is absent and report collisions.
- **Gaps there:** no version pin or upgrade path for copied process documents, no drift
  check on hand-written lanes, Grok has only one hook.

**What OpenRig adds** (worth copying):

- It **runs** the team, not only configures it: a daemon keeps each member alive, gives it an
  identity, and lets members message each other (send, broadcast, chat rooms, queues; also as
  MCP tools), with context monitoring and restore briefs.
- A clean model: a team of pods and members, each member an agent reference with a named
  profile (skills, guidance, subagents, plugins), plus typed edges; agent specs kept separate
  from team specs; presets limited to what the author declared.
- Content-addressed bundles pinned to a commit, a pack-time secret and path-escape scan,
  refuse-on-conflict instead of overwrite, a dry-run preview, and an honest table of what it
  changes on the machine.

**What OpenRig gets wrong for us:** it writes user-global configuration (`~/.claude.json`,
`~/.codex/config.toml`, global skills, `~/.tmux.conf`); defaults to broad permissions,
including a bypass mode; has no signing and checks secrets by filename only; has no real
uninstall; supports Claude Code and Codex but not Grok.

## Decision

### 1. Team spec v1 (`agents/teams/<team>/team.yaml`)

Extend the existing file into a versioned schema, validated by the gate:

- **Members:** `agent` (package ref, pinned to a catalog version), nickname, `one_job` (no
  "and"), `model_tier`, optional tool allow-list narrowing the package's, `authority`
  (autonomous | supervised | escalated), `owner` (a named human role), `must_not` list,
  `tiers` (which confidentiality tiers the member may read), `active` and `phase`.
- **Edges:** `consumes` (producer → steward, finding → docs owner, lesson → memory router),
  `delegates_to`, `consults` (consult-only, never blocking).
- **Write-authority table:** resource → exactly one member. The gate fails on a resource with
  zero or two holders, or a holder whose tools cannot perform the write.
- **Lead:** doctrine and stand-down/handoff rules as shared skills; never a spawnable member.
- **Team rules block:** the hard rules above as named, testable rules the renderer writes into
  each host's instruction files.
- **Presets:** declared configurations only (for example "lean" vs "full", or a member's host
  swapped from Claude to Codex within the set the author allowed); anything else is refused.
- **Hosts:** Claude, Codex and Grok targets, with a tool-name map per host (Task, SendMessage,
  TodoWrite and their Grok/Codex equivalents).

Lints: no client names or paths in team or agent files (the existing leak check); agents hold
method, never project facts.

### 2. Applying a team to a project (`weftos init`, extended)

- **Project-scoped only.** Writes inside the project (host agent/skill/hook files, a managed
  block in `CLAUDE.md` / `AGENTS.md` / Grok rules). Never user-global files; anything a host
  needs globally is reported as a step for ADR-109's doctor, never written silently.
- **`project-context.md`:** the union of every member's `required_project_context` keys,
  filled from what can be detected, the rest written as open questions; never overwritten.
- **Lock:** team version, package versions and per-file SHA-256; re-applying shows a diff;
  local edits are drift.
- **Into an existing project:** copy-if-absent, refuse on conflict, and write a collision
  report (copied / skipped-identical / skipped-divergent) and a **birth report** (adapters
  bound, keys missing).
- **Uninstall:** removes exactly what the lock lists, leaving project facts.

### 3. Applying from the dashboard, over the mesh

The dashboard assigns a team (and preset) to a project. Apply is a reviewed change set
(AD-8, AD-9) delivered as an ADR-108 node action `team.apply` to the machine that holds the
workspace; the node runs the apply after the member confirms, and reports the lock, birth
report and drift on later heartbeats. The same path serves upgrades and removal. Projects
without a node use a PR, as AD-7 does today.

### 4. Running a team (`weaver team`)

WeftOS-native, opt-in, after §2 and §3 work:

- `weaver team up <team> --project <ULID>` starts the members as supervised, project-scoped
  host sessions under the user daemon, each with an identity derived from the project.
- Messaging over WeftOS's own agent bus (send, broadcast, inbox, task queue), exposed as MCP
  tools, replacing per-host shims; one bus for Claude, Codex and Grok.
- Board integration: pickup is a claim (the steward moves and assigns before work starts);
  members recycle after one bounded task; the handoff document carries continuity; session
  transcripts flow to ADR-110 collection.
- Permissions are passed per launch, never written to global settings; no bypass mode.
- `weaver team down` stops members; `weaver team ps`, **Weave Manager** (the egui binary,
  launchable from the control plane) and the dashboard show who is running, on which model,
  holding which card, and can start and stop the team.

### 5. Distribution and trust

- Team bundles are content-addressed (per-file SHA-256), pinned to a commit, and **signed**
  with the WeftOS release key (the same chain as cog repositories, COG-008).
- Pack-time scan: the existing leak check plus a real secret scanner (not filename-only),
  path-escape and symlink checks.
- Public teams live in weftos `agents/`; client teams live in that client's private
  `weftos-<slug>` repository and are never published.

### 6. The owner's team, deployable anywhere, with fresh memory

The owner's agents (Stew, Doc/Gus, Liber, Mo, the lead doctrine, developer, reviewer, tester,
documenter, measurer) are modelled as one deployable team, evolving `weftos-core` to team
spec v1. They carry **method only**:

- **No memory travels.** Applying the team copies no memory, history, card files, brain
  corpus or learned patterns from any other project. Every project starts empty.
- **Memory is built in place.** Apply creates the project's own stores and binds them in
  `project-context.md` (memory store path, card-history root, board adapter, docs root);
  Liber routes new lessons into them and Doc/Gus starts the project's card histories. ADR-110
  transcripts stay partitioned by project.
- **No project facts in agent files.** Anything project-specific found in the current copies
  (board id prefixes, tool names, quotes) becomes a `project-context` key or is dropped; the
  leak check enforces it.
- Domain experts stay per project; the team ships the template (`agents/templates/
  domain-expert`), not a filled-in expert.

## Phases

| Phase | Delivers | Done when |
|---|---|---|
| TM1 Team spec v1 | Schema, gate lints (authority table, one-job, edges, presets, leak check), `weftos-core` migrated | A seeded authority conflict, client name or undeclared preset fails the gate; `weftos-core` passes |
| TM2 Project apply | `project-context.md`, lock with per-file digests, collision and birth reports, uninstall, project-scoped only (finishes AD-4/AD-7) | Applying `weftos-core` to WeftOS and Shasta on three hosts is deterministic; a re-apply is empty; uninstall leaves only project facts |
| TM3 Apply surfaces | Team assignment and change-set review in the dashboard and in Weave Manager (egui), delivered as the `team.apply` node action (AD-8..11 with ADR-108) | A team assigned from either surface lands on the workspace machine after one confirmation, and its lock shows on the project panel |
| TM4 Owner team v1 | Stew, Doc/Gus, Liber, Mo, lead doctrine and the lanes as one team-spec-v1 team; project-specific details lifted into `project-context` keys; fresh per-project memory created and bound on apply | Applied to a new project, the team runs with empty memory stores bound in `project-context.md`, writes its first lesson and card history there, and no file carries another project's facts or memory |
| TM5 Team runtime | `weaver team up/down/ps`, agent bus with MCP tools, board claims, recycle, handoff | A three-member team runs a card end to end on Claude and Codex, with messages on the bus and the card history written by its owner |
| TM6 Bundles and signing | Signed, content-addressed bundles, commit-pinned import, scanner | An unsigned or tampered bundle is refused; an import shows provenance and digests |

## Consequences

- One team definition drives install, upgrade, drift, dashboard assignment and runtime on
  three hosts.
- Teams become a WeftOS product surface (as OpenRig is), but project-scoped and signed.
- New surface: the runtime starts processes and the bus carries instructions between agents;
  §4 keeps permissions per launch and the stop-the-line rule applies on the bus.

## 7. Owner decisions (2026-10-07)

| # | Decision |
|---|---|
| 1 | Direction accepted. Team spec v1 and project apply first (TM1-TM3), the runtime after. |
| 2 | **The runtime is WeftOS-native.** WeftOS already has the surfaces: the daemon and mesh, the dashboard control plane, and Weave Manager (egui, builds as a binary and runs from the control plane). OpenRig is a design source only. |
| 3 | **First project after WeftOS itself: the Cogs project** (`WeftOS/weftos-cogs`), now its own product extracted from WeftOS. Its extraction rules apply: changes go through its integration branch and nothing is pushed there without the owner's approval. |
| 4 | **The team is the owner's own** (Stew, Doc/Gus, Liber, Mo and the rest), not a client's. It is modelled as a deployable team that carries no memory from any project and builds new memories on each project it joins (§6). |
