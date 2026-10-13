# Configuration Guide

clawft uses a single JSON file for all configuration. Every field is optional
and falls back to a sensible default when omitted.

## Config File Location

The runtime resolves its configuration as a three-layer merge. Each layer is
loaded if present and deep-merged on top of the previous layer; later layers
win on key collisions. This matches the convention used by `git`, `npm`,
etc. (most-specific wins).

| Layer | Source | Purpose |
|-------|--------|---------|
| 1 | `weave.toml` in the current directory | Project-level defaults (versioned). |
| 2 | JSON config from the discovery chain (see below) | User-level config (per-machine). |
| 3 | `.clawft/config.json` in the current directory | Workspace overlay — most specific. |

The Layer 2 discovery chain stops at the first file found:

| Priority | Source | Notes |
|----------|--------|-------|
| 1 | `CLAWFT_CONFIG` environment variable | Absolute path to a JSON file. |
| 2 | `~/.clawft/config.json` | Recommended location. |
| 3 | `~/.nanobot/config.json` | Legacy fallback for migration. |

If no file is found at any layer, all values fall back to their compiled-in
defaults (equivalent to an empty `{}` document).

You can inspect the fully resolved configuration at any time:

```sh
weft config show              # full config as JSON
weft config section agents    # single section
weft config section gateway
```

### Layer 2 sync vs Layer 3 async asymmetry

The Layer 2 discovery (`discover_config_path` in
`crates/clawft-platform/src/config_loader.rs`) checks `~/.clawft/config.json`
and `~/.nanobot/config.json` using **synchronous** `Path::exists()` against
the real filesystem, not the injected `FileSystem` trait. The Layer 3
workspace overlay uses the **async** `fs.exists().await` path on the trait.
Keep this distinction in mind when reasoning about the loader:

- **Why the asymmetry exists.** Layer 2 paths are absolute home-dir
  candidates resolved via `dirs::home_dir()`. They are deliberately not
  routed through the platform `FileSystem` trait because the trait's
  `home_dir()` is informational, not a sandbox: a sandboxed
  `BrowserFileSystem` should not pretend a synchronous home-dir lookup
  succeeded. Layer 3 (`.clawft/config.json` in the cwd) is a relative
  path and goes through the trait so the same loader works on
  native + WASM targets.
- **Testing impact.** Tests cannot mock Layer 2 (no `FileSystem` injection
  point), so the workspace-overlay smoke test
  (`crates/clawft-platform/tests/overlay_probe.rs`) is gated
  `#[ignore]` and run manually when verifying end-to-end behaviour.
  Unit tests in `config_loader.rs` exercise Layer 3 against `MockFs`
  and treat Layer 2 as inert by pointing `home_dir()` at a path that
  does not exist on the real filesystem.
- **Forward compatibility.** Lifting Layer 2 to async would unblock the
  ignored test and remove the asymmetry, but requires either (a)
  threading the platform `FileSystem` through `discover_config_path`
  and accepting that the trait's `home_dir()` becomes load-bearing, or
  (b) introducing a separate `home_dir_resolver` capability. Either is
  a net change worth doing in a follow-up; today's loader keeps the
  surface small at the cost of one un-mockable layer.

See ADR-021 (CLI ↔ kernel compliance) for the broader principle that
platform-trait code paths must be the one mockable surface; the Layer 2
sync exception is documented here rather than in an ADR because it is a
local pragmatic choice, not a system-wide policy.

## Config File Format

The configuration file accepts both `snake_case` and `camelCase` keys. Keys
are normalized to `snake_case` internally. Unknown fields are silently ignored
for forward compatibility.

Below is a fully annotated example showing every section. Remove or omit any
section you do not need.

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-opus-4-5",
      "workspace": "~/.clawft/workspace",
      "max_tokens": 8192,
      "temperature": 0.7,
      "max_tool_iterations": 20,
      "memory_window": 50
    }
  },

  "providers": {
    "anthropic": {
      "api_key": "sk-ant-...",
      "api_base": null,
      "extra_headers": {}
    },
    "openai": {
      "api_key": "sk-..."
    },
    "elevenlabs": {
      "apiKey": ""
    },
    "openrouter": {
      "api_key": "sk-or-...",
      "api_base": "https://openrouter.ai/api/v1"
    }
  },

  "channels": {
    "telegram": {
      "enabled": true,
      "token_env": "TELEGRAM_BOT_TOKEN",
      "allow_from": ["user1"],
      "proxy": null
    },
    "slack": {
      "enabled": true,
      "mode": "socket",
      "bot_token_env": "SLACK_BOT_TOKEN",
      "app_token_env": "SLACK_APP_TOKEN",
      "webhook_path": "/slack/events",
      "user_token_read_only": true,
      "group_policy": "mention",
      "group_allow_from": [],
      "dm": {
        "enabled": true,
        "policy": "open",
        "allow_from": []
      }
    },
    "discord": {
      "enabled": true,
      "token_env": "DISCORD_BOT_TOKEN",
      "allow_from": [],
      "gateway_url": "wss://gateway.discord.gg/?v=10&encoding=json",
      "intents": 37377
    }
  },

  "gateway": {
    "host": "127.0.0.1",
    "port": 18790,
    "heartbeat_interval_minutes": 0,
    "heartbeat_prompt": "heartbeat",
    "dangerously_plain_http": false
  },

  "routing": {
    "mode": "tiered",
    "tiers": [
      {
        "name": "free",
        "models": ["gemini/gemini-2.5-flash-lite-preview-06-17"],
        "complexity_range": [0.0, 0.3],
        "cost_per_1k_tokens": 0.0,
        "max_context_tokens": 32768
      },
      {
        "name": "standard",
        "models": ["gemini/gemini-2.5-flash"],
        "complexity_range": [0.0, 0.7],
        "cost_per_1k_tokens": 0.0003,
        "max_context_tokens": 128000
      },
      {
        "name": "premium",
        "models": ["anthropic/claude-sonnet-4-5"],
        "complexity_range": [0.5, 1.0],
        "cost_per_1k_tokens": 0.003,
        "max_context_tokens": 200000
      }
    ],
    "selection_strategy": "preference_order",
    "fallback_model": "gemini/gemini-2.5-flash",
    "permissions": {
      "users": {
        "136554197234483201": { "level": 2 }
      },
      "channels": {
        "cli": { "level": 2 },
        "discord": { "level": 1 }
      }
    },
    "escalation": {
      "enabled": true,
      "threshold": 0.6,
      "max_escalation_tiers": 1
    },
    "cost_budgets": {
      "global_daily_limit_usd": 50.0,
      "global_monthly_limit_usd": 500.0,
      "tracking_persistence": true,
      "reset_hour_utc": 0
    },
    "rate_limiting": {
      "window_seconds": 60,
      "strategy": "sliding_window",
      "global_rate_limit_rpm": 0
    }
  },

  "tools": {
    "web": {
      "search": {
        "api_key": "",
        "max_results": 5
      }
    },
    "exec": {
      "timeout": 60
    },
    "restrict_to_workspace": false,
    "mcp_servers": {
      "example-server": {
        "command": "npx",
        "args": ["-y", "@example/mcp-server"],
        "env": {
          "API_KEY": "secret"
        }
      }
    }
  },

  "voice": {
    "enabled": true,
    "tts": {
      "provider": "openai",
      "model": "tts-1",
      "voice": "alloy",
      "speed": 1.0
    },
    "stt": {
      "enabled": true,
      "language": "en"
    },
    "vad": {
      "threshold": 0.5,
      "silence_timeout_ms": 1500
    },
    "wake": {
      "enabled": false,
      "phrase": "hey weft"
    }
  }
}
```

## Section Reference

### agents

Top-level agent container (see also [agents.md](./agents.md)).

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `workspace_root` | string path or null | `null` | When set, daemon identity (`SOUL.md` / `IDENTITY.md`) and tool workspace resolve from this path instead of process CWD (WEFT-83). Accepts absolute paths or `~/…`. Alias: `workspaceRoot`. |
| `defaults` | object | (see below) | Default settings applied to every agent instance. |
| `cost_budget` | object | (circuit-breaker defaults) | Per-conversation spend caps. |
| `cow_memory` | object | disabled | Per-turn COW memory checkpointing. |

### agents.defaults

Default settings applied to every agent instance.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `model` | string | `"anthropic/claude-opus-4-5"` | LLM model identifier using `provider/model` syntax. |
| `workspace` | string | `"~/.nanobot/workspace"` | Working directory for file tool operations. Tilde is expanded at runtime. Distinct from `agents.workspace_root` (daemon identity root). |
| `max_tokens` | integer | `8192` | Maximum tokens in a single LLM response. |
| `temperature` | float | `0.7` | Sampling temperature for LLM calls. |
| `max_tool_iterations` | integer | `20` | Maximum tool-use rounds per message turn. |
| `memory_window` | integer | `50` | Number of recent messages included in context. |

### providers

Credentials and endpoint overrides for LLM providers. Each provider section
has the same structure. Named providers: `anthropic`, `openai`, `openrouter`,
`deepseek`, `groq`, `zhipu`, `dashscope`, `vllm`, `gemini`, `moonshot`,
`minimax`, `aihubmix`, `openai_codex`, `xai`, `elevenlabs`, `custom`.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api_key` | string | `""` | API key for authentication. Prefer environment variables over inline keys. |
| `api_base` | string or null | `null` | Base URL override (e.g., for proxies or self-hosted endpoints). |
| `extra_headers` | object | `{}` | Additional HTTP headers sent with every request to this provider. |

The `model` field in `agents.defaults` uses a `provider/model` prefix to route
requests. For example, `"anthropic/claude-opus-4-5"` routes to the `anthropic`
provider and strips the prefix before calling the API.

Built-in provider routing:

| Provider | Prefix | API Key Env Var | Base URL |
|----------|--------|-----------------|----------|
| OpenAI | `openai/` | `OPENAI_API_KEY` | `https://api.openai.com/v1` |
| Anthropic | `anthropic/` | `ANTHROPIC_API_KEY` | `https://api.anthropic.com/v1` |
| Groq | `groq/` | `GROQ_API_KEY` | `https://api.groq.com/openai/v1` |
| DeepSeek | `deepseek/` | `DEEPSEEK_API_KEY` | `https://api.deepseek.com/v1` |
| Mistral | `mistral/` | `MISTRAL_API_KEY` | `https://api.mistral.ai/v1` |
| Together | `together/` | `TOGETHER_API_KEY` | `https://api.together.xyz/v1` |
| OpenRouter | `openrouter/` | `OPENROUTER_API_KEY` | `https://openrouter.ai/api/v1` |
| Gemini | `gemini/` | `GOOGLE_GEMINI_API_KEY` | `https://generativelanguage.googleapis.com/v1beta/openai` |
| xAI | `xai/` | `XAI_API_KEY` | `https://api.x.ai/v1` |

### gateway

HTTP server and heartbeat settings.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `host` | string | `"127.0.0.1"` | Bind address for the HTTP server and API. Loopback by default; see `dangerously_plain_http` for anything else. |
| `port` | integer | `18790` | Listen port. |
| `api_enabled` | boolean | `false` | Serve the REST/WebSocket API. |
| `api_port` | integer | `18789` | API listen port. |
| `cors_origins` | string[] | `["http://localhost:5173"]` | Allowed CORS origins for the API. |
| `dangerously_plain_http` | boolean | `false` | Allow the API on a non-loopback `host` over plain HTTP. Same as `weft gateway --dangerously-plain-http`. |
| `static_dir` (`staticDir`) | string | none | Built cog-manager web console, served under `/console/` (never the root). Same as `weft gateway --static-dir <dir>`. |
| `console_connect_src` (`consoleConnectSrc`) | string[] | `[]` | Extra `connect-src` origins for the `/console/` page (the project's Cog Host and Seeds), each a bare origin such as `http://192.168.1.50:8081`. Anything else is refused at startup. The API keeps its own CSP. |
| `tailnet_identity` (`tailnetIdentity`) | object | disabled | Tailnet-identity token mint, see below. |
| `heartbeat_interval_minutes` | integer | `0` | Minutes between heartbeat messages. `0` disables heartbeats. |
| `heartbeat_prompt` | string | `"heartbeat"` | Prompt text sent on each heartbeat tick. |

**API authentication.** Every API route except `GET /api/health` needs a
bearer token issued by the kernel daemon (`weft token issue`, ADR-102); the
daemon must be running. See
[API reference: Authentication](../ui/api-reference.md#authentication).

**TLS guard.** A bearer token is owner-equivalent, and the gateway has no TLS
of its own. With the API enabled, a non-loopback `host` (for example
`0.0.0.0`) is refused at startup unless `dangerously_plain_http` is set or
`--dangerously-plain-http` is passed, which states that TLS is terminated in
front of the gateway (a reverse proxy). Loopback binds are unaffected.

**Project console.** With `static_dir` set the gateway serves the console at
`/console/` (unknown paths fall back to `index.html`) under a CSP of
`connect-src 'self'` plus `console_connect_src`, `frame-ancestors 'none'`.

`tailnet_identity` lets a console on the tailnet get a token without anyone
pasting one:

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Mounts `POST /api/console/token`. Off: the route does not exist (404). |
| `allowed_logins` (`allowedLogins`) | string[] | `[]` | Tailscale login names (`user@domain`, case-insensitive). Empty means nobody. |
| `ttl_secs` (`ttlSecs`) | integer | `900` | Lifetime of a minted token, capped at the authority maximum (24 h). |
| `tailscale_bin` (`tailscaleBin`) | string | `tailscale` | Binary used for `tailscale whois --json`. |

```json
{ "gateway": { "staticDir": "/opt/weftos/console", "consoleConnectSrc": ["http://192.168.1.50:8081"],
  "tailnetIdentity": { "enabled": true, "allowedLogins": ["me@example.com"], "ttlSecs": 900 } } }
```

Security model: the tailnet is the boundary. The route takes the TCP peer
address (never `X-Forwarded-For`), requires it to be in `100.64.0.0/10` or
`fd7a:115c:a1e0::/48`, asks the local tailscaled who it is (`tailscale whois`,
3 s timeout, no shell) and checks the login against the allowlist. It mints
only a read-only token confined to the one project in the request, through the
daemon, valid for `ttl_secs`. It needs `Content-Type: application/json`, and a
request that carries an `Origin` must be same-origin (or on `cors_origins`), so
a web page elsewhere cannot make a tailnet user's browser mint. It shares the
auth rate limit (10 per minute per peer). The login and project are logged;
the token never is. Bind the gateway to the tailnet address and keep
`dangerously_plain_http` for that case only: WireGuard encrypts the hop.

### channels

Each channel section follows its own schema. All channels share an `enabled`
boolean that defaults to `false`.

### tools

Top-level tool configuration.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `restrict_to_workspace` | boolean | `false` | When `true`, all file tools are sandboxed to the workspace directory. |

#### tools.web.search

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `api_key` | string | `""` | Search provider API key (e.g., Brave Search). |
| `max_results` | integer | `5` | Maximum number of search results returned. |

#### tools.exec

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `timeout` | integer | `60` | Command execution timeout in seconds. |

### voice

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable voice features globally. |

#### voice.tts

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable text-to-speech. |
| `provider` | string | `"local"` | TTS provider: `"local"` (default, offline), `"local-stub"`, `"browser"` (UI Web Speech), `"openai"`, or `"elevenlabs"`. See WEFT-238 / voice guide. |
| `model` | string | (varies) | TTS model. Defaults: `tts-1` (OpenAI), `eleven_multilingual_v2` (ElevenLabs). |
| `voice` | string | (varies) | Voice ID. Defaults: `alloy` (OpenAI), `Rachel` (ElevenLabs). |
| `speed` | float | `1.0` | Speaking speed multiplier (0.25 - 4.0). |

> **Note:** API keys are resolved from `providers.openai.api_key` or
> `providers.elevenlabs.api_key`. If the config key is empty, the corresponding
> environment variable is used as fallback (`OPENAI_API_KEY` or
> `ELEVENLABS_API_KEY`).

#### voice.stt

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `true` | Enable speech-to-text. |
| `model` | string | `"sherpa-onnx-streaming-zipformer-en-20M"` | STT model name. |
| `language` | string | `""` | Language code. Empty = auto-detect. |

#### voice.vad

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `threshold` | float | `0.5` | VAD activation threshold (0.0 - 1.0). |
| `silence_timeout_ms` | integer | `1500` | Silence duration in ms before speech end. |
| `min_speech_ms` | integer | `250` | Minimum speech duration to trigger processing. |

#### voice.wake

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable wake word detection. |
| `phrase` | string | `"hey weft"` | Wake word phrase. |
| `sensitivity` | float | `0.5` | Detection sensitivity (0.0 - 1.0). |

### routing

The routing section controls model selection, cost management, and per-user
permissions. When omitted entirely, the system defaults to `mode = "static"`
which uses the model from `agents.defaults.model` for every request.

Set `mode` to `"tiered"` to enable complexity-based routing, where the pipeline
classifies each request and selects a model tier accordingly.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mode` | string | `"static"` | `"static"` (Level 0 -- single model) or `"tiered"` (Level 1 -- complexity-based routing). |
| `tiers` | array | `[]` | Model tier definitions, ordered cheapest to most expensive. Only used in tiered mode. |
| `selection_strategy` | string | `null` | How to pick among multiple models within a tier: `"preference_order"`, `"round_robin"`, `"lowest_cost"`, or `"random"`. |
| `fallback_model` | string | `null` | Model used when all tiers or budgets are exhausted. Format: `"provider/model"`. |
| `permissions` | object | `{}` | Permission level defaults and per-user/channel overrides. |
| `escalation` | object | `{}` | Complexity-based escalation settings. |
| `cost_budgets` | object | `{}` | Global cost budget limits. |
| `rate_limiting` | object | `{}` | Rate limiting settings. |

#### routing.tiers

Each tier groups models at a similar cost/capability level. Tiers are evaluated
from cheapest to most expensive. Complexity ranges may overlap -- the router
picks the best tier the user is permitted and can afford.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `name` | string | `""` | Tier name (e.g., `"free"`, `"standard"`, `"premium"`, `"elite"`). |
| `models` | string[] | `[]` | Models available in this tier, in preference order. Format: `"provider/model"`. |
| `complexity_range` | [float, float] | `[0.0, 1.0]` | Complexity range this tier covers. Each value is 0.0-1.0. |
| `cost_per_1k_tokens` | float | `0.0` | Approximate cost per 1K tokens (blended input/output) in USD. Used for budget tracking. |
| `max_context_tokens` | integer | `8192` | Maximum context window for models in this tier. The pipeline's context assembler uses the largest value across all tiers as its truncation budget. |

Example with three tiers:

```json
{
  "routing": {
    "mode": "tiered",
    "tiers": [
      {
        "name": "free",
        "models": ["gemini/gemini-2.5-flash-lite-preview-06-17"],
        "complexity_range": [0.0, 0.3],
        "cost_per_1k_tokens": 0.0,
        "max_context_tokens": 32768
      },
      {
        "name": "standard",
        "models": ["gemini/gemini-2.5-flash"],
        "complexity_range": [0.0, 0.7],
        "cost_per_1k_tokens": 0.0003,
        "max_context_tokens": 128000
      },
      {
        "name": "premium",
        "models": ["anthropic/claude-sonnet-4-5"],
        "complexity_range": [0.5, 1.0],
        "cost_per_1k_tokens": 0.003,
        "max_context_tokens": 200000
      }
    ],
    "selection_strategy": "preference_order",
    "fallback_model": "gemini/gemini-2.5-flash"
  }
}
```

> **Note on `max_context_tokens`:** This value controls how much conversation
> history the context assembler keeps. It is the *input* context window, not
> the output token limit (`agents.defaults.max_tokens`). The assembler uses the
> largest `max_context_tokens` across all configured tiers as its budget.

#### routing.permissions

Permissions use three built-in levels with per-user and per-channel overrides.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `zero_trust` | object | `{}` | Level 0 defaults. Most restrictive. Applied to unknown/unauthenticated users. |
| `user` | object | `{}` | Level 1 defaults. Standard access. |
| `admin` | object | `{}` | Level 2 defaults. Full access. |
| `users` | object | `{}` | Per-user overrides, keyed by sender ID (e.g., `"alice_telegram_123"`). |
| `channels` | object | `{}` | Per-channel overrides, keyed by channel name (e.g., `"cli"`, `"discord"`). |

**Permission resolution order:** built-in defaults -> level config -> per-user
override -> per-channel override. Later layers win for any field they specify.

**Built-in level defaults:**

| Dimension | Level 0 (zero_trust) | Level 1 (user) | Level 2 (admin) |
|-----------|---------------------|----------------|-----------------|
| `max_tier` | `"free"` | (config) | (config) |
| `max_context_tokens` | 4096 | (config) | (config) |
| `max_output_tokens` | 1024 | (config) | (config) |
| `rate_limit` (rpm) | 10 | (config) | 0 (unlimited) |
| `streaming_allowed` | false | (config) | (config) |
| `escalation_allowed` | false | (config) | (config) |
| `escalation_threshold` | 1.0 (never) | (config) | (config) |
| `model_override` | false | (config) | (config) |
| `cost_budget_daily_usd` | $0.10 | (config) | 0.0 (unlimited) |
| `cost_budget_monthly_usd` | $2.00 | (config) | 0.0 (unlimited) |

Each level or override object supports these fields (all optional -- unset
fields inherit from the resolved level):

| Field | Type | Description |
|-------|------|-------------|
| `level` | integer | Permission level (0, 1, or 2). |
| `max_tier` | string | Highest tier name the user can access. |
| `model_access` | string[] | Explicit model allowlist. Empty = all models in allowed tiers. |
| `model_denylist` | string[] | Models explicitly denied even if tier allows. |
| `tool_access` | string[] | Tool names this user can invoke. `["*"]` = all tools. |
| `tool_denylist` | string[] | Tools explicitly denied even if `tool_access` allows. |
| `max_context_tokens` | integer | Maximum input context tokens. |
| `max_output_tokens` | integer | Maximum output tokens per response. |
| `rate_limit` | integer | Requests per minute. 0 = unlimited. |
| `streaming_allowed` | boolean | Whether SSE streaming is allowed. |
| `escalation_allowed` | boolean | Whether complexity-based escalation to higher tiers is allowed. |
| `escalation_threshold` | float | Complexity threshold (0.0-1.0) above which escalation triggers. |
| `model_override` | boolean | Whether the user can manually select a model. |
| `cost_budget_daily_usd` | float | Daily cost budget in USD. 0.0 = unlimited. |
| `cost_budget_monthly_usd` | float | Monthly cost budget in USD. 0.0 = unlimited. |
| `custom_permissions` | object | Extensible key-value pairs for custom permission dimensions. |

Example with per-user and per-channel overrides:

```json
{
  "routing": {
    "permissions": {
      "users": {
        "136554197234483201": { "level": 2 }
      },
      "channels": {
        "cli": { "level": 2 },
        "discord": { "level": 1 }
      }
    }
  }
}
```

The CLI channel automatically gets admin-level permissions (level 2) with
`sender_id = "local"`. When no `AuthContext` is present on a request, zero-trust
defaults apply.

#### routing.escalation

Controls whether the router can automatically promote a request to a higher
model tier when complexity exceeds a threshold.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Whether escalation is enabled globally. |
| `threshold` | float | `0.6` | Default complexity threshold for escalation (0.0-1.0). |
| `max_escalation_tiers` | integer | `1` | Maximum number of tiers a request can jump beyond the user's `max_tier`. |

Example:

```json
{
  "routing": {
    "escalation": {
      "enabled": true,
      "threshold": 0.6,
      "max_escalation_tiers": 1
    }
  }
}
```

#### routing.cost_budgets

System-wide spending limits that apply regardless of individual user budgets.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `global_daily_limit_usd` | float | `0.0` | Global daily spending limit in USD. 0.0 = unlimited. |
| `global_monthly_limit_usd` | float | `0.0` | Global monthly spending limit in USD. 0.0 = unlimited. |
| `tracking_persistence` | boolean | `false` | Whether to persist cost tracking data to disk across restarts. |
| `reset_hour_utc` | integer | `0` | Hour (0-23 UTC) at which daily budgets reset. |

Example:

```json
{
  "routing": {
    "cost_budgets": {
      "global_daily_limit_usd": 50.0,
      "global_monthly_limit_usd": 500.0,
      "tracking_persistence": true,
      "reset_hour_utc": 0
    }
  }
}
```

#### routing.rate_limiting

Controls the sliding-window rate limiter. Per-user limits are defined in the
permission level config (`rate_limit` field); these settings control the global
window and strategy.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `window_seconds` | integer | `60` | Window size in seconds for rate limit calculations. |
| `strategy` | string | `"sliding_window"` | Rate limiting strategy: `"sliding_window"` or `"fixed_window"`. |
| `global_rate_limit_rpm` | integer | `0` | Global rate limit in requests per minute across all users. 0 = unlimited. Checked before per-user limits. |

Example:

```json
{
  "routing": {
    "rate_limiting": {
      "window_seconds": 60,
      "strategy": "sliding_window",
      "global_rate_limit_rpm": 120
    }
  }
}
```

## Channel Setup

### Telegram

1. Create a bot via [@BotFather](https://t.me/BotFather) on Telegram.
2. Copy the bot token.
3. Add the following to your config file:

```json
{
  "channels": {
    "telegram": {
      "enabled": true,
      "token_env": "TELEGRAM_BOT_TOKEN"
    }
  }
}
```

Or with an inline token (not recommended for shared configs):

```json
{
  "channels": {
    "telegram": {
      "enabled": true,
      "token": "123456789:ABCdef..."
    }
  }
}
```

**All fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable or disable the Telegram channel. |
| `token` | string | `""` | Bot API token from BotFather. |
| `token_env` | string or null | `null` | Environment variable holding the bot token. Used when `token` is empty. |
| `allow_from` | string[] | `[]` | Restrict to these user IDs or usernames. Empty allows all users. |
| `proxy` | string or null | `null` | HTTP or SOCKS5 proxy URL for regions where Telegram is restricted. |

### Slack

Slack uses Socket Mode, which requires both a Bot Token and an App-Level Token.

1. Create a Slack app at [api.slack.com/apps](https://api.slack.com/apps).
2. Enable **Socket Mode** under Settings and generate an App-Level Token
   (`xapp-...`) with the `connections:write` scope.
3. Under **OAuth & Permissions**, add the required Bot Token Scopes
   (`chat:write`, `channels:history`, `groups:history`, `im:history`,
   `mpim:history`, `app_mentions:read`).
4. Install the app to your workspace and copy the Bot User OAuth Token
   (`xoxb-...`).
5. Under **Event Subscriptions**, subscribe to `message.channels`,
   `message.groups`, `message.im`, `message.mpim`, and `app_mention`.

```json
{
  "channels": {
    "slack": {
      "enabled": true,
      "bot_token_env": "SLACK_BOT_TOKEN",
      "app_token_env": "SLACK_APP_TOKEN"
    }
  }
}
```

Or with inline tokens:

```json
{
  "channels": {
    "slack": {
      "enabled": true,
      "bot_token": "xoxb-...",
      "app_token": "xapp-..."
    }
  }
}
```

**All fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable or disable the Slack channel. |
| `mode` | string | `"socket"` | Connection mode. Only `"socket"` is currently supported. |
| `bot_token` | string | `""` | Bot User OAuth Token (`xoxb-...`). |
| `bot_token_env` | string or null | `null` | Environment variable holding the bot token. Used when `bot_token` is empty. |
| `app_token` | string | `""` | App-Level Token (`xapp-...`) for Socket Mode. |
| `app_token_env` | string or null | `null` | Environment variable holding the app token. Used when `app_token` is empty. |
| `webhook_path` | string | `"/slack/events"` | Webhook path for event subscriptions (future use). |
| `user_token_read_only` | boolean | `true` | Whether the user token is treated as read-only. |
| `group_policy` | string | `"mention"` | Group message policy: `"mention"` (respond when @-mentioned), `"open"` (respond to all), or `"allowlist"` (respond only in listed channels). |
| `group_allow_from` | string[] | `[]` | Channel IDs permitted when `group_policy` is `"allowlist"`. |

**DM sub-section (`channels.slack.dm`):**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `true` | Whether direct messages are accepted. |
| `policy` | string | `"open"` | DM access policy: `"open"` or `"allowlist"`. |
| `allow_from` | string[] | `[]` | Slack user IDs permitted when policy is `"allowlist"`. |

### Discord

1. Create an application at the
   [Discord Developer Portal](https://discord.com/developers/applications).
2. Under **Bot**, create a bot and copy its token.
3. Enable the **Message Content** privileged intent.
4. Generate an invite URL under **OAuth2 > URL Generator** with the `bot`
   scope and `Send Messages` + `Read Message History` permissions.
5. Invite the bot to your server.

```json
{
  "channels": {
    "discord": {
      "enabled": true,
      "token_env": "DISCORD_BOT_TOKEN"
    }
  }
}
```

Or with an inline token:

```json
{
  "channels": {
    "discord": {
      "enabled": true,
      "token": "MTIz..."
    }
  }
}
```

**All fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | boolean | `false` | Enable or disable the Discord channel. |
| `token` | string | `""` | Bot token from the Developer Portal. |
| `token_env` | string or null | `null` | Environment variable holding the bot token. Used when `token` is empty. |
| `allow_from` | string[] | `[]` | Restrict to these user IDs. Empty allows all users. |
| `gateway_url` | string | `"wss://gateway.discord.gg/?v=10&encoding=json"` | WebSocket gateway URL. Override only for testing. |
| `intents` | integer | `37377` | Gateway intents bitmask. Default = GUILDS (1) \| GUILD_MESSAGES (512) \| DIRECT_MESSAGES (4096) \| MESSAGE_CONTENT (32768, **privileged** — enable in Developer Portal or Discord closes with 4014). `0` is rejected at startup. See [channels.md § Gateway Intents](channels.md#56-gateway-intents). |

### Additional Channels

clawft also supports WhatsApp (via WebSocket bridge), Feishu/Lark, DingTalk,
Mochat, Email (IMAP + SMTP), and QQ. Each follows the same pattern of
`enabled` plus channel-specific credentials. Refer to the source type
definitions in `clawft-types` for the full field list.

Unknown channel names are captured as extension data and silently ignored,
allowing forward compatibility with future channel plugins.

## MCP Server Configuration

MCP (Model Context Protocol) servers extend the tool system with external
capabilities. Servers are defined as named entries under `tools.mcp_servers`.

Each entry supports two transport modes:

**Stdio transport** (most common) -- the runtime spawns the server as a child
process and communicates over stdin/stdout:

```json
{
  "tools": {
    "mcp_servers": {
      "filesystem": {
        "command": "npx",
        "args": ["-y", "@modelcontextprotocol/server-filesystem", "/path/to/dir"],
        "env": {}
      }
    }
  }
}
```

**HTTP transport** -- the runtime connects to an already-running server over
Streamable HTTP:

```json
{
  "tools": {
    "mcp_servers": {
      "remote-tools": {
        "url": "http://localhost:8080/mcp"
      }
    }
  }
}
```

**MCP server fields:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `command` | string | `""` | Executable to run (stdio transport). |
| `args` | string[] | `[]` | Arguments passed to the command. |
| `env` | object | `{}` | Environment variables set for the child process. |
| `url` | string | `""` | Streamable HTTP endpoint URL (HTTP transport). |

The server name (the JSON key) is used for tool namespacing. A server named
`"filesystem"` exposes tools prefixed with `filesystem__`.

## Delegation & Multi-Agent

The `delegation` section controls task routing between local execution and
Claude AI (`DelegationEngine` + `ClaudeDelegator`). Multi-agent orchestration
(claude-flow, external tools) uses **MCP servers + skills**, not a subprocess
Flow target.

Source of truth: `crates/clawft-types/src/delegation.rs` and
`docs/reference/config.md` § delegation.

### delegation

```json
{
  "delegation": {
    "claude_enabled": true,
    "claudeModel": "claude-sonnet-4-20250514",
    "maxTurns": 10,
    "maxTokens": 4096,
    "rules": [
      {
        "pattern": "(?i)deploy|orchestrate|swarm",
        "target": "claude"
      },
      {
        "pattern": "(?i)^list\\b",
        "target": "local"
      }
    ],
    "excludedTools": ["exec_shell"]
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `claude_enabled` | boolean | see note | Enable Claude as a delegation backend. (`Default` is `true`; bare `{}` via serde is `false`.) |
| `claudeModel` / `claude_model` | string | `"claude-sonnet-4-20250514"` | Claude model for delegated tasks. |
| `maxTurns` / `max_turns` | integer | `10` | Maximum conversation turns per delegated task. |
| `maxTokens` / `max_tokens` | integer | `4096` | Token limit per Claude response. |
| `rules` | array | `[]` | Ordered regex rules; first match wins. Targets: `local`, `claude`, `auto`. |
| `excludedTools` / `excluded_tools` | string[] | `[]` | Tool names that should never be delegated. |

**Retired (WEFT-179):** `flow_enabled`, `claudeFlowEnabled`,
`claude_flow_enabled`, and nested `delegation.flow` subprocess settings are
not supported. Legacy rule `"target": "flow"` still deserializes as `claude`.
For multi-agent coordination, configure an MCP server (e.g. claude-flow) and
expose tools via skills — see `docs/guides/mcp-integration.md`.

### routing.planning

The planning section configures the `PlanningRouter`, which provides
structured reasoning strategies for multi-step tasks. The router supports
two strategies with configurable guard rails to prevent runaway execution.

```json
{
  "routing": {
    "planning": {
      "strategy": "react",
      "max_depth": 10,
      "max_cost_usd": 1.0,
      "step_timeout_seconds": 60,
      "circuit_breaker_no_ops": 3
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `strategy` | string | `"react"` | Planning strategy: `"react"` (step-by-step reasoning with observe-think-act cycles) or `"plan_and_execute"` (generate a full plan first, then execute steps sequentially). |
| `max_depth` | integer | `10` | Maximum number of planning steps. Execution terminates with partial results when exceeded. |
| `max_cost_usd` | float | `1.0` | Maximum cost budget in USD for a single planning session. Tracked via token usage from the routing cost system. |
| `step_timeout_seconds` | integer | `60` | Wall-clock timeout in seconds for each individual planning step. |
| `circuit_breaker_no_ops` | integer | `3` | Number of consecutive no-op steps (steps that produce no observable state change) before the circuit breaker triggers. When triggered, the planning session aborts and returns partial results with a `TerminationReason`. |

**Guard rails:** `PlanningRouter.check_guard_rails()` is called after each
step and returns a `TerminationReason` if any limit is exceeded:

| TerminationReason | Trigger |
|-------------------|---------|
| `MaxDepthReached` | Step count exceeds `max_depth` |
| `BudgetExhausted` | Cumulative cost exceeds `max_cost_usd` |
| `StepTimeout` | A single step exceeds `step_timeout_seconds` |
| `CircuitBreakerTripped` | Consecutive no-op steps reach `circuit_breaker_no_ops` |

When termination occurs, `explain_termination()` produces a human-readable
summary of the partial results and the reason for stopping.

### routing.agents

The agent routing table controls how incoming requests are dispatched to
named agents based on keyword matching and channel origin. Routes are
evaluated in order (first match wins).

```json
{
  "routing": {
    "agents": {
      "routes": [
        {
          "name": "code-agent",
          "match": {
            "keywords": ["code", "implement", "fix"],
            "channels": ["cli"]
          },
          "model": "anthropic/claude-sonnet-4-5",
          "workspace": "~/.clawft/agents/code-agent/"
        },
        {
          "name": "research-agent",
          "match": {
            "keywords": ["research", "search", "find"],
            "channels": ["cli", "slack"]
          },
          "model": "anthropic/claude-opus-4-5",
          "workspace": "~/.clawft/agents/research-agent/"
        }
      ],
      "catch_all": "default-agent",
      "reject_unmatched": false
    }
  }
}
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `routes` | array | `[]` | Ordered list of agent route definitions. First matching route wins. |
| `catch_all` | string | `"default-agent"` | Agent name used when no route matches and `reject_unmatched` is `false`. |
| `reject_unmatched` | boolean | `false` | When `true`, requests that match no route are rejected with a warning log instead of falling through to the catch-all agent. |

**Route fields:**

| Field | Type | Description |
|-------|------|-------------|
| `name` | string | Agent name. Used as the routing target and workspace identifier. |
| `match.keywords` | string[] | Keywords to match against the request content (case-insensitive substring match). |
| `match.channels` | string[] | Channel names this route applies to (e.g., `"cli"`, `"slack"`, `"discord"`). Empty matches all channels. |
| `model` | string | LLM model override for this agent. Falls back to `agents.defaults.model` if omitted. |
| `workspace` | string | Workspace directory for this agent. Provides per-agent file isolation. |

## Environment Variables

| Variable | Description |
|----------|-------------|
| `CLAWFT_CONFIG` | Override the config file path. Takes priority over all file-based discovery. |
| `RUST_LOG` | Controls log verbosity via `tracing`'s `EnvFilter` syntax. Examples: `info`, `debug`, `clawft_core=trace`, `clawft_llm=debug,info`. |
| **Provider API Keys** | |
| `OPENAI_API_KEY` | API key for OpenAI models. |
| `ANTHROPIC_API_KEY` | API key for Anthropic models. |
| `GROQ_API_KEY` | API key for Groq models. |
| `DEEPSEEK_API_KEY` | API key for DeepSeek models. |
| `MISTRAL_API_KEY` | API key for Mistral models. |
| `TOGETHER_API_KEY` | API key for Together AI models. |
| `OPENROUTER_API_KEY` | API key for OpenRouter gateway. |
| `GOOGLE_GEMINI_API_KEY` | API key for Google Gemini models. |
| `XAI_API_KEY` | API key for xAI (Grok) models. |
| **Channel Tokens** | |
| `DISCORD_BOT_TOKEN` | Bot token for the Discord channel. Referenced via `token_env` in the channel config. |
| `SLACK_BOT_TOKEN` | Bot User OAuth Token for the Slack channel. Referenced via `bot_token_env`. |
| `SLACK_APP_TOKEN` | App-Level Token for Slack Socket Mode. Referenced via `app_token_env`. |
| `TELEGRAM_BOT_TOKEN` | Bot token for the Telegram channel. Referenced via `token_env`. |

Provider API keys can be set either in the config file (`providers.<name>.api_key`)
or as environment variables. Environment variables are generally preferred to
avoid storing secrets in files.

Channel tokens support the same pattern via `token_env` / `bot_token_env` /
`app_token_env` fields. When the inline token is empty, the runtime reads the
named environment variable instead. This keeps secrets out of the config file.

## Security Policy

clawft includes configurable security policies for command execution and URL
access. These policies protect against prompt injection attacks where malicious
instructions in user content could trick the agent into executing dangerous
operations.

### Command Execution Policy

The `tools.commandPolicy` section controls which commands the `exec_shell` and
`spawn` tools can execute.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `mode` | string | `"allowlist"` | `"allowlist"` or `"denylist"` |
| `allowlist` | string[] | `[]` | Permitted commands (overrides defaults) |
| `denylist` | string[] | `[]` | Blocked patterns (overrides defaults) |

**Default allowlist** (used when `allowlist` is empty):
`echo`, `cat`, `ls`, `pwd`, `head`, `tail`, `wc`, `grep`, `find`, `sort`,
`uniq`, `diff`, `date`, `env`, `true`, `false`, `test`

Example -- expand the allowlist:
```json
{
  "tools": {
    "commandPolicy": {
      "mode": "allowlist",
      "allowlist": ["echo", "cat", "ls", "pwd", "python3", "node", "cargo"]
    }
  }
}
```

Example -- use denylist mode (less secure, more permissive):
```json
{
  "tools": {
    "commandPolicy": {
      "mode": "denylist",
      "denylist": ["rm -rf /", "sudo ", "mkfs", "dd if="]
    }
  }
}
```

### URL Safety Policy (SSRF Protection)

The `tools.urlPolicy` section controls which URLs the `web_fetch` tool can
access. By default, requests to private networks, loopback addresses, and
cloud metadata endpoints are blocked.

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `enabled` | bool | `true` | Enable URL safety validation |
| `allowPrivate` | bool | `false` | Allow private/internal IPs |
| `allowedDomains` | string[] | `[]` | Domains that bypass checks |
| `blockedDomains` | string[] | `[]` | Additional blocked domains |

Blocked by default:
- Private networks: `10.0.0.0/8`, `172.16.0.0/12`, `192.168.0.0/16`
- Loopback: `127.0.0.0/8`, `::1`
- Link-local: `169.254.0.0/16`, `fe80::/10`
- Cloud metadata: `169.254.169.254`, `metadata.google.internal`

Example -- allow specific internal services:
```json
{
  "tools": {
    "urlPolicy": {
      "enabled": true,
      "allowedDomains": ["api.internal.corp", "vault.internal.corp"]
    }
  }
}
```

## Workspace Bootstrap Files

clawft loads optional Markdown files from the workspace directory to
customize the system prompt and agent behavior. These files are searched
in the workspace root first, then in the `.clawft/` subdirectory.

| File | Purpose |
|------|---------|
| `SOUL.md` | Primary identity preamble. Overrides the default system prompt identity section. |
| `IDENTITY.md` | Alternative identity file. Used if `SOUL.md` is absent. |
| `AGENTS.md` | Agent definitions and team structure. |
| `USER.md` | User preferences and context. |
| `TOOLS.md` | Tool usage guidelines and restrictions. |

### Identity Override Behavior

If either `SOUL.md` or `IDENTITY.md` is present, the default hardcoded
identity preamble is replaced. The priority is:

1. **`SOUL.md`** is included first (if it exists).
2. **`IDENTITY.md`** is included second (if it exists).
3. Configuration context (model, workspace, available tools) is always appended.

If neither file exists, the built-in default identity is used, which
identifies the assistant as "clawft" with the configured agent name and model.

All bootstrap files are cached by modification time (mtime). The cache is
checked on each system prompt build, so changes to these files take effect
on the next message without restarting the process.

## Feature Flags

Compile-time feature flags enable optional capabilities that are not included
in the default build.

### vector-memory

Enables the vector memory subsystem, which provides `IntelligentRouter`,
`VectorStore`, and `SessionIndexer` for semantic search over session history.

```sh
cargo install clawft-cli --features vector-memory
```

Or when building from source:

```sh
cargo build --release --features vector-memory
```

This flag is propagated from `clawft-cli` to `clawft-core`. It is off by
default to keep the baseline binary small and avoid unnecessary dependencies.

## Mesh networking (`kernel.mesh`)

Cross-node mesh is configured under `kernel.mesh` (TOML `[kernel.mesh]` or
JSON `kernel.mesh`). Transports: `tcp` (default), `ws`, `quic` (WEFT-118).

```toml
[kernel.mesh]
enabled = true
transport = "quic"
listen_addr = "0.0.0.0:9489"
noise = true
seed_peers = ["quic://10.0.0.2:9489"]
```

A seed sends no admission hello, so its node id is only what it claims. Without
more, a dialled seed is bound to the first id it names (in an envelope or an
assessment frame; a frame that names none is dropped) and can claim no id another
connection routes. To pin the id instead, write the entry as
`address#node-id`, for example `"quic://10.0.0.2:9489#n-0123abcd"`: frames
naming any other id are then dropped from the first byte. The pin is your word
about the seed, not a proof. Under `admission = "enforce"` the seed's peer is
still held as `unverified` (not an active cluster member, and placement refuses
it); under `observe`, the default, and `off`, it counts as a member as before.
A pinned seed that keeps sending frames as other ids is disconnected after 3 and
redialled with backoff.

Admission and the machine mesh service (ADR-103 Phase 3):

| Key | Default | Meaning |
|-----|---------|---------|
| `admission` | `"observe"` | `off`, `observe` or `enforce`. `observe` checks peers' signed hellos and records would-be refusals but never refuses and never marks a peer verified; it takes effect only once `genesis_hash` is set. `enforce` refuses unsigned, plaintext, wrong-genesis, revoked and verdict-denied peers. |
| `genesis_hash` | unset | 64 hex chars peers must present. A cluster label, not a credential. Required for `enforce`. |
| `admission_open_membership` | `false` | Let `enforce` admit every peer with a valid hello when there is no governance gate (otherwise it refuses everyone). |
| `max_connections_per_ip` | `64` | Concurrent inbound mesh connections per source IP (IPv6 per /64). Applies under every `admission` mode, including `off`. The machine mesh service reads the same key from `mesh.toml`. |
| `first_frame_timeout_secs` | `10` | Seconds an inbound connection may stay silent before its first frame. Applies under every `admission` mode; a legacy leaf that connects and waits longer is dropped. The machine mesh service reads the same key from `mesh.toml`. |
| `leaf_listen_addr` | absent | Where the certified-leaf listeners bind, apart from `listen_addr`. An IP (`192.0.2.10`) puts the leaf TCP listener on `<ip>:<main port + 2>` and leaf discovery (UDP) on `<ip>:<main port + 1>`; `ip:port` (`192.0.2.10:9600`) sets the leaf TCP port and discovery uses that port minus one. Absent: the main listener's IP, as before. A list (`["192.0.2.10", "100.64.0.5"]`) binds one leaf listener and discovery socket per entry, all admitting through the same registry and keys; each entry follows these rules, and a duplicate or empty list fails boot. An address that cannot bind yet (for example a tailnet interface that is not up) is logged and retried every 15 s while the others keep serving. Boot fails on a value that is not an IP or `ip:port`. This changes where the sockets bind, not who is admitted: every leaf still needs its certificate and signed frames. |
| `leaf_listen_any` | `false` | Allows a `leaf_listen_addr` entry to be a wildcard (`0.0.0.0`, `::`). Without it a wildcard fails boot, so a LAN-only bind is the default. |
| `service` | `"auto"` | User daemon only. `auto`: use the machine mesh service when it answers and verifies, else run the mesh in this daemon (collapsed). `required`: boot fails without the service. `off`: never probe. A service that answers but fails verification always fails the boot. Under `auto`, a machine whose service key is pinned and that has no local `node.key` refuses to collapse (it would need a new node id). |
| `service_socket` | unset | Service socket override; else `$WEFTOS_MESH_SOCKET`, else `/var/run/weftos/mesh.sock`. |

```toml
[kernel.mesh]
enabled = true
service = "required"      # after moving to the machine mesh service
```

Full guide: [mesh-quic.md](./mesh-quic.md). Kernel overview: [kernel.md](./kernel.md#mesh-k6).

## Outside-project policy (`kernel.governance.outside_project`)

ADR-103 D12. A request is *outside any project* when it claims no project and
the daemon is not bound to one, or when the project it claims does not verify
(it differs from the daemon's binding, or has no active manifest in
`~/.weftos/projects/<id>.toml`). The claim itself is never trusted. The
daemon's own in-process voice consumer counts as inside.

```toml
[kernel.governance]
outside_project = "read_only"   # read_only | deny_all | allow_all
```

When unset the default depends on the daemon's root (ADR-103 D12; an
amendment is pending): the `--profile user` daemon defaults to `read_only`;
every other root (project-bound, legacy `~/.clawft`, env-isolated) defaults to
`allow_all`, preserving pre-ADR-103 behaviour until it migrates. An explicit
value always wins.

| Value | Outside a project |
|-------|-------------------|
| `read_only` | Only an explicit allow-list of read methods (`kernel.status`, `kernel.ps`, `kernel.services`, `kernel.logs`, `kernel.handshake`, `ping`, `cluster.status\|nodes\|health`, `chain.status\|verify`, `agent.list\|inspect`, `control.list`, `llm.models`, `mcp.list`, `auth.token.validate`, `project.list\|show`), plus the user-level operations `auth.token.issue\|revoke\|list` and `project.register` for callers holding `admin`. Everything else, including methods added later, fails with `error_kind = "project_required"`: "not in a project; run `weft project init` or pass `--project`". |
| `deny_all` | Only `kernel.status`, `kernel.handshake`, `project.list`, `project.show`. |
| `allow_all` | No restriction. |

Inside a project every method falls through to the usual capability check.

Limit: `project` is declared by the client, so verification proves the project
is registered, not that the caller belongs to it. In Phase 1 this guards
against working in the wrong place by accident; it is not an authorisation
boundary between local processes of one user.

Independently of this setting, the daemon's in-process voice principal can
never call the cron mutations (`cron.add`, `cron.remove`, `cron.enable`,
`cron.disable`): a spoken command must not be able to schedule recurring
unattended agent jobs (`error_kind = "voice_denied"`).

## Project kernels (`kernel.profile`, `kernel.shared_services`, `[serve]`)

ADR-103 Phase 2. A project kernel is started by the user daemon with
`--profile project`, which forces the profile; the config setting exists so
the same value can be read back and checked.

```toml
[kernel]
profile = "project"           # only value; absent for every other daemon

[kernel.shared_services]      # read only when profile = "project"
embeddings = "parent"         # "parent" (the default) is the only mode
llm = "parent"
voice = "parent"
```

`parent` means the service is called on the user daemon and fails closed
(`parent_unavailable`) when it is down; a project kernel never falls back to
a local model or a provider key. A project kernel also ignores
`[kernel.mesh]` (it opens no mesh listener) and runs no voice pipeline.

The per-project manifest `~/.weftos/projects/<id>.toml` has a `[serve]`
table the resolver and the supervisor read:

| Key | Meaning |
|-----|---------|
| `via` | `"user-daemon"` (served by the user daemon) or `"child-kernel"` (a supervised project kernel; set by `weaver project migrate-kernel`) |
| `runtime_dir` | Explicit runtime root; wins over `via` |
| `idle_stop_secs` | Stop an idle child after this many seconds; default 1800 for `child-kernel`, `0` never |
| `restart_max` | Crash restarts allowed inside `restart_window_secs` before the child is `failed`; default 5 |
| `restart_window_secs` | Default 60 |
| `kernel_version`, `kernel_sha` | Written by the supervisor: the build it last started. Do not edit |

The project's governance overlay, `<root>/.weftos/overlay.toml`, is described
in the kernel guide ([Project kernels](kernel.md#project-kernels-phase-2)): it
may add denies and approvals and lower limits, never loosen the parent's
rules.
