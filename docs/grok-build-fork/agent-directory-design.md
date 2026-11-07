# Agent directory: design note

- **Status**: Built on `feat/agent-directory`, 2026-11-07 (follow-up the same day). Card c3d1051d.
- **Surface**: MCP `control` profile tools `agent_list` and `agent_get` (attach mode).
- **Code**: `crates/clawft-cli/src/commands/agent_directory.rs` (join), `mcp_attach.rs` (tools),
  `crates/clawft-weave/src/agent_sources_rpc.rs` (`agent.sources`, mesh fetch).

## Identity

An agent's key is `weftos://agent/<id>`. `<id>` is the package directory name for a declared
agent, the `agent_id` string for a running one (the name it was spawned under, which is the
definition id by convention), and the exposure's `agent` field for an exposed one. Characters
outside `[A-Za-z0-9._-]` are percent-encoded. The key holds no pid, node or boot number, so a
daemon restart gives the same entry; a pid is an attribute of a running instance.

Not keys: `AgentRegistry` UUIDs (restart-reset), pids, key material. Per-agent pubkeys stay an
attribute for a later phase: declared and exposed agents have none.

## Mesh: no node qualifier on the key

Decision: one key per definition, with an instance list. In a mesh view every `running[]` and
`exposed[]` row carries `node`; the key stays `weftos://agent/<id>`. Reasons: the directory
answers "what is this agent and where does it run", so a per-node key would split one agent
into N entries and `agent_get developer` would need a node first; a restart or a move to
another node keeps the entry; and a durable placed bot already has its own node-bound name
(`weftos://mesh.bot.*`, on the unmerged `feat/mesh-bots-phase-a`, joined by its `definition`).
Cost: a caller wanting one node reads `node` on the rows. Local-only views carry no `node`.

## Sources and the join

| Source | Read from | Contributes |
|---|---|---|
| declared | the `agents/` package tree through `weftos::init::source::AgentSource` | version, kind, trust tier, `state` (`inactive` kept) |
| running | `agent.list` per node | one `running[]` row per instance |
| exposed | `expose.list` per node | `exposed[]` rows that name an agent |

Declared comes from the checkout's `agents/` (nearest at or above the cwd with a package, or
`$WEFTOS_AGENTS_DIR`, which must be usable: a bad value is an error, never a fallback). With
no checkout the tree embedded in the binary is used. `sources.declared.origin` says `disk` or
`embedded` (plus `location`, `commit`). Declared is local only; peers' trees are not read.

Exposure to agent: an exposure's `agent` field (ADR-120, updated 2026-11-07) is the only
link, plus a fallback for older exposures: a `harness` exposure `harness.<id>` links to
`<id>`, marked `linked_by: harness-id-fallback`. A malformed `agent` field is a `warnings`
entry; the fallback is not tried when the field is present.

Each entry: `status`, `from`, `declared`, `running`, `exposed`, `notes`. `agent_get` (bare id
or URI) adds `sources`. Both take `mesh` (default false: this node only).

## Mesh fetch

`agent_list {mesh:true}` calls the daemon RPC `agent.sources {mesh:true}` (Admin: it contacts
peers). The daemon returns this node's block, then asks each known peer for the read-only
node-admin method `agent.sources` on the signed `workload.ctl` wire (same wire and controller-key
rule as `dashboard.status` and the bot directory). A read-only method needs a peer at tier
`paired` or above; the peer must list this node's key as a controller. A peer that does not
answer is a block `{node_id, ok:false, error}`, shown in `sources.nodes[]` with its error; it
fails nothing and nothing is shown for it from memory. If the whole mesh call cannot be made
(older daemon, no Admin) the tool errors instead of answering with the local view.

## When sources disagree

| Case | `status` | `notes` |
|---|---|---|
| declared, nothing running or exposed | `declared` (`inactive` if the package says so) | none |
| running (any node) | `running` | `undeclared` if not in the tree; `inactive_but_running` |
| exposure on at a node with no instance there | `running` or `stopped` | `exposed_not_running` |
| exposure off or paused, nothing running | `stopped` | none |
| nothing running seen, but a node was unreadable | `unknown` | `running_unknown`, naming the nodes |

If every source fails the tool errors, never an empty list.

## Open

- A spawn name that differs from the definition id shows as undeclared (accepted for now).
- `agent.sources` and the bot directory both list peers; `directory_peer_ids` in
  `workload_place_rpc.rs` duplicates `peer_node_ids` on the mesh-bots branch. Keep one when
  they merge. The two also both extend `workload_ctl::msg::method::NODE_ADMIN`.
