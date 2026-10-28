# Plugin System Architecture

## Overview

clawft's plugin system provides six core extension points through the
`clawft-plugin` crate, plus runtime WASM sandboxing, skill hot-reload,
autonomous skill creation, slash-command integration, and MCP tool exposure.

| Extension Point | Trait | Purpose |
|----------------|-------|---------|
| Tools | `Tool` | Agent tool execution (e.g. web search, file I/O) |
| Channels | `ChannelAdapter` | External platform message handling (Telegram, Slack, etc.) |
| Pipeline Stages | `PipelineStage` | Custom processing stages in the agent pipeline |
| Skills | `Skill` | High-level agent capabilities with tools and instructions |
| Memory Backends | `MemoryBackend` | Pluggable memory storage (vector, KV, graph) |
| Voice Handlers | `VoiceHandler` | Voice/audio processing — **forward-compat placeholder** in 0.7.x: `pub` API surface only, no production impl, no plugin-loader path exercises it. Real impls (VAD / STT / TTS / wake-word) land in Workstream G. See `crates/clawft-plugin/src/traits.rs` (WEFT-77). |

## Plugin Manifest

Every plugin declares its capabilities, permissions, and resource limits
through a `clawft.plugin.json` manifest file:

```json
{
  "name": "my-plugin",
  "version": "0.1.0",
  "capabilities": ["tool"],
  "permissions": {
    "network": false,
    "filesystem": false,
    "env_vars": []
  },
  "resources": {
    "max_memory_mb": 64,
    "max_cpu_seconds": 10
  }
}
```

The `PluginManifest` struct in `clawft-plugin` validates and parses this file.

## Plugin Traits

### Tool

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> serde_json::Value;
    async fn execute(
        &self,
        args: serde_json::Value,
        ctx: &dyn ToolContext,
    ) -> Result<serde_json::Value, PluginError>;
}
```

### ChannelAdapter

```rust
#[async_trait]
pub trait ChannelAdapter: Send + Sync {
    fn name(&self) -> &str;
    async fn start(&mut self, host: Arc<dyn ChannelAdapterHost>) -> Result<(), PluginError>;
    async fn stop(&mut self) -> Result<(), PluginError>;
    async fn send(&self, payload: MessagePayload) -> Result<(), PluginError>;
}
```

### PipelineStage

```rust
#[async_trait]
pub trait PipelineStage: Send + Sync {
    fn stage_type(&self) -> PipelineStageType;
    fn name(&self) -> &str;
    async fn process(
        &self,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, PluginError>;
}
```

### Skill

```rust
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn tools(&self) -> Vec<Arc<dyn Tool>>;
    fn system_prompt(&self) -> Option<String>;
}
```

### MemoryBackend

```rust
#[async_trait]
pub trait MemoryBackend: Send + Sync {
    async fn store(&self, key: &str, value: &[u8]) -> Result<(), PluginError>;
    async fn retrieve(&self, key: &str) -> Result<Option<Vec<u8>>, PluginError>;
    async fn delete(&self, key: &str) -> Result<(), PluginError>;
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<String>, PluginError>;
}
```

## Supporting Types

- **`ToolContext`** -- Execution context passed to tool invocations, providing
  access to key-value storage and configuration.
- **`ChannelAdapterHost`** -- Host services available to channel adapters for
  dispatching inbound messages and querying state.
- **`PluginError`** -- Unified error type for plugin operations.
- **`MessagePayload`** -- Structured message envelope for channel communication.

---

## WASM Sandbox

Feature gate: `wasm-plugins`

The WASM plugin host uses wasmtime 29 with the WIT component model to execute
untrusted plugin code in a sandboxed environment.

### Resource Limits

| Resource | Default | Configurable |
|----------|---------|-------------|
| Fuel (CPU budget) | 1,000,000,000 instructions | Yes |
| Memory | 16 MB | Yes |
| Binary size | 300 KB max | Yes |
| Wall-clock timeout | Epoch interruption | Yes |

### Host Functions

Five host functions are exposed to WASM plugins. Each is gated by the plugin's
declared permissions:

| Function | Permission Gate | Behavior |
|----------|----------------|----------|
| `http-request` | `permissions.network` allowlist | Validates URL against the allowlist; rejects private IPs (SSRF check via `is_private_ip()`). Rate-limited. |
| `read-file` | `permissions.filesystem` paths | Canonicalizes the path; rejects symlinks resolving outside allowed directories. |
| `write-file` | `permissions.filesystem` paths | Same canonicalization and symlink rejection as `read-file`. |
| `get-env` | `permissions.env_vars` list | Returns only environment variables explicitly listed in the permission set. |
| `log` | Always available | Rate-limited to prevent log flooding. |

All host function calls are audit-logged.

### Fuel Metering and Epoch Interruption

Fuel metering tracks instruction count. When fuel is exhausted, the WASM
instance traps. Epoch interruption provides wall-clock timeout as a secondary
safeguard -- the host increments the epoch on a timer, and the engine checks
epoch boundaries during execution.

---

## Skill Loader

The skill loader (`clawft-plugin` C3) discovers and registers skills using
`serde_yaml` to parse `SKILL.md` frontmatter.

### Discovery Precedence

Skills are resolved in priority order (first match wins):

1. **Workspace** -- `./skills/` in the current project
2. **User (managed)** -- `~/.clawft/skills/`
3. **Builtin** -- bundled skills shipped with the binary

WASM-based skills are automatically registered when discovered. The loader
parses the skill manifest, validates permissions, and registers any declared
tools with the `ToolRegistry`.

---

## Skill Hot-Reload

The hot-reload system (C4) uses the `notify` crate to watch skill directories
for changes.

### Reload Process

1. File watcher detects a change in a skill directory.
2. Debounce timer prevents rapid successive reloads.
3. New skill version is loaded and validated alongside the old version.
4. Atomic swap: once the new version is ready, it replaces the old one.
   In-flight calls on the old version complete before it is dropped.
5. Skill precedence (workspace > managed > builtin) is re-evaluated.

### CLI Commands

```bash
weft skills install <path>         # Install a skill from a local path
weft skills remote-install <name>  # Install a skill from ClawHub by name
weft skills remove <name>          # Remove a user-installed skill
```

---

## Plugin Permission System

### PluginPermissions

The `PluginPermissions` struct declares what a plugin may access:

| Field | Type | Description |
|-------|------|-------------|
| `network` | `Vec<String>` | URL allowlist for `http-request` |
| `filesystem` | `Vec<PathBuf>` | Allowed directory paths for file access |
| `env_vars` | `Vec<String>` | Permitted environment variable names |
| `shell` | `bool` | Whether shell execution is allowed |

### Permission Diff (T41)

When a plugin version upgrade requests new permissions, `PermissionDiff`
computes exactly which permissions are new compared to the previously approved
set. Only the new permissions require user approval.

### PermissionStore

`PermissionStore` persists the set of approved permissions per plugin. On
upgrade, the store is consulted to determine which permissions have already been
granted.

### PermissionApprover Trait

The `PermissionApprover` trait abstracts the user consent flow. Implementations
can prompt interactively (CLI) or through a UI. The approver receives only the
diff of new permissions, not the full set.

---

## Autonomous Skill Creation

The autonomous skill creation system (C4a) detects repeated task patterns and
generates skills automatically.

### How It Works

1. The agent monitors task patterns during execution.
2. When a pattern repeats beyond a configurable threshold (default: 3
   occurrences), it triggers skill generation.
3. A `SKILL.md` file and implementation are auto-generated.
4. The skill is installed in **pending** state -- it is not active until the
   user explicitly approves it.
5. Auto-generated skills receive minimal permissions: no shell access, no
   network access, workspace-only filesystem.

### Configuration

Autonomous skill creation is **disabled by default**. Enable it in the
configuration:

```json
{
  "skills": {
    "autonomous": {
      "enabled": true,
      "pattern_threshold": 3
    }
  }
}
```

---

## Slash-Command Framework

The `SlashCommandRegistry` (C5) provides a unified command system.

- Skills can contribute slash commands that appear in `/help` output.
- Collision detection prevents two skills from registering the same command
  name. The first registration wins; conflicts are logged as warnings.
- Commands are dispatched to the owning skill's handler.

---

## MCP Skill Exposure

The MCP skill exposure layer (C6) bridges skills into the MCP tool protocol.

- `SkillToolProvider` implements the `ToolProvider` trait from
  `clawft-services`.
- Skills appear in `tools/list` responses with auto-generated JSON Schema
  derived from the skill's `parameters_schema()`.
- `tools/call` requests are routed through `skill.execute_tool()`.
- When hot-reload swaps a skill, the MCP tool listing is updated automatically.

---

## PluginHost Unification

The unified `PluginHost` (C7) manages all plugin types through a single
lifecycle controller.

- `ChannelAdapterShim` wraps existing `Channel` implementations to conform to
  the `ChannelAdapter` trait, providing backward compatibility.
- `start_all()` and `stop_all()` operate concurrently across all registered
  plugins.
- `SoulConfig` injects `SOUL.md` content into plugin contexts, allowing
  personality and behavioral instructions to propagate to plugin-hosted agents.
