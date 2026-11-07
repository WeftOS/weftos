# WeftOS MCP capability catalog

**Authority:** [ADR-076](../adr/adr-076-mcp-tool-surface-capability-catalog.md)  
**Related:** [ADR-075](../adr/adr-075-grok-weftos-mcp-client-bridge.md) (Grok attach), [grok-weftos-mcp.md](../guides/grok-weftos-mcp.md)

Living map of product capabilities → MCP tools, weave methods, and HTTP (where known).  
**Rule:** new external-facing capability gets a row here first.

## Profiles

| Profile | Includes |
|---------|----------|
| `control` | instance + agents + window + skill_list/get + read-safe inspect |
| `workspace` | FS, shell, process_spawn, file memory, web (if policy allows) |
| `media` | voice, audio, render_ui (session-bound) |
| `default` | `control` ∪ `workspace` |
| `full` | everything + skill expansion + optional MCP re-export |

## Catalog (seed)

Status: `planned` = not yet stable on MCP wire; `partial` = exists under different name; `live` = matches catalog.

| id | class | mcp_tool (public) | weave_method | http (approx) | profiles | session_bound | status | notes |
|----|-------|-------------------|--------------|---------------|----------|---------------|--------|-------|
| `instance.status` | Read | `status` | `kernel.status` | `/api/status` | control, default | no | planned | |
| `agents.list` | Read | `agent_list` | `agent.list` + `expose.list` + `agents/catalog.json` | agents API | control, default | no | live | attach mode; joined directory, key `weftos://agent/<id>` |
| `agents.get` | Read | `agent_get` | (directory join) | agents API | control, default | no | live | attach mode; takes the stable id |
| `agents.spawn` | Write | `agent_spawn` | `agent.spawn` | — | control, default | no | planned | visible-agent policy ADR-073 |
| `agents.stop` | Write | `agent_stop` | `agent.stop` | — | control, default | no | planned | |
| `agents.chat` | Chat | `agent_chat` | `agent.chat` | — | control (opt) | no | planned | high token cost; profile flag |
| `window.spawn` | Write | `window_spawn` | UI bus | — | control, default | yes (shell) | live | WindowIntent via submit_mcp (WEFT-695/702) |
| `window.focus` | Write | `window_focus` | UI bus | — | control, default | yes | live | |
| `window.arrange` | Write | `window_arrange` | UI bus | — | control, default | yes | live | |
| `window.close` | Write | `window_close` | UI bus | — | control, default | yes | live | |
| `window.summarize` | Write | `window_summarize` | UI bus | — | control, default | yes | live | |
| `window.attach` | Write | `window_attach` | UI bus | — | control, default | yes | live | tool pane ↔ agent pane |
| `substrate.read` | Read | `substrate_read` | `substrate.read` | — | control | no | planned | |
| `substrate.list` | Read | `substrate_list` | `substrate.list` | — | control | no | planned | |
| `mcp.list` | Read | `mcp_list` | `mcp.list` | — | control | no | planned | registry inspect |
| `skills.list` | Read | `skill_list` | — | skills API | control, default | no | live | façade; not one tool per skill |
| `skills.get` | Read | `skill_get` | — | — | control, default | no | live | body-return contract |
| `workspace.read_file` | Read | `read_file` | — | — | workspace, default | no | live | sandboxed |
| `workspace.write_file` | Write | `write_file` | — | — | workspace, default | no | live | |
| `workspace.edit_file` | Write | `edit_file` | — | — | workspace, default | no | live | |
| `workspace.list_directory` | Read | `list_directory` | — | — | workspace, default | no | live | |
| `workspace.exec_shell` | Write | `exec_shell` | — | — | workspace, default | no | live | policy middleware |
| `workspace.process_spawn` | Write | `process_spawn` | — | — | workspace, default | no | live | public rename of OS `spawn`; not agent lifecycle |
| `workspace.memory_read` | Read | `memory_read` | — | memory API | workspace, default | no | live | file memory, not AgentDB |
| `workspace.memory_write` | Write | `memory_write` | — | memory API | workspace, default | no | live | |
| `workspace.web_search` | Read | `web_search` | — | — | workspace | no | live | URL policy |
| `workspace.web_fetch` | Read | `web_fetch` | — | — | workspace | no | live | |
| `media.voice_listen` | Write | `voice_listen` | — | voice API | media | yes | live | default profile: **off** |
| `media.voice_speak` | Write | `voice_speak` | — | voice API | media | yes | live | |
| `media.audio_transcribe` | Write | `audio_transcribe` | — | — | media | no | live | |
| `media.audio_synthesize` | Write | `audio_synthesize` | — | — | media | no | live | |
| `media.render_ui` | Write | `render_ui` | — | — | media | yes | live | needs canvas |
| `delegate.claude` | Write | `delegate_task` | — | — | full | no | live | not in default |
| `admin.kernel_shutdown` | Admin | — | `kernel.shutdown` | — | — | no | weave-only | never default MCP |

## Serve flags

```bash
weft mcp-server --profile default
weft mcp-server --profile control,media
weft mcp-server --profile full                  # skill expansion; no peer re-export
weft mcp-server --profile full --reexport-mcp   # peer MCP re-export; explicit only
# weft mcp-server --attach                      # façade over live kernel (WEFT-701)
```

| Flag | Default | Notes |
|------|---------|-------|
| `--profile` | `default` | `control` ∪ `workspace`; never a full dump |
| `--reexport-mcp` | off | Requires `full`; even `full` alone does **not** re-export inbound MCP peers (ADR-076 §8) |

**Public wire names:** flat product names (no `builtin__` / `skill__` prefix). OS subprocess is `process_spawn`. Skills on default/control: `skill_list` + `skill_get` only; per-skill tools only on `full`.

## Drift policy

**CI (WEFT-703 / ADR-076 C5):** `crates/clawft-cli` unit test
`catalog_default_live_tools_allowed_by_default_profile` pins catalog
**default** `status=live` wire names to `ProfileSet::product_default()`.
Fails when a live default catalog tool is missing from the allowlist, or
when media/full-only live tools leak into default.

When adding a tool or weave method:

1. Add/update a catalog row.  
2. Implement on the surfaces listed.  
3. Prefer tests that assert default profile ⊆ catalog `default` set (C5).

## Plane

| WEFT | Phase |
|------|-------|
| WEFT-698 | C0 catalog/ADR lock-in |
| WEFT-699 | C1 `--profile` (also ADR-075 G1 / WEFT-693) |
| WEFT-700 | C2 names / skills / reexport |
| WEFT-701 | C3 `--attach` weave façade |
| WEFT-702 | C4 window_* + media |
| WEFT-703 | C5 drift CI |

## History

- 2026-07-30: Seeded from ADR-076 audit (Grok MCP surface review).
- 2026-07-30: WEFT-700 (C2) — public wire: flat names, `process_spawn`, skill façades, `--reexport-mcp`.
- 2026-07-31: WEFT-695 / WEFT-702 (C4) — `window_*` MCP tools → `WindowIntent` (`submit_mcp`); media profile gating for `voice_*`/`render_ui` asserted; catalog rows `live`.
