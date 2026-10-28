# Workspaces Guide

A workspace is a project-level container that scopes configuration, sessions,
memory, skills, agents, and hooks to a single directory tree. Each workspace
has its own `.clawft/` directory and optional `CLAWFT.md` instructions file.

## Directory Structure

When you create a workspace, clawft scaffolds the following layout:

```
my-project/
  CLAWFT.md                   # Project-level instructions (optional)
  .clawft/
    config.json               # Workspace config overrides
    MEMORY.md                 # Persistent workspace memory
    HISTORY.md                # Workspace history log
    sessions/                 # Session state files
    memory/                   # Vector memory storage
    skills/                   # Custom skill definitions
    agents/                   # Agent configurations
    hooks/                    # Lifecycle hooks
```

All resources inside `.clawft/` are scoped to this workspace. Sessions created
here do not appear in other workspaces, and memory stored here is isolated from
the global store.

## Workspace Discovery

When clawft starts, it resolves the active workspace through a three-step
discovery chain:

| Priority | Source | Notes |
|----------|--------|-------|
| 1 | `$CLAWFT_WORKSPACE` environment variable | Must point to a directory containing `.clawft/`. |
| 2 | Walk from current directory upward | Stops at the first parent with a `.clawft/` directory. |
| 3 | `~/.clawft/` | Global fallback (always exists after first run). |

The upward walk is the most common path. If you `cd` into any subdirectory of
a workspace, clawft automatically discovers the workspace root above you.

```sh
# Force a specific workspace regardless of cwd
export CLAWFT_WORKSPACE=/home/user/projects/my-api
weft workspace status
```

## CLI Commands

All workspace commands live under `weft workspace`.

### Create a workspace

```sh
weft workspace create my-project            # creates ./my-project/.clawft/
weft workspace create my-project --dir /opt # creates /opt/my-project/.clawft/
```

This creates the `.clawft/` directory tree, an empty `config.json`, starter
`CLAWFT.md`, and registers the workspace in the global registry at
`~/.clawft/workspaces.json`.

### List workspaces

```sh
weft workspace list         # registered workspaces with valid paths
weft workspace list --all   # include entries whose paths no longer exist
```

Output is a table with name, path, status, and last-accessed timestamp.

### Load a workspace

```sh
weft workspace load my-project         # by registered name
weft workspace load /path/to/project   # by filesystem path
```

Loading a workspace prints its scoped resource paths (sessions, memory, skills).

### Check status

```sh
weft workspace status
```

Shows the discovered workspace name, path, session count, whether `config.json`
and `CLAWFT.md` exist, and scoped resource directories.

### Delete a workspace

```sh
weft workspace delete my-project              # prompts; removes .clawft/ + CLAWFT.md
weft workspace delete my-project -y           # skip confirmation
weft workspace delete my-project --keep-data  # registry-only (keep files on disk)
```

By default (FR-W06), deletion removes the global registry entry **and** the
workspace markers on disk: `.clawft/` and `CLAWFT.md`. The project root and
other user source files are left alone.

Pass `--keep-data` for a registry-only delete (the pre-0.8 default).

> **Migration (WEFT-86):** older clawft versions only unregistered the
> workspace and never deleted files. Scripts that assumed a no-op file
> delete should add `--keep-data` if they still need that behavior.

### Workspace configuration

Per-workspace config overrides live in `.clawft/config.json`. Use dot-notation
keys to read and write individual values:

```sh
weft workspace config set agents.defaults.model openai/gpt-4o
weft workspace config set agents.defaults.max_tokens 4096
weft workspace config get agents.defaults.model
weft workspace config reset   # resets to empty {}
```

Values are auto-parsed: `42` becomes a number, `true`/`false` become booleans,
`null` removes the key, and everything else is stored as a string.

## Projects

A workspace is a configuration directory; a **project** is the identity the
kernel uses to tell trees apart (ADR-103). Each project has a ULID in
`<root>/.weftos/project.toml` and an index entry in
`~/.weftos/projects/<id>.toml` (set `WEFTOS_MANIFESTS_DIR` to use another
index).

```bash
weft project init --name my-api   # give the current tree an id and register it
weft project list                 # all registered projects, `missing` if the root is gone
weft project show                 # the project containing the current directory
weft project show my-api --json   # by name or id; includes the live daemon handshake
weft project seed                 # import ~/.clawft/workspaces.json
```

`init` is idempotent, works from any subdirectory of a project, and adopts the
id of an entry that `seed` already created for the same root.

### Clones and forks

The id lives in the tree, so `cp -r` of a project carries it along. On one
machine a project id belongs to one root: running `weft project init` in the
copy fails with `RootConflict` naming the original. Give the copy its own
identity with:

```bash
weft project init --fork
```

The copy gets a new id and records the old one as `parent`; the original is
untouched. `--fork` on the registered home of an id is refused unless you add
`--force`, which archives that manifest first.

### Choosing a kernel

`--project <ULID>` and `--runtime <DIR>` (global flags) select which daemon a
command talks to; `WEFTOS_PROJECT` and `WEFTOS_RUNTIME_DIR` do the same from the
environment. If the daemon that answers is bound to a different project, the
command stops and prints the command that fixes it.

## Config Merging

Configuration is resolved through a three-level merge:

```
compiled defaults  <  ~/.clawft/config.json (global)  <  .clawft/config.json (workspace)
```

Each layer overrides the one before it. The merge follows these rules:

| Type | Behavior |
|------|----------|
| Objects | Recursively merged (workspace keys override global keys). |
| Arrays | Replaced entirely (not concatenated). |
| Scalars | Right side wins. |
| `null` | Removes the key from the base config. |

Both `snake_case` and `camelCase` keys are accepted. Keys are normalized to
`snake_case` before merging, so `maxTokens` and `max_tokens` refer to the same
field.

### Example: removing a global MCP server in a workspace

If your global config defines a `slack` MCP server but a particular project
does not need it:

```json
// .clawft/config.json
{
  "tools": {
    "mcp_servers": {
      "slack": null,
      "project-db": { "command": "npx", "args": ["-y", "project-db-mcp"] }
    }
  }
}
```

The `null` removes `slack` from the merged result, while `project-db` is added.
All other global MCP servers are preserved.

## CLAWFT.md

`CLAWFT.md` is a Markdown file at the workspace root that provides project-level
instructions to the agent. It serves the same purpose as `.cursorrules` or
`CLAUDE.md` in other tools.

### Basic usage

```markdown
# My API Project

This is a REST API built with Actix-web and PostgreSQL.

## Rules

- Always use parameterized SQL queries.
- Follow the repository's existing error handling pattern.
- Run `cargo test` before suggesting changes are complete.
```

### Import syntax

Lines starting with `@` import another file inline:

```markdown
# My Project

@prompts/safety.md
@agents/researcher.md

## Project-specific instructions

...
```

Imports are resolved relative to the directory containing the CLAWFT.md file.
Imported files can themselves contain `@` imports, up to a maximum depth of 5.

### Hierarchical loading

clawft walks up from the current directory looking for `CLAWFT.md` files,
stopping at the nearest `.git` boundary. This lets you define org-wide
instructions at the repository root and project-specific instructions in
subdirectories:

```
repo/                     # .git lives here
  CLAWFT.md               # org-wide rules (loaded second)
  services/
    api/
      CLAWFT.md           # project-specific rules (loaded first, takes precedence)
```

Files are collected from most specific (closest to cwd) to most general
(highest ancestor before `.git`).

### Security constraints

- **Path traversal blocked**: Import paths containing `..` are rejected.
- **Absolute paths blocked**: Import paths like `/etc/passwd` are rejected.
- **Max depth 5**: Recursive imports are bounded to prevent circular imports.
- **Graceful failure**: If an imported file is missing, a comment is inlined
  and processing continues.

## Scoped Resources

Each workspace isolates the following resources inside `.clawft/`:

| Resource | Path | Description |
|----------|------|-------------|
| Sessions | `.clawft/sessions/` | Conversation state and history |
| Memory | `.clawft/memory/` | Vector memory and knowledge base |
| Skills | `.clawft/skills/` | Custom skill definitions |
| Agents | `.clawft/agents/` | Agent configurations |
| Hooks | `.clawft/hooks/` | Lifecycle hooks (pre-task, post-edit, etc.) |

These paths are resolved relative to the discovered workspace root, so
switching workspaces automatically switches all scoped resources.

## Examples

### Setting up a workspace for an existing project

```sh
cd ~/projects/my-api
weft workspace create my-api --dir ~/projects
cd my-api

# Customize the model for this project
weft workspace config set agents.defaults.model anthropic/claude-sonnet-4-20250514
weft workspace config set agents.defaults.max_tokens 4096

# Add project-specific MCP servers
weft workspace config set tools.mcp_servers.my-db.command npx
```

### Using CLAWFT.md with shared prompts

```
my-api/
  CLAWFT.md
  prompts/
    safety.md
    code-style.md
  .clawft/
    config.json
    ...
```

```markdown
# my-api

@prompts/safety.md
@prompts/code-style.md

## API-specific rules

- All endpoints must return JSON.
- Use 404 for missing resources, 422 for validation errors.
```

### Checking which workspace is active

```sh
weft workspace status
```

```
Workspace: my-api
  Path:       /home/user/projects/my-api
  Sessions:   3
  Has config: yes
  CLAWFT.md:  yes

Scoped resource paths:
  Sessions: /home/user/projects/my-api/.clawft/sessions
  Memory:   /home/user/projects/my-api/.clawft/memory
  Skills:   /home/user/projects/my-api/.clawft/skills
```

---

## Per-Agent Workspaces (H1)

Each agent gets its own isolated workspace directory under the agents root.
This enables per-agent configuration, personality, session history, and
skill overrides without affecting other agents.

Source: `clawft-core/src/workspace/agent.rs`,
`clawft-channels/src/plugin_host.rs`

### Directory Layout

When an agent workspace is created, the following structure is scaffolded:

```
~/.clawft/agents/<agent_id>/
  SOUL.md          # Agent personality preamble
  AGENTS.md        # Agent capabilities description
  USER.md          # User preferences for this agent
  config.toml      # Per-agent config overrides
  sessions/        # Per-agent session store
  memory/          # Per-agent memory namespace
  skills/          # Agent-specific skill overrides
  tool_state/      # Per-plugin state (see "tool_state contract" below)
```

All directories are created with `0700` permissions on Unix for security.
Files are only created if they do not already exist (idempotent operation).

#### tool_state contract

`tool_state/` is the host-managed key-value namespace exposed to
plugins via the [`KeyValueStore`] trait
(`crates/clawft-plugin/src/traits.rs`). It implements **Contract 3.1**
(Tool Plugin -> Memory) from the cross-element integration spec
(`.planning/sparc/phase4/02-improvements-overview/01-cross-element-integration.md`).

[`KeyValueStore`]: https://docs.rs/clawft-plugin/latest/clawft_plugin/trait.KeyValueStore.html

The contract:

- **Layout**: each plugin gets a sub-namespace at
  `~/.clawft/agents/<agent_id>/tool_state/<plugin_name>/`. The host is
  responsible for materializing the per-plugin subdirectory; plugins
  see only their own slice through the trait.
- **API surface**: plugins use the `ToolContext::key_value_store()`
  accessor to get a `&dyn KeyValueStore`, which provides async
  `get`, `set`, `delete`, and `list_keys` methods. The trait
  signature is in `crates/clawft-plugin/src/traits.rs:329-347`.
- **Sandbox grants**: the plugin sandbox grants read+write to
  `<agent_workspace>/tool_state/<plugin_name>/` only -- no other
  filesystem path is reachable through the trait. Cross-plugin
  reads are explicitly out of scope; share state via shared memory
  namespaces (see "Cross-Agent Memory Sharing" below) instead.
- **Idempotent setup**: the directory is created at agent-workspace
  init time even if no plugin writes to it yet. This makes the
  contract visible to operators and lets sandbox-grant rules
  resolve a stable path.

Status as of 0.7.0: the directory is created and the trait is
defined, but the only `KeyValueStore` impls in-tree are
test-fixture mocks (`MockKvStore` in plugin crates). The first
production-backed implementation (file-system-backed, scoped to the
per-agent path) is tracked under the post-0.7.0 plugin work and
referenced from the audit doc
(`.planning/reviews/0.7.0-release-gate/06-memory-workspace.md`,
row WS-O7) and Plane WEFT-94 (this commit closes the
documentation half; the runtime impl is a separate item). New
plugin authors should call through `ToolContext::key_value_store()`
and treat the backing store as eventually-real; do not write to
`tool_state/` directly.

### Workspace Creation

Agent workspaces are created on demand via `ensure_agent_workspace()`:

```rust
pub fn ensure_agent_workspace(&self, agent_id: &str) -> Result<PathBuf>
```

This method is idempotent -- if the workspace already exists, it returns the
path without modifying any files. Custom content (such as an edited
`SOUL.md`) is preserved across calls.

For template-based creation, `create_agent_workspace()` copies an entire
directory tree from a template:

```rust
pub fn create_agent_workspace(
    &self,
    agent_id: &str,
    template: Option<&Path>,
) -> Result<PathBuf>
```

If no explicit template is provided and a `~/.clawft/agents/default/`
directory exists, it is used as the template automatically.

### Agent ID Validation

Agent IDs are validated to prevent path traversal attacks:

- Must be non-empty
- Must not start with `.`
- Must not contain `/`, `\`, or null bytes
- Must not be `..`

### 3-Level Config Merge

Agent-level configuration participates in a three-level merge:

```
compiled defaults  <  ~/.clawft/config.json (global)  <  agent config.toml (per-agent)
```

Per-agent `config.toml` overrides can set model, max tokens, tool access,
and other dimensions independently. A workspace-level override (from the
project's `.clawft/config.json`) can further override agent defaults when
the agent is operating inside that workspace.

### SOUL.md Injection

The `SOUL.md` file provides per-agent personality without code changes. Its
content is injected into the Assembler pipeline stage's system prompt:

1. On first message, `SoulConfig::load()` searches for `SOUL.md` in:
   - `{workspace}/.clawft/SOUL.md`
   - `{workspace}/SOUL.md`
   - `~/.clawft/SOUL.md` (global fallback)
2. The content is appended as a `## Agent Personality (SOUL.md)` section
   in the system prompt.
3. The file is cached by modification time (`mtime`). Changes to `SOUL.md`
   on disk take effect on the next message without requiring a restart.

When no `SOUL.md` exists, the system prompt is unmodified.

### Cross-Agent Memory Sharing

Agents can share memory namespaces via symlink-based references:

```rust
pub fn link_shared_namespace(
    &self,
    exporter_id: &str,
    importer_id: &str,
    namespace: &str,
) -> Result<PathBuf>
```

This creates a symlink from the importer's `memory/` directory to the
exporter's namespace directory. The naming convention for the symlink is
`{exporter_id}--{namespace}`.

**Access rules:**

| Access | Default | Configuration |
|--------|---------|---------------|
| Read | Allowed | Symlinks are read-only by default |
| Write | Denied | Requires explicit `read_write = true` flag |

**Configuration fields:**

- `shared_namespaces`: Declares namespaces an agent exports for read-only
  cross-agent access.
- `import_namespaces`: Declares namespaces an agent imports from other agents.

**Security:**

- Both agent IDs and namespace names are validated against path traversal
- The symlink target must resolve within the agents root directory
  (canonical path check prevents escapes)
- Existing symlinks are replaced on re-link (idempotent)
- Symlink creation requires Unix (`std::os::unix::fs::symlink`)

### Filesystem Permissions

All per-agent directories are created with `0700` permissions on Unix:

- Only the owner can read, write, or enter agent directories
- This prevents other users on a shared system from accessing agent data
- On non-Unix platforms, the permission call is a no-op

### CLI Commands

```sh
# List all agents with source annotation
weft agents list

# Show one agent's definition
weft agents show <name>

# Set the active agent for the next session
weft agents use <name>
```

Creating or deleting an agent workspace from a template is not yet
implemented as a command: `weft agents` has only `list`, `show` and `use`.

### Source Files

| File | Description |
|------|-------------|
| `clawft-core/src/workspace/agent.rs` | `ensure_agent_workspace`, `create_agent_workspace`, `link_shared_namespace` |
| `clawft-channels/src/plugin_host.rs` | `SoulConfig` for SOUL.md loading and injection |
| `clawft-core/src/agent/context.rs` | Bootstrap file loading (SOUL.md, AGENTS.md, USER.md) |
