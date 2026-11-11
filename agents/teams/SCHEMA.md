# Team spec v1 (`agents/teams/<team>/team.yaml`)

ADR-112 section 1. A team with `schema: 1` is checked by `scripts/agents-team-validate.mjs`
(called from `scripts/agents-validate.mjs`, so the `agents-validate` command and gate check
17 cover it; fixtures and tests: `scripts/agents-team-validate.test.mjs`,
`scripts/fixtures/agents-teams/`). A file without `schema` is a legacy team and only gets the
old checks. `weftos init` and `weftos team apply` read a `schema: 1` team strictly: an undeclared
field, a wrong type or an unknown value refuses the run and names the field. What they do with each:

| Field | Applied as |
|---|---|
| `schema`, `name`, `version`, `description` | Checked (`schema` must be 1, `name` must equal the directory, the rest must be text). |
| `lead`, `shared_skills`, `members[].agent` | Selected for install; `agent@version` must equal the catalog version. |
| `members[].active`, `phase` | Inactive members are not installed; the plan names their phase. `active` must be a boolean. |
| `members[].position` | Order of the lock slots. |
| `members[].tools` | Claude `tools:` (narrowing only). Refused on grok and codex, whose agent files have no tool list. |
| `members[].model_tier` | Claude `model:` (`top` opus, `mid` sonnet, `small` haiku). Not applied on grok or codex; the plan warns. |
| `members[].nickname`, `one_job`, `authority`, `owner`, `tiers`, `must_not` | "Team charter" section in the rendered agent, every host. |
| `edges`, `write_authority`, `team_rules` | Same section: the member's own edges, all write holders, all rules. |
| `hosts` | The host's `tool_map` in that section, on that host only. |
| `presets` | `--preset NAME` (`weftos init`, `weftos team apply`): overrides `active`, puts a member on one host. |

A legacy team (no `schema`) is read as before and the plan lists the fields it does not apply.
Two applied teams that share a package get both charters; they may not disagree on its `tools` or
`model_tier`. The lock does not record the preset; `active` overrides are recorded in the slots.

Method only: no project facts, client names or memory in a team file (ADR-112 section 6;
`scripts/agents-leak-check.sh` enforces it).

## Fields

| Field | Meaning |
|---|---|
| `schema` | Must be `1`. |
| `name`, `description` | Human text. |
| `lead` | `null`, or a doctrine skill package. Never a member or any spawnable package. |
| `shared_skills[]` | Skill packages loaded into the lead session (e.g. `lead-doctrine`). |
| `members[].agent` | Spawnable package id, optionally pinned `id@version` (must equal the catalog version, the weftos release). |
| `members[].nickname`, `position` | Display only. |
| `members[].one_job` | One sentence, no " and ". |
| `members[].model_tier` | `top`, `mid` or `small`. |
| `members[].tools` | Optional allow-list; may only narrow the package's AGENT.md `tools`. |
| `members[].authority` | `autonomous`, `supervised` or `escalated`. |
| `members[].owner` | A human role slug (`docs-owner`), never a person's name. |
| `members[].must_not[]` | Non-empty list of refusals. |
| `members[].tiers[]` | Confidentiality classes it may read: `public`, `internal`, `restricted`. |
| `members[].active`, `phase` | `active: false` requires an integer `phase` that activates it. |
| `edges[]` | `{from, to, kind}` between members; `kind` is `consumes`, `delegates_to` or `consults` (consult-only, never blocking). |
| `write_authority` | Resource to exactly one member. All of `board`, `card_history`, `memory`, `docs`, `code`, `production_read` are required. |
| `team_rules[]` | `{name, statement}`; all eight names required: `board-is-for-humans`, `pickup-is-a-claim`, `worktree-per-lane`, `result-header`, `refuse-not-empty`, `stand-down`, `handoff-ritual`, `injection-stop-the-line`. Statement is one line. |
| `presets` | Name to `{active: {member: bool}, hosts: {member: host}}`. Only declared presets are valid; unknown members, hosts or keys are refused, and a preset may not deactivate a write-authority holder. |
| `hosts` | `claude`, `codex`, `grok`, each with a `tool_map` for `Task`, `SendMessage`, `TodoWrite`. |

## What a holder must be able to do

A `write_authority` holder must have the package capability and at least one of the tools
(after any `tools` narrowing):

| Resource | Capability | Tools (any) |
|---|---|---|
| `board` | `board_write` | Bash (writes go through the project's board adapter) |
| `card_history` | `queue_write` | Write, Edit |
| `memory` | `memory_write` | Write, Bash |
| `docs` | `docs_write` | Write, Edit |
| `code` | `code_write` | Write, Edit |
| `production_read` | `read_only_measurement` | Bash, Read |

## Example

```yaml
schema: 1
lead: null
shared_skills: [lead-doctrine]
members:
  - agent: steward
    one_job: Write staged manifests onto the board.
    model_tier: mid
    authority: supervised
    owner: board-owner
    must_not: [decide board status or assignee]
    tiers: [internal]
    active: true
edges:
  - {from: developer, to: steward, kind: consumes}   # one block per edge in real files
write_authority:
  board: steward
presets:
  lean:
    active: {mo: false}
hosts:
  codex:
    tool_map: {Task: spawn_agent, SendMessage: send_input, TodoWrite: update_plan}
```

The Codex and Grok tool names in `weftos-core` are working assumptions, not yet checked
against each host's tool list (AD-3 evals will).
