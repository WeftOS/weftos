# ADR-076: MCP tool surface principles + unified capability catalog

**Date**: 2026-07-30  
**Status**: Accepted  
**Updated**: 2026-11-07 (`agent_list` now serves the joined agent directory over `agent.list`, `expose.list` and the catalog; `agent_get` added; see docs/grok-build-fork/agent-directory-design.md)  
**Deciders**: product + integration (owner: outbound MCP must be a designed API, not a dump of the agent registry)  
**Depends-On**: ADR-070 (MCP registry ownership), ADR-075 (Grok/peer MCP clients)  
**Relates-To**: ADR-073 (`WindowIntent`), ADR-074 (voice frontends), weave capabilities (`clawft-weave` capability map), HTTP `/api/*` facade

## Context

### Audit (2026-07-30)

| Layer | Assessment |
|-------|------------|
| **MCP engine** | Strong: `ToolProvider`, composite namespaces, middleware (Security / Permission / Result / Audit) |
| **Inbound MCP** (WeftOS as client) | Strong: `internal_only` + skill-gated discovery |
| **Outbound MCP** (`weft mcp-server`) | Weak product design: re-exports agent `ToolRegistry` + skills; empty `allowed_tools` = all tools |
| **Weave RPC** | Stronger taxonomy: Read / Chat / Write / Admin methods (`agent.*`, `kernel.*`, `substrate.*`, …) |
| **HTTP/WS API** | Dashboard-oriented resources; bearer auth; not unified with MCP names |

**Conclusion:** plumbing is good; **catalog and defaults** are not. External clients (Grok Build, Claude Code) need a **stable, profiled control plane**, not a sidecar dump of coding-agent tools. Instance attach should prefer a **daemon façade**, not a disconnected process with `substrate = None`.

### Current outbound bag (illustrative)

Builtin-class tools today include FS (`read_file`…), `exec_shell`, OS `spawn`, web, file `memory_*`, voice/audio, `render_ui`, `delegate_task` — plus N **skill** tools that return SKILL.md bodies. Weave methods like `agent.list` / `agent.spawn` / `substrate.read` are **not** first-class MCP tools.

### Naming traps

| Wire name today | Actual meaning | Confusion |
|-----------------|----------------|-----------|
| `spawn` | OS subprocess | Not agent/window spawn (ADR-073) |
| `memory_*` | workspace memory file | Not full substrate / AgentDB |
| `builtin__X` after composite | namespaced list | Clients may see `weftos__builtin__X` (double prefix) |

## Decision

### 1. Capability catalog is the single source of truth

Maintain a **capability catalog** (doc + eventual code registry) that maps each product capability to:

| Field | Purpose |
|-------|---------|
| `id` | Stable id, e.g. `agents.list`, `workspace.read_file` |
| `class` | `Read` \| `Chat` \| `Write` \| `Admin` (align weave) |
| `mcp_tool` | Public MCP tool name (or none) |
| `weave_method` | Kernel RPC method (or none) |
| `http_route` | REST path (or none) |
| `profiles` | Which serve profiles include it |
| `session_bound` | Needs live user/hardware/canvas session? |
| `notes` | Semantic constraints |

**Rule:** new external-facing capability lands in the catalog first; MCP / weave / HTTP implement from it. No silent MCP-only or HTTP-only product features without catalog rows.

Canonical doc path (living): `docs/weftos/mcp-capability-catalog.md` (seeded with this ADR).

### 2. Serve profiles (normative defaults)

| Profile | Intent | Default for |
|---------|--------|-------------|
| **`control`** | Instance ops: status, agents, window intents, read-safe inspect | Remote / high-trust clients later |
| **`workspace`** | Sandboxed FS + shell + file memory | Coding attach |
| **`media`** | voice/audio/render_ui (session-bound) | Explicit only |
| **`full`** | Entire registry + skill dump (dev) | Explicit only |
| **`default` (Grok / peer CLI)** | **`control` ∪ `workspace`** | `weft mcp-server` without flags |

```bash
weft mcp-server --profile default          # control + workspace
weft mcp-server --profile full             # opt-in dump
weft mcp-server --profile control,media    # compose
```

**Breaking change allowed for external attach:** empty `tools.allowed_tools` must **not** mean “full dump” for `mcp-server` defaults going forward. Prefer profile default; config allowlist remains an additional filter.

### 3. Public MCP names are product names, not internal namespaces

- Prefer flat, stable names: `status`, `agent_list`, `agent_spawn`, `window_focus`, `read_file`, …  
- Or single product prefix: `weftos__agent_list` from the **client** server key only — **not** `weftos__builtin__agent_list`.  
- Implementation: serve path uses a dedicated provider namespace (`""` or `"weftos"`) that maps catalog ids → dispatch; avoid leaking `builtin` / `skill` as outer namespaces to clients.

### 4. Semantic renames / splits (normative)

| Old / ambiguous | New public MCP | Notes |
|-----------------|----------------|--------|
| `spawn` (OS) | `process_spawn` or keep only via `exec_shell` | Do not use `spawn` for agents |
| Agent lifecycle | `agent_list`, `agent_spawn`, `agent_stop`, … | Façade over weave `agent.*` |
| Window control | `window_*` / WindowIntent tools | ADR-073 / ADR-075 G3 |
| Skills | Prefer `skill_get` + optional resources | Not one MCP tool per skill by default |
| File memory | `memory_read` / `memory_write` kept | Document as workspace file memory |

### 5. Daemon façade preferred for “instance” attach

| Mode | Behavior |
|------|----------|
| **`standalone`** (today) | Local registry; coding tools; no live agents |
| **`attach` (target)** | Tools dispatch to running kernel/weave (live agents, substrate, status) |

`weft mcp-server --attach` (or config) is the product path for “Grok works *within* this instance.” Standalone remains for offline/dev.

### 6. Session-bound tools gated

`voice_*`, `audio_*`, `render_ui` (and future hardware/canvas tools):

- Excluded from **`default`** and **`control`** unless profile includes **`media`**.  
- Require session / display / device context when attached; fail closed with clear errors if none.

### 7. Skills default: not N peer tools

Default profiles:

- **Do not** register every skill as a top-level MCP tool.  
- Provide `skill_list` + `skill_get` (returns body per existing skill contract).  
- `full` profile may expand all skills for power users.

### 8. Inbound re-export guard

`weft mcp-server` **must not** re-export proxied external MCP tools (`internal_only: false` peers) unless profile is `full` **and** explicit `--reexport-mcp`. Prevents recursion and tool floods through Grok.

### 9. Align classes with weave

Capability class drives both weave ACL and MCP session scopes (ADR-075 G5):

| Class | Examples |
|-------|----------|
| Read | status, agent_list, substrate_read, skill_list |
| Chat | agent_chat (when exposed) |
| Write | agent_spawn, window_*, write_file, exec_shell |
| Admin | kernel shutdown, cluster ops — **not** in default MCP |

### 10. Catalog seed (minimum rows)

| id | class | mcp_tool | weave | profiles |
|----|-------|----------|-------|----------|
| `instance.status` | Read | `status` | `kernel.status` | control, default |
| `agents.list` | Read | `agent_list` | `agent.list` + `expose.list` + catalog | control, default |
| `agents.get` | Read | `agent_get` | (directory join) | control, default |
| `agents.spawn` | Write | `agent_spawn` | `agent.spawn` | control, default |
| `agents.stop` | Write | `agent_stop` | `agent.stop` | control, default |
| `window.intent` | Write | `window_*` / intent tools | (UI bus) | control when WM exists |
| `workspace.read_file` | Read | `read_file` | — | workspace, default |
| `workspace.write_file` | Write | `write_file` | — | workspace, default |
| `workspace.exec_shell` | Write | `exec_shell` | — | workspace, default |
| `workspace.memory` | Write | `memory_*` | — | workspace, default |
| `media.voice` | Write | `voice_*` | — | media only |
| `media.render_ui` | Write | `render_ui` | — | media only |
| `skills.get` | Read | `skill_get` | — | control, default |
| `process.spawn` | Write | `process_spawn` | — | workspace (not named `spawn`) |

Full living table lives in `docs/weftos/mcp-capability-catalog.md`.

## Non-goals

- Breaking the internal agent loop’s existing tool names in one release (agent may keep internal aliases).  
- Implementing full catalog ↔ HTTP parity in one PR.  
- Exposing Admin weave methods on default MCP.  
- Replacing Ruflo MCP (orchestration remains separate).

## Implementation phases

| Phase | Cycle | Plane | Work | Exit |
|-------|-------|-------|------|------|
| **C0** | 0.8.x | **WEFT-698** | Land catalog doc + this ADR; link ADR-075 G1 | Doc complete |
| **C1** | 0.8.x | **WEFT-699** | `--profile` on `weft mcp-server`; default ≠ full; allowlist interaction | Grok attach uses default |
| **C2** | 0.8.x | **WEFT-700** | Public renames / aliases; skill_list/get; reexport guard | No `builtin__` leak; no skill flood |
| **C3** | 0.8–0.9 | **WEFT-701** | Control tools as weave façade (`--attach`) | agent_list/spawn hit live kernel |
| **C4** | 0.9.x | **WEFT-702** | WindowIntent MCP tools + media gating | ADR-075 G3 aligned |
| **C5** | 0.9.x | **WEFT-703** | Codegen or test: catalog rows ↔ registered tools | CI fails on drift |

**Overlap:** WEFT-699 implements the same outcome as ADR-075 G1 (**WEFT-693**). Prefer a single implementation PR; close the other as duplicate with a comment.

## Consequences

### Positive

- External clients get a stable, safe default.  
- Weave / MCP / HTTP can converge.  
- CNVS-like conductor (Grok + voice + GUI) shares one capability language.  
- Reduces accidental shell/FS/media exposure.

### Negative

- Migration cost for anyone relying on full dump + `spawn` name.  
- Dual names during alias period.  
- Catalog maintenance discipline required.

## References

- ADR-070, ADR-075  
- `crates/clawft-cli/src/commands/mcp_server.rs`  
- `crates/clawft-services/src/mcp/*`  
- `crates/clawft-weave/src/capability.rs`  
- `docs/guides/grok-weftos-mcp.md`  
- `docs/weftos/mcp-capability-catalog.md`  
