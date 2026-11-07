# Agent directory: design note

- **Status**: Built on `feat/agent-directory`, 2026-11-07. Card c3d1051d.
- **Surface**: MCP `control` profile tools `agent_list` and `agent_get` (attach mode).
- **Code**: `crates/clawft-cli/src/commands/agent_directory.rs` (join), `mcp_attach.rs` (tools).

## Identity

An agent's key is `weftos://agent/<id>`. `<id>` is the definition id: the `id` in
`agents/catalog.json` for a declared agent, the `agent_id` string for a running one (the
supervisor table stores the name the agent was spawned under, which is the definition id by
convention), and the part after `harness.` for an exposure (`harness.<id>`, ADR-120 section 3).
Anything outside `[A-Za-z0-9._-]` is percent-encoded. The key holds no pid, node or boot
number, so a daemon restart gives the same entry; a pid is only an attribute of a running
instance (`running[].pid`).

Not used as keys: `agent_id` UUIDs from `AgentRegistry` (restart-reset), pids, and key
material. Keying on name plus pubkey (research 08) would need per-agent keys on every source;
declared and exposed agents have none, so the pubkey stays an attribute for a later phase.

## The join

Three sources, one entry per key:

| Source | Read from | Contributes |
|---|---|---|
| declared | `agents/catalog.json` (found at or above the cwd, or `$WEFTOS_AGENTS_DIR`) | version, kind, role, trust tier, state, digest |
| running | weave `agent.list` | one `running[]` row per instance (pid, state, parent) |
| exposed | weave `expose.list`, rows of kind `harness` | `exposed[]`: exposure id, state, audience, served or refused |

Each entry has `status`, `from` (which sources it appears in), the three blocks, and `notes`.
`agent_get` takes the bare id or the URI and adds the `sources` health block.

The join runs in the MCP server (the client side of the daemon socket): it adds no daemon RPC.
`weftos::init::AgentSource` is the build-time loader for packages, but `clawft-cli` does not
depend on the `weftos` crate; the directory reads the generated catalog directly rather than
add a dependency.

## When sources disagree

| Case | `status` | `notes` |
|---|---|---|
| declared, nothing running or exposed | `declared` (`inactive` if the catalog says so) | none |
| declared and running | `running` | none |
| running, not in the catalog | `running` | `undeclared` |
| declared `inactive` but running | `running` | `inactive_but_running` |
| exposure on, no instance running | `stopped` | `exposed_not_running` |
| exposure off or paused, nothing running | `stopped` | none (the exposure block says why) |
| exposed under `harness.<x>`, `<x>` not in the catalog | `stopped` or `running` | `undeclared` |

Several instances of one id are one entry with several `running[]` rows.

## When a source is unreadable

`sources.<name>` is `{ok: false, error}`; it is never an empty list. Entries that depend on the
missing source say so: if `agent.list` fails, every entry without a running row has status
`unknown` and the note `running_unknown`. If all three sources fail the tool returns an
error, not an empty directory. A missing `expose.list` (older daemon) shows as an exposed
source error.

## Relation to mesh bots

`feat/mesh-bots-phase-a` (unmerged) names bots `weftos://mesh.bot.*` with a per-bot Ed25519
key, and its `BotRecord` carries a `definition` and version. A bot is a placed instance of a
definition, so the two do not collide: `weftos://agent/<definition>` is the template-and-
instances view here, `weftos://mesh.bot.<name>` is one durable bot. The join key between them
is the bot's `definition`. A shared type is worth it only for the state vocabulary
(`running|idle|dead`) and is left until that branch merges.

## Not done

- The directory is per daemon. A mesh-wide view needs peers' `agent.list` and `expose.list`
  over the signed wire (the bot directory's `mesh:true` pattern).
- Exposure to agent mapping is by the `harness.<id>` naming convention; ADR-120 does not yet
  say which agent a harness exposure runs.
