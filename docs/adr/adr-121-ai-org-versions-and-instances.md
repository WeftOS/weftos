# ADR-121: AI org: definition, installation and instance; versions, dependencies and chain-reported instances

- **Status**: Accepted (2026-10-11; owner answered the model's decisions the same day)
- **Deciders**: owner
- **Builds on**: `docs/plans/ai-org-model.md` (the model, adopted here) and
  `docs/plans/ai-org-page.md` (the page), ADR-112 (agent teams: TM2 project apply, TM3 apply over
  the mesh, TM6 signed bundles), the Agent Directory decision (AD-1..AD-23), ADR-114 (`weftos://`
  names, §5a), ADR-120 (exposures), ADR-022 (chain audit), ADR-108 (node actions),
  `crates/clawft-cow-memory` (agenticow semantics over `rvf-runtime`).

## Decision

### 1. The model
Adopt `docs/plans/ai-org-model.md`: **definition → installation → instance**, and for groups
**team definition → team installation → team run**. OS agents are catalog packages installed
by the release at node scope and enabled or disabled per node, versioned with WeftOS.

### 2. Owner answers (2026-10-11)

| # | Decision |
|---|---|
| 1 | OS agents are catalog packages (default). |
| 2 | Organise the page by lifecycle stage with an "OS agents" filter (default). |
| 3 | An install targets (project, node); the dashboard installs into projects and enables/disables node OS agents; machine-global config stays a doctor action (default). |
| 4 | `agents/<id>` names the package (pinned by `rev`); add the `instances` kind; add the sub-resources in the model's §5 (default; ADR-114 amended). |
| 5 | **Every person at WeftOS has their own chief of staff. The org is flat above the chiefs of staff**: the people and their chiefs of staff are peers; teams hang below each chief of staff by delegation edges. `reports_to` is dropped. |
| 6 | **Install is request → approval → install, and it must work end to end.** Approval may be by the requester themself when they are allowed to approve. The dashboard can see and stop; starting is a queued node action the node confirms locally. |
| 7 | The chief of staff can be both an OS agent and a team member (default). |
| 8 | **Instances should report transcripts.** Transcripts are not easily available yet, so v1 reports a summary only (ids, package, slot, model, state, card); a card tracks adding transcripts. |
| 9 | **Agents use their own brains and metadata.** Each installation binds a ruvector (RVF) brain with a metadata schema. Project agents working next to a developer take more care: they leave notes (what was decided, why, where) so the brain can be reused or analysed later. Nothing carries between projects unless promoted deliberately. |
| 10 | **Package the 12 legacy `agents/weftos/*.md` agents now, as originals, marked inactive.** This exercises the inactive-package feature. |
| 11 | Private catalogs and teams are visible only inside their workspace (default). |
| 12 | Templates show "instantiate per project" (default), with an explain affordance (§5). |

### 3. Versions, version trees and dependencies for first-class objects
- Every first-class object with a definition (agent packages, teams, cogs, projects' agent
  sets) has **versions**: a content digest (`sha256` of the complete bundle), a semantic
  version label, its parent version, and its **dependencies** (other packages, skills, cogs,
  models by role) pinned by digest. Sub-packages form a **version tree**.
- The catalog (`agents/catalog.json`) carries per-package digest, version, parent, state
  (`active` | `inactive`) and dependencies; `scripts/build.sh agents-validate` checks them.
- The version record of truth is a simple local file per node (`~/.weftos/versions/*.json`,
  append-only), and the same records are indexed in a **CoW ruvector lineage**
  (`clawft-cow-memory`): one lineage per object, `promote` per released version, `branch` for
  sub-packages and local derivations, so history, diffs and "what depends on this" queries are
  cheap.

### 4. Nodes report instances and changes on the chain
- A node running instances of a project (or OS agents) is responsible for reporting them:
  start, stop, version, configuration change, failure. Each is a chain record (ADR-022), so it
  is visible and auditable, and the heartbeat carries a compact summary for the dashboard.
- **Versions are enforced**: a node refuses to start an installation whose files do not match
  its pinned digest, and reports drift.

### 5. Explain affordance
A small reusable "explain" icon sits next to actions and decisions across the dashboard. It
opens a modal that says what the action will do, what it changes, who must approve it and how
to undo it. It starts with install, approve, rotate token, revoke pairing, make primary and
key project, and becomes the default for new actions.

### 6. The agents API gets used
Exposing an agent package (installation with an exposure, ADR-120) is how the dashboard's
`/api/harness/agents/*` routes come into use: the node reports installations and instances
through them, and install requests flow through them.

## Phases

| Phase | Delivers | Done when |
|---|---|---|
| O1 | Catalog digests, versions, parents, dependencies, state; legacy agents packaged inactive; local versions file + CoW lineage index | `agents-validate` passes with digests; the 12 legacy agents show as inactive packages; a version tree query lists a package's sub-packages |
| O2 | Node instance registry, chain records, heartbeat summary, digest enforcement | starting a tampered installation is refused and reported; instances appear on the dashboard |
| O3 | Install request → approval (self allowed) → install end to end through the agents API | the owner requests an install, approves it, and the node installs and reports it |
| O4 | Dashboard AI org page (stage tabs, flat org above chiefs of staff), versions/dependencies view, explain modal | the page shows packages, installs and instances with names and versions; explain opens on each action |
| O5 | Transcripts | (card) instances report transcripts, under the privacy rules of ADR-110 |
