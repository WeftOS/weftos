# `weft mcp add` — installing third-party MCP servers

This page covers the WeftOS-side install path for adding a third-party
MCP server (Claude Code, claude-flow, IDE bridges, etc.) into your
project's `weave.toml`. For the broader integration model — how
sessions are created, how skills scope tool visibility, how
`internal_only` hides tools from the LLM by default — see
[`mcp-integration.md`](./mcp-integration.md).

> Most agents reference `claude mcp add ...` (the Claude Desktop /
> Claude Code path). The WeftOS daemon reads its server list from
> `.weftos/weave.toml` instead, so the steps below are the daemon
> equivalent. Both tools can coexist; they read independent config.

## TL;DR

```bash
# Install claude-flow as an MCP server in this project's weave.toml.
# Pin matches package.json / .mcp.json (WEFT-684). Never @latest — schema-bearing.
weft mcp add claude-flow \
  --command "npx" \
  --arg=--no-install --arg=@claude-flow/cli --arg=mcp --arg=start \
  --internal-only
```

Then start the daemon (or `weaver kernel restart` if it's already up).
Run `weft mcp list` to confirm the session attached.

## Why we don't ship claude-flow first-party

claude-flow stays user-installed, not vendored into the WeftOS
release. See [ADR-054](../adr/adr-054-claude-flow-integration.md) for
the full rationale. The short version: vendoring adds Node / npm to
the WeftOS release matrix, and the protocol-level integration via
MCP already gives us everything we need without that cost.

## What `weft mcp add` writes

`weft mcp add` edits your project's `.weftos/weave.toml` and adds a
`[tools.mcp_servers.<name>]` table with the connection parameters.
After running the example above you will see:

```toml
[tools.mcp_servers.claude-flow]
command = "npx"
args = ["--no-install", "@claude-flow/cli", "mcp", "start"]
internalOnly = true

[tools.mcp_servers.claude-flow.env]
# (no env vars set; add API keys here)
```

The same shape works for HTTP MCP servers — set `url = "..."` and
omit `command`/`args`. The serde alias `internalOnly` matches the
field name used by Claude Desktop's `claude mcp add`, so you can
copy-paste between them.

## Sample `weave.toml` excerpt with multiple servers

```toml
# .weftos/weave.toml

[tools.mcp_servers.claude-code]
# Claude Code as an MCP client (stdio).
command = "claude"
args    = ["mcp-serve"]
internalOnly = true

[tools.mcp_servers.claude-flow]
# claude-flow user-install. Pin = package.json weftos.rufloPin (WEFT-684).
command = "npx"
args    = ["--no-install", "@claude-flow/cli", "mcp", "start"]
internalOnly = true

[tools.mcp_servers.claude-flow.env]
ANTHROPIC_API_KEY = "${env:ANTHROPIC_API_KEY}"
CLAUDE_FLOW_HOOKS = "1"

[tools.mcp_servers.plane]
# Third-party HTTP MCP server.
url = "https://mcp.plane.so/v1"
internalOnly = true

[tools.mcp_servers.plane.env]
PLANE_API_KEY = "${env:PLANE_API_KEY}"
```

`${env:VAR}` references are resolved at daemon startup. Missing
variables surface as a startup error rather than a silent empty
string, so a typo in your shell environment fails loudly.

## Setting environment variables for an MCP server

Two ways to provide API keys / tokens:

1. **Pass-through from the shell** (recommended). Reference
   `${env:NAME}` in `weave.toml`. The daemon reads `NAME` from its
   own environment when it starts the MCP server. Keep secrets in
   your shell profile or your `.envrc`, never commit them.

2. **Inline** (only for non-secret config like flags). Quote the
   value directly in the `env` table.

```toml
[tools.mcp_servers.claude-flow.env]
# Pass-through (preferred for secrets).
ANTHROPIC_API_KEY = "${env:ANTHROPIC_API_KEY}"
# Inline (non-secret).
CLAUDE_FLOW_LOG_LEVEL = "info"
```

## Internal vs external

`internalOnly = true` (the default) means the MCP session is created
and tracked, but the server's tools are NOT registered in the main
`ToolRegistry`. Skills opt-in to specific tools per-turn via their
`allowed-tools` list (see `mcp-integration.md` §4). This keeps the
LLM's context window from being flooded with hundreds of unrelated
tool schemas.

Set `internalOnly = false` only for small, focused servers whose
tools the agent should see in every request.

## Removing or disabling a server

```bash
# Remove from weave.toml
weft mcp remove claude-flow

# Or hand-edit and just delete the table.
```

## Troubleshooting

- **`weft mcp list` shows the server with status `failed`**: check
  `.weftos/runtime/kernel.log` for the handshake error. The most
  common causes are a wrong `command` path, a missing env var, or
  the upstream server not implementing
  `protocolVersion = "2025-06-18"`. The daemon now warns and
  rejects on protocol-version mismatch (WEFT-489) — see
  `mcp-integration.md` for the supported set.

- **Tools don't appear in the LLM context**: this is expected for
  `internalOnly = true` servers. The session is up, but a skill
  must opt in via `allowed-tools`. Inspect with
  `weft skills show <skill-name>`.

- **Allowlist rejects a tool you expect to be callable**: the
  PermissionFilter (WEFT-189 / WEFT-480) gates `tools/list` and
  `tools/call` on `tools.allowed_tools`. If you set that list,
  every tool you want to expose has to match a glob there. Empty
  list = back-compat permissive behavior.

## MCP served by the gateway

With the API enabled, `weft gateway` also serves MCP at `POST /mcp` on the API
port (the `full` profile), authenticated with the same daemon-issued bearer
token as the REST API (`weft token issue`; see
[API reference](../ui/api-reference.md#mcp)). Use it when one origin and one
token should cover both. `weft mcp-server --listen` still serves MCP alone,
with its own static `--token`.

**What a gateway token can do.** A gateway token is owner-equivalent
(ADR-102 D4): `/mcp` runs the `full` profile with owner scopes, including
shell, process, file-write and agent-spawn tools. The control tools attach to
the daemon over its local socket as the gateway's own user, so a token holder
can drive the daemon's admin RPCs through them. Treat a gateway token like
shell access: keep the TTL short, never send it over plain HTTP off loopback
(the gateway refuses that bind unless you opt in), and revoke it when done.

**Daemon attach.** Control tools attach to the daemon when the gateway starts.
If the daemon restarts later, the attach reconnects on the next call. If no
daemon was running at gateway start, the control tools are omitted until the
gateway is restarted. Calls are serialised: a caller waits at most 10 seconds
behind a running tool (then gets 503), and a single call is cancelled after
120 seconds.

## See also

- [`mcp-integration.md`](./mcp-integration.md) — full integration
  model: sessions, skills, allowlist, vector-stored metadata.
- [ADR-054](../adr/adr-054-claude-flow-integration.md) — why
  claude-flow stays user-installed.
- `CLAUDE.md` — `claude mcp add claude-flow ...` for users running
  Claude Desktop / Claude Code instead of the WeftOS daemon.
