# AI org page design (draft 2, 2026-10-11)

Status: DRAFT, not committed. Design only; no dashboard code changes.
Owner direction: "AI org is complicated, it needs to handle both OS level agents but then
agents as groups, and agents installed as packages etc into environments."
Builds on: `docs/plans/ai-org-model.md` (aiorg-architect's domain model),
`docs/plans/dashboard-os-redesign-2026-10.md`, ADR-112 (teams), ADR-114 §5a (`weftos://` names
for first-class objects), ADR-120 (exposures; harnesses are exposures), and the shipped Agent
Directory (`components/AgentOrgView*`, `lib/agentCatalog.ts`, `project_agent_assignments`,
`agent_changesets`).

## 1. The decision that shapes the page

The model has one spine: **definition -> installation (into an environment) -> instance
(running)**, and teams mirror it (team definition -> team installation -> team run). An OS
agent is **not** a separate species: it is an installation whose source is the WeftOS release
and whose environment is a node daemon (scope = system).

So the owner's three cases are two facets on one spine, not three object types:

| Owner's phrase | In the model | Facet |
|---|---|---|
| OS level agents | installation, scope = system (node daemon) | **Scope: System** |
| agents as groups | team definition / installation / run | **Shape: Team** |
| agents installed as packages into environments | installation, scope = project workspace | **Scope: Project** |

**The page is organised by lifecycle stage, not by kind.** Tabs are Catalog (what exists),
Installed (what is where), Running (what is alive), Org (how it is structured). Shape (agent vs
team) and scope (system vs project) are badges on every row. This avoids three overlapping
lists and puts an OS agent and a project agent in one table, told apart by a scope badge. The
cost: the owner's own words ("OS agents") become a filter chip, not a tab.

## 2. What exists today (and why it is not enough)

`AgentOrgView` has three tabs (Org chart, Directory, Teams) over one catalog source, i.e. all
definitions. It shows nothing about where anything is installed or running. Installation data
exists in the dashboard DB (`project_agent_assignments`: desired; `agent_changesets`: proposed
change) but no view shows it. The sidebar link also throws the user out of the OS (shell lane
fixes that).

Data gaps the design works around (from the architect):

- The dashboard types expect an old catalog shape (`slug`, `reports_to`, `build_status`,
  per-agent version, kind `lead_doctrine`); the live catalog has `id`, `name`, `nickname`,
  kind `skill|template|specialist|lane`, `hosts[]`, one top-level version, no `reports_to`,
  no per-agent digest.
- `agent_team_members` has only position and pin, none of the slot fields (one_job, model_tier,
  authority, owner, must_not).
- No instance table, no OS-agent concept; assignments are project-only; the hosts check
  excludes weftos-native.
- Nothing in weftos calls `/api/harness/agents/*`; the `team.apply` node action is unbuilt;
  the daemon report has no agents section.
- `reports_to` is gone: structure is team edges (`delegates_to`, `consumes`, `consults`).

Consequence: v1 = Catalog works after adapting the catalog type; Installed shows desired state
from assignments; Running and System-scope rows are empty-but-honest until the node report
carries agents. Section 10 sequences this.

## 3. Information architecture

Tabs on `/ai-org`: **Overview | Running | Installed | Catalog | Org**. Each is a route.

| Route | Shows | weftos:// name on detail |
|---|---|---|
| `/ai-org` | Overview: counts, needs attention, running now | none (a view) |
| `/ai-org/running` | Instances and team runs, grouped by node | |
| `/ai-org/running/<instance-id>` | One instance (summary only) `instances/<ULID>` |
| `/ai-org/installed` | Installations, grouped by environment (System, then each project) | |
| `/ai-org/installed/<install-id>` | One installation (agent or team) `installations/<uuid>/agents/<id>` or `/teams/<id>` (system scope: `nodes/<node-id>/agents/<id>`) |
| `/ai-org/catalog` | Definitions: agents and teams; filter by role type/trust/host/source | |
| `/ai-org/catalog/agents/<id>` | One agent definition | `agents/<id>?rev=sha256:...` |
| `/ai-org/catalog/teams/<id>` | One team definition | `teams/<id>?rev=sha256:...` |
| `/ai-org/org` | Team graph (replaces the `reports_to` chart) | |
| `/ai-org/installed/<id>/review` | Change set review (AD-8). Not in v1 | |

Cross-surface links (the only places AI org data appears elsewhere):

- **Project page, Info tab, "Agents" section:** this project's installations (agent or team,
  version, state, node) plus Install. Developers act here; `/ai-org` is where the owner sees.
- **Mesh Network node / Server page:** read-only "System agents on this node" block (system-
  scope installations and instances) linking to `/ai-org/installed/<id>`. No install controls
  there, per the redesign plan.
- **Overview notifications:** failed installs, drift, a stopped system agent link into AI org.

### 3a. weftos:// names (ADR-114 §5a, proposed)

Every detail page shows its `weftos://<mesh>/<kind>/<id>` name under the title with a copy
button; in-page links resolve by name. A name is an identifier only and grants nothing. Rules
from ADR-114: one thing, one name; relationships are data, never path; installing needs a
pinned `rev`.

| Object | Name | Page |
|---|---|---|
| Agent package | `weftos://<mesh>/agents/<id>?rev=sha256:<bundle digest>` | own page (Catalog) |
| Team | `weftos://<mesh>/teams/<id>?rev=sha256:...`; a member slot is `teams/<id>/members/<agent-id>` | own page; slot is a section |
| Project assignment (desired) | `projects/<ULID>/teams/<id>`, `projects/<ULID>/agents/<id>` | drawer on the project page and the definition page |
| Installed copy in one checkout (realised) | `installations/<uuid>/teams/<id>`, `installations/<uuid>/agents/<id>` | row on the checkout's installation page, own detail page |
| OS agent (node-scope install) | `nodes/<node-id>/agents/<id>` | own page, listed under System |
| Running agent or team run | `instances/<ULID>` (new kind; ADR-114 edit needed) | own page |
| Harness exposure | `nodes/<node-id>/exposures/harness.<id>` | section on the installation |
| Bound memory store | `memory/<store>` | link from the installation |
| Lead seat | `members/<uuid>` (a person) | no page |
| Views (Overview, Running list, Catalog, Org) and team data (edges, write authority, presets, rules) | none | |

Rules the page enforces:

- **Installation names never take `?rev`**; the pinned revision is shown as data next to the name.
- **Instance names do not survive restarts.** A restart is a new ULID with `restart_of`. The
  instance page shows "stable link" with the installation name (`nodes/<n>/agents/chief-of-staff`
  or `projects/<P>/teams/weftos-core`), which resolves to the current instance. Exited
  instances stay viewable as records for the retention period; member instances link to their
  team run as parent.
- **Cross-mesh:** show an authority label (friendly mesh name, display only) beside every name.
  A shared project's assignments keep the owning mesh's authority; an instance our mesh runs for
  it gets ours. Public packages show "same as <rev>" across meshes.
- **A name grants nothing:** render as copyable text plus internal links, never as access.

Node and project of an install or instance are fields, not part of its own name, except the
stable-link forms above.

## 4. What a user sees first (`/ai-org`)

```
AI org                                    [ Overview | Running | Installed | Catalog | Org ]

  Running now        Installed             Catalog           Needs attention
  ● 6 instances      14 installations      9 agents          3
    2 system, 4      (2 system, 12 proj)   2 teams
                     1 drifted, 1 failed   3 updates avail

  NEEDS ATTENTION
  ! weftos-core (team) drifted in weftos @ macbook: 2 files edited locally   [review]
  ! Liber 0.3 -> 0.4 available, installed in 2 projects                      [open]
  ! hermes (system) stopped on macbook, last seen 14m ago                    [open]

  RUNNING NOW                                                  filter: [All][System][Project]
  SHAPE   NAME           SCOPE / WHERE                    STATE        DOING
  agent   hermes         SYSTEM  macbook                  running      idle
  agent   cog-host       SYSTEM  example-host             running      idle
  team    weftos-core    PROJECT weftos @ macbook         3 live       WEFT-812
  agent   Stew 0.3       PROJECT weftos @ macbook         working      WEFT-812

  [ Install an agent or team into a project ]
```

Rows link to their detail page; WHERE links to the node or project. Install is the one
primary action.

## 5. Telling things apart at a glance

State vocabulary shown in the UI (architect): instance "running" = starting, idle or working; "stopped" = exited (done, recycled, failed, stopped); also awaiting_confirmation, suspended, refused. Installation: proposed, planned, approved, delivering, installed, drifted, upgrading, removed, failed, refused. OS install: enabled, disabled. Definition: draft, gated, published, deprecated, yanked.

| Facet | Values | Treatment |
|---|---|---|
| Shape | `AGENT`, `TEAM` | glyph (single square outline vs stacked grid) plus the word in the eyebrow |
| Scope | `SYSTEM` (node daemon), `PROJECT` (workspace) | filled chip for System, outlined chip for Project, plus node or project name |
| Stage | definition (version + install count), installation (state chip), instance (live dot) | given by the tab the row is on |

Colour is never the only signal. Trust tier stays a colour chip as today, as a separate third
chip. Catalog `kind` (specialist, lane, skill, template) is shown as **Role type**, only in
Catalog/definition detail, so "kind" is not overloaded with Shape.

Templates (domain-expert) are not installable as-is: the Catalog marks them "Template:
instantiate per project"; the derived package appears only in that project's private source.

## 6. Wireframes

### 6.1 Catalog (`/ai-org/catalog`)

```
AI org > Catalog     [Agents | Teams]   filter: [role type v] [trust v] [host v] [source v] [has update]
 ┌────────────────────────┐ ┌────────────────────────┐ ┌─────────────────────────┐
 │ AGENT  Stew            │ │ AGENT  Liber           │ │ TEAM  weftos-core       │
 │ lane · Core · gate ok  │ │ specialist · Core      │ │ 9 slots · lead seat     │
 │ v0.3 · C G X           │ │ v0.4 · C G X · UPDATE  │ │ v0.3 · presets lean/full│
 │ installed in 2         │ │ installed in 2         │ │ applied to 2 projects   │
 └────────────────────────┘ └────────────────────────┘ └─────────────────────────┘
```

Agent definition detail:

```
AI org > Catalog > Liber
 AGENT  Liber  v0.4   Core   specialist   signed                    [Install]
 weftos://<mesh>/agents/liber?rev=sha256:9c1e...                            [copy]
 Source: weftos agents/ (public)        Hosts: claude, codex, grok
 Capabilities / skills / risk flags / provenance / eval receipt   (as today)
 Needs from the project: memory store, docs root     Secret names: none
 In teams: weftos-core (memory router)
 INSTALLED IN
   ENVIRONMENT            VERSION  STATE      DRIFT   RUNNING
   weftos @ macbook       0.4      installed  none    idle
   weftos-cogs @ macbook  0.3      installed  none    -     update available
```

Team definition detail:

```
AI org > Catalog > weftos-core
 TEAM  weftos-core  v0.3  signed  trust: core                          [Install]
 weftos://<mesh>/teams/weftos-core?rev=sha256:3fa9...                       [copy]
 Lead seat: chief of staff (human-driven main session, never spawned)
 PRESET: ( ) lean  (o) full  ( ) codex-review
 SLOTS        one job                 tier  authority   owner     agent
  Stew        writes the board        top   supervised  ops       -> Stew 0.3
  Doc / Gus   writes card history     mid   supervised  docs      -> Doc 0.2
  Liber       routes memory           mid   autonomous  memory    -> Liber 0.4
  Mo          consult-only advisor    mid   escalated   -         -> Mo 0.1
 WRITE AUTHORITY (read-only)  board: Stew · card history: Doc · memory: Liber · docs: Doc · ...
 EDGES   delegates_to · consumes · consults     (graph on the Org tab)
 TEAM RULES (8)  [expand]
 INSTALLED IN   environment        version  preset  state      drift  run
                weftos @ macbook   0.3      full    installed  none   3 of 9 live
```

### 6.2 Installed (`/ai-org/installed`)

```
AI org > Installed          filter: [shape v] [state v] [node v]

 SYSTEM · macbook (node, v0.8.4-rc.3)                                   open node ->
   AGENT  hermes (voice talk)     weftos 0.8.4   enabled    exposed: paired
   AGENT  agent loop              weftos 0.8.4   enabled    exposed: local
   AGENT  chief of staff host     weftos 0.8.4   enabled    (also in team weftos-core)
 PROJECT · weftos @ macbook                                             open project ->
   TEAM   weftos-core 0.3 full    installed   no drift      3 of 9 live
   AGENT  Domain-expert (derived) 0.1  planned   change set preview, risk medium
 PROJECT · weftos-cogs @ macbook
   TEAM   weftos-core 0.3 lean    delivering  PR #14 open
```

Installation detail:

```
AI org > Installed > weftos-core in weftos @ macbook
 TEAM install   weftos-core 0.3 (rev sha256:3fa9...)  full   state: installed   drift: none
 weftos://<mesh>/installations/<uuid>/teams/weftos-core      authority: weaveLogic  [copy]
 Environment: project weftos @ macbook  -> /projects/<id>   node -> /servers/macbook
 Hosts rendered: claude, grok, codex          Lock: .weftos/agents.lock.json (47 files)
 Bound project context: memory store, board adapter, docs root   (never overwritten)
 Birth report: adapters bound 4, keys missing 1 (card-history root)
 Change sets: #14 preview -> approved -> delivered, risk medium
 Instances: Stew (working WEFT-812), Liber (idle), Doc (idle)
 Actions:  [Update to 0.4]  [Remove]    (owner approval required)
```

A system-scope installation is the same page with Environment = node, no project, no lock,
"version tracks the weftos release", and **Enabled / Disabled** instead of install/remove
(not uninstallable per project).

### 6.3 Running (`/ai-org/running`)

```
AI org > Running                               group: [by node v]
 macbook
   AGENT  hermes            SYSTEM   running   idle             model: infer.chat (local)
   AGENT  Stew              PROJECT  working   WEFT-812         model: top-tier (frontier)
   TEAM   weftos-core run   PROJECT  3 live     since 14:02      -> members
 example-host 
   AGENT  cog-host          SYSTEM   running   idle
```

Instance detail (daemon reports a summary only: ids, package, slot, model, state, card; never transcript content, so the transcript link is shown only on the node): installation link, member slot, runner (host CLI session, daemon process or
headless harness job), node, project, who started it, kernel identity, model actually in use
(resolved from tier), current card, transcript link (ADR-110), weftos:// name. Instances are
ephemeral (lanes recycle after one bounded task), so exited ones stay listed briefly with
their outcome (done, failed, recycled). A team run (TM5, `weaver team up`) is unbuilt; until
it ships a team row shows "run: not available yet".

### 6.4 Org (`/ai-org/org`)

```
AI org > Org            team: [weftos-core v]      edges: [x] delegates_to [ ] consumes [ ] consults
        ┌───────────────┐
        │  you (owner)  │          root = the human principal
        └──────┬────────┘
        ┌──────┴────────┐
        │ Chief of staff│          first report; the lead seat
        └──┬─────┬────┬─┘
        Stew   Doc   Liber   ...   members hang off delegates_to; other edges toggle on
```

Click a node to open its definition.

### 6.5 Project Info > Agents (developer entry point)

```
Project weftos > Info > Agents
 INSTALLED   TEAM  weftos-core 0.3 full    macbook   installed  no drift    3 of 9 live
             AGENT Domain-expert 0.1       macbook   planned    change set, risk medium
 [ Install agent or team ]      (same flow as AI org)
```

### 6.6 Install flow (shared; reviewed change set, ADR-112 §3)

```
Install                  1 What   2 Where   3 Review   4 Result
 What:  (o) team weftos-core  ( ) agent ...      version/rev [0.3 v]   preset [full v]
 Where: environment [project weftos v]  node [macbook v] (only nodes that hold the workspace)
        hosts [x] Claude [x] Grok [x] Codex
 Review: change set digest 3fa9...  risk medium  14 files (create 9, update 5)
         [view diff]      Needs owner approval before delivery
         Project context to fill: memory store, docs root (open questions)
 Result: node action team.apply -> macbook, or a PR if the project has no node
```

System scope has no Install; it has Enable/Disable on the installation row (later).

## 7. How installed/running connects to projects and nodes

- **Two levels** (architect): *desired* = the project, mesh-wide (the dashboard assignment);
  *realised* = one checkout of the project on a machine (ADR-114 `installations/<uuid>`, holding
  the lock and drift), or a node for OS agents. The Installed tab shows both: the project row
  (assignment drawer) expands into one row per checkout. One project on two machines
  shows two installations. Node links to `/servers/<host>`, project to `/projects/<id>`.
- **Desired vs observed.** Assignments = desired, change sets = proposed, locks = observed
  (`project_agent_locks`). The Installed tab shows desired state now; installed/drifted appear
  once nodes report locks. Until then state reads "waiting for node". Never show "installed"
  from desired state alone.
- **Memory binding belongs to the installation**, not the definition. The page shows which
  store/board/docs root an install is bound to and never offers "copy memory from another
  project" (ADR-112 §6).
- The project Agents section, the node "System agents" block and the definition "Installed in"
  table all read the same installation rows.

## 8. Actions, and what is governed

Principle (surfaces first, controls later): read everything day one; ship exactly **one**
write in v1, Install (draft + preview; owner-approved trigger).

| Action | Where | v1? | Governance |
|---|---|---|---|
| View anything | all | yes | workspace member |
| Install agent/team into a project | Catalog, AI org, Project Info | yes | change set; owner approves before delivery; high-risk always owner |
| Update to newer rev | installation | v1.1 | same change-set path; owner approves |
| Remove (uninstall by lock) | installation | v1.1 | owner only; lists exactly the files removed; project facts stay |
| Resolve drift | installation | v1.1 | a change set, never an overwrite; owner |
| Enable/Disable a system agent | system installation | later | owner only; node local policy wins (may report `refused`) |
| Stop an instance or team run | Running | later (needs TM5) | owner only; queued node action, node confirms locally |
| Start an agent or team | none (dashboard cannot start) | not planned | architect default: starting is a queued node action the node confirms locally (ADR-120 Q3); the dashboard sees and stops only |
| Expose a harness (ADR-120 kind `harness`) | installation | later (ADR-120 X3/X4) | owner only; audience, limits, local wins; start is governed `workload.ctl`, never a shell endpoint |
| Edit a team (AD-12), change preset | team installation | later | owner only |
| Add a source (private repo, signed bundles TM6) | Admin | out of scope | owner only |

Every governed action shows, before it runs: what changes, who must approve, which node
applies it. Refused/pending/failed are visible row states, not toasts.

## 9. Empty states

| Situation | Message and action |
|---|---|
| No system agents reported | "No system agents reported by nodes yet." Link Mesh Network. No create button. |
| No nodes paired | "Pair a node first; installs need a machine that holds the project." Link `/mesh`. |
| Catalog unreachable (fixture fallback) | Keep the muted fixture note; Install disabled. |
| Catalog empty | "No agents in the catalog yet." |
| No teams | "No teams defined. Teams live in `agents/teams/` in the weftos repo." |
| Definition never installed | "Not installed anywhere" with [Install]. |
| Project has no node | Install step 2: "Add this project locally first" -> project Info. |
| Installed, no run support yet | Running column `-` with "Runtime ships with weaver team". |
| Node offline | Rows stay, greyed, "last seen 14m ago"; actions disabled. |
| Applied, no heartbeat yet | State `waiting for node`. |
| Template selected | "Templates are instantiated per project" with the derive flow, not Install. |
| Client-private source | Private workspace catalogs merge into the Catalog labelled by source (public / workspace name), visible only inside that workspace. |

## 10. v1 build order

1. Route-ify tabs; adapt `lib/agentCatalog.ts` to the live catalog shape; drop `reports_to`.
   Directory becomes Catalog; Teams moves under it. Show weftos:// names.
2. Installed tab from `project_agent_assignments` + `agent_changesets` (desired/proposed,
   read-only). Overview strip and needs-attention from the same rows.
3. Org tab from team edges (needs edges/slots from the catalog or `team.yaml`; the DB
   `agent_team_members` has no slot fields).
4. Project Info > Agents section and install flow (draft/preview; owner trigger).
5. Blocked on a node-report agents section (within the 60 KB budget): system-scope rows,
   observed locks, Running tab.
6. Everything marked "later" in section 8.

## 11. Reconciliation with the architect's model

Resolved: OS agent = system-scope installation; environment has three scopes; install is
(project, node); `reports_to` is dead so Org uses team edges.

Open or concerning:

1. **"Agent" still means definition, installation and instance.** Headings use the stage
   nouns Catalog / Installed / Running; the model doc and API should too.
2. **Chief of staff is the one hybrid:** catalog package, weftos-core member and system-scope
   installation. It appears under System in Installed and inside the team, cross-linked. Needs
   confirmation that one definition may have both.
3. **Machine-global host config (scope c)** only via the ADR-109 doctor; no page row in v1,
   later a "Host setup" link.
4. **weftos-native host** exists in the model but the DB hosts check excludes it; shown for
   system scope only. Migration needed.
5. **No instance table and no report field**, so Running is empty until both exist.
6. **weftos:// names** (section 3a): `instances/<ULID>` is a new kind and the installation/OS-agent sub-resources are new paths; ADR-114 needs the edit (the model doc §5 still lists the instance name as open).
7. **Principal-scoped memory for system agents** is an architect question; system-install
   detail says "none; principal scope TBD" until decided.
8. **Legacy `agents/weftos/*.md`** (12 agents) are retired from the org view (architect default) until packaged.

## Owner questions (recommended defaults)

1. **Organise by lifecycle, OS agents as a filter.** Tabs Overview / Running / Installed /
   Catalog / Org with System vs Project badges, not separate OS-agent / Team / Package tabs.
   Default: yes.
2. **Stage nouns.** Catalog / Installed / Running; "agent"/"team" only as the shape badge.
   Default: yes.
3. **Open on Overview**, not the org chart; Org tab built from team edges, principal at the root, then chief of staff, then members via `delegates_to`. Default: yes.
4. **Install target is (project, node).** One project on two machines = two rows. Default: yes.
5. **Who installs.** Developers draft and preview; only the owner approves a trigger;
   high-risk always the owner. Default: yes.
6. **v1 write scope.** Only Install; everything else read-only until its node action exists.
   Default: yes.
7. **System agents are enabled/disabled**, not installed or removed, and versioned with
   weftos. Default: yes.
8. **Chief of staff may be both a system install and a team slot.** Default: yes, shown in
   both places and cross-linked.
9. **Running before the runtime exists.** Hide the team Run column until `weaver team up`
   ships, but show real instances as soon as nodes report them. Default: yes.
10. **weftos:// names.** Packages/teams `agents`/`teams` with `rev`; assignments `projects/<ULID>/...`; checkout installs `installations/<uuid>/...` (never `rev`); OS agents `nodes/<id>/agents/<id>`; running things `instances/<ULID>` (new kind, new ULID per restart, stable link via the installation name). Needs an ADR-114 edit. Default: yes.
11. **Private catalogs** (a workspace's `weftos-<slug>` packages and teams) merge into the
    Catalog, labelled by source, visible only in that workspace. Default: yes (architect Q9).
12. **Templates** show "instantiate per project", not Install. Default: yes.
13. **Dashboard may see and stop, not start** agents or teams; starting is a queued node
    action the node confirms locally. Default: yes (architect Q8).
14. **Running instances are reported as a summary only** (ids, package, slot, model, state,
    card), never transcripts. Default: yes (architect Q7).
15. **Principal-scope memory for system agents** (CoS preferences): none on the page in v1;
    shown as "none; principal scope TBD". Default: architect Q6 (no agent-owned memory).

Merged with the architect's list (docs/plans/ai-org-model.md §9): their Q1 = my Q1/Q7, Q2 =
section 6.6 (project install plus node enable/disable; machine-global stays doctor), Q3 = my
Q10, Q4 = my Q3, Q5 = section 11.8, Q6 = my Q15, Q7 = my Q14, Q8 = my Q13, Q9 = my Q11.
