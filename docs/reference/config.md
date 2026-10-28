# Configuration Reference

clawft is configured via a JSON configuration file. All fields support both
`snake_case` and `camelCase` names. Unknown fields are silently ignored for
forward compatibility. Every field has a default value and can be omitted.

Source: `crates/clawft-types/src/config.rs`

---

## Root Structure

```json
{
  "agents": { ... },
  "channels": { ... },
  "providers": { ... },
  "gateway": { ... },
  "tools": { ... },
  "delegation": { ... },
  "routing": { ... }
}
```

| Section      | Description                                          |
|--------------|------------------------------------------------------|
| `agents`     | Agent defaults (model, tokens, temperature)          |
| `channels`   | Chat channel configurations (Telegram, Slack, etc.)  |
| `providers`  | LLM provider credentials and endpoints               |
| `gateway`    | HTTP server settings                                 |
| `tools`      | Tool configurations (web search, exec, MCP, security)|
| `delegation` | Task delegation routing rules                        |
| `routing`    | Tiered model routing, permissions, budgets, rate limits |

---

## agents

Agent defaults and workspace root applied to all agents.

```json
{
  "agents": {
    "workspace_root": "/home/user/my-project",
    "defaults": {
      "workspace": "~/.nanobot/workspace",
      "model": "anthropic/claude-opus-4-5",
      "maxTokens": 8192,
      "temperature": 0.7,
      "maxToolIterations": 20,
      "memoryWindow": 50
    }
  }
}
```

| Field            | Type            | Default | Description |
|------------------|-----------------|---------|-------------|
| `workspace_root` | string path / null | `null` | Daemon identity + tool workspace root (WEFT-83). When unset, process CWD is used. Alias: `workspaceRoot`. See [guides/agents.md](../guides/agents.md). |
| `defaults`       | object          | (below) | Per-agent default settings. |
| `cost_budget`    | object          | (defaults) | Per-conversation cost circuit-breaker. |
| `cow_memory`     | object          | disabled | Per-turn COW memory checkpointing. |

### agents.defaults

| Field               | Type    | Default                       | Description                                   |
|---------------------|---------|-------------------------------|-----------------------------------------------|
| `workspace`         | string  | `"~/.nanobot/workspace"`      | Working directory for agent file operations. `~` is expanded to the home directory. Distinct from `agents.workspace_root`. |
| `model`             | string  | `"anthropic/claude-opus-4-5"` | Default LLM model in `provider/model` format. See [Routers](#routers). |
| `maxTokens`         | integer | `8192`                        | Maximum tokens in a single LLM response.      |
| `temperature`       | float   | `0.7`                         | Sampling temperature (0.0 = deterministic, 1.0 = creative). |
| `maxToolIterations` | integer | `20`                          | Maximum tool-use rounds per turn before stopping. |
| `memoryWindow`      | integer | `50`                          | Number of recent messages to include in context. |

---

## providers

LLM provider credentials. API keys should be set via environment variables, not
hardcoded in configuration files. The `apiKey` field in the config is used for
testing only; production deployments should rely on the environment variable
lookup performed by the `ProviderRouter`.

```json
{
  "providers": {
    "anthropic": {
      "apiKey": "",
      "apiBase": null,
      "extraHeaders": {}
    },
    "openai": { ... },
    "openrouter": { ... },
    "deepseek": { ... },
    "groq": { ... },
    "zhipu": { ... },
    "dashscope": { ... },
    "vllm": { ... },
    "gemini": { ... },
    "moonshot": { ... },
    "minimax": { ... },
    "aihubmix": { ... },
    "openai_codex": { ... },
    "custom": { ... }
  }
}
```

### Provider Fields

Each provider entry has the same shape:

| Field          | Type              | Default | Description                                         |
|----------------|-------------------|---------|-----------------------------------------------------|
| `apiKey`       | string            | `""`    | API key for authentication (prefer env vars instead).|
| `apiBase`      | string or null    | `null`  | Base URL override. Use for proxies or self-hosted endpoints. |
| `extraHeaders` | object or null    | `null`  | Custom HTTP headers sent with every request (e.g. `{"APP-Code": "xyz"}`). |

---

## Routers

clawft has two layers of routing that work together to direct requests to the
right LLM provider and model.

### Provider Router (clawft-llm)

The `ProviderRouter` performs **prefix-based routing**: it strips a known prefix
from the model string and dispatches to the matching provider. Seven providers
are built-in and require only an environment variable to activate.

Source: `crates/clawft-llm/src/config.rs`, `crates/clawft-llm/src/router.rs`

#### Built-in Providers

| # | Name         | Prefix        | Base URL                          | Env Variable          | Default Model                  |
|---|--------------|---------------|-----------------------------------|-----------------------|--------------------------------|
| 1 | **openai**   | `openai/`     | `https://api.openai.com/v1`       | `OPENAI_API_KEY`      | `gpt-4o`                       |
| 2 | **anthropic**| `anthropic/`  | `https://api.anthropic.com/v1`    | `ANTHROPIC_API_KEY`   | `claude-sonnet-4-5-20250514`   |
| 3 | **groq**     | `groq/`       | `https://api.groq.com/openai/v1`  | `GROQ_API_KEY`        | `llama-3.1-70b-versatile`      |
| 4 | **deepseek** | `deepseek/`   | `https://api.deepseek.com/v1`     | `DEEPSEEK_API_KEY`    | `deepseek-chat`                |
| 5 | **mistral**  | `mistral/`    | `https://api.mistral.ai/v1`       | `MISTRAL_API_KEY`     | `mistral-large-latest`         |
| 6 | **together** | `together/`   | `https://api.together.xyz/v1`     | `TOGETHER_API_KEY`    | *(none)*                       |
| 7 | **openrouter** | `openrouter/` | `https://openrouter.ai/api/v1`  | `OPENROUTER_API_KEY`  | *(none)*                       |
| 8 | **gemini**   | `gemini/`     | `https://generativelanguage.googleapis.com/v1beta/openai` | `GOOGLE_GEMINI_API_KEY` | `gemini-2.5-flash`     |

The first provider in the list (**openai**) is the default fallback. If a model
string has no recognized prefix (e.g. `"gpt-4o"` instead of `"openai/gpt-4o"`),
it is routed to the default provider with the model string unchanged.

Anthropic requests automatically include the `anthropic-version: 2023-06-01`
header.

#### Routing Examples

```
"openai/gpt-4o"                    -> provider: openai,     model: gpt-4o
"anthropic/claude-opus-4-5"        -> provider: anthropic,  model: claude-opus-4-5
"groq/llama-3.1-70b"              -> provider: groq,       model: llama-3.1-70b
"deepseek/deepseek-chat"          -> provider: deepseek,   model: deepseek-chat
"mistral/mistral-large-latest"    -> provider: mistral,    model: mistral-large-latest
"together/meta-llama/Llama-3-70b" -> provider: together,   model: meta-llama/Llama-3-70b
"openrouter/anthropic/claude-3"   -> provider: openrouter,  model: anthropic/claude-3
"gpt-4o"                          -> provider: openai,     model: gpt-4o  (default fallback)
"unknown/model-x"                 -> provider: openai,     model: unknown/model-x  (no match)
```

Prefixes are matched longest-first (greedy). For nested models like
`openrouter/anthropic/claude-3`, only the first segment is consumed as the
prefix.

#### Custom Providers

Custom providers can be configured by supplying a `ProviderConfig` with a
`model_prefix`, `base_url`, and `api_key_env`:

```json
{
  "providers": {
    "custom": {
      "apiKey": "",
      "apiBase": "https://my-llm-proxy.example.com/v1"
    }
  }
}
```

Or programmatically via `ProviderRouter::from_configs()` with a custom
`ProviderConfig` entry. A provider without a `model_prefix` can still be used as
the default fallback.

### Pipeline Model Router (clawft-core)

The pipeline's Stage 2 `ModelRouter` trait provides a higher-level routing
abstraction that considers task classification and complexity.

Source: `crates/clawft-core/src/pipeline/router.rs`,
`crates/clawft-core/src/pipeline/traits.rs`

#### StaticRouter (Level 0)

The current implementation. Always returns the same provider/model pair
configured in `agents.defaults.model`. Does not learn from outcomes.

```
Request  ->  StaticRouter  ->  RoutingDecision { provider, model, reason }
```

The `model` string from config is split on the first `/`:
- `"anthropic/claude-opus-4-5"` -> provider: `anthropic`, model: `claude-opus-4-5`
- `"gpt-4o"` (no slash) -> provider: `openai` (default), model: `gpt-4o`

#### ModelRouter Trait

Future router implementations (Level 1 adaptive, Level 2 neural) can implement
the `ModelRouter` trait to make routing decisions based on task profiles:

```rust
#[async_trait]
pub trait ModelRouter: Send + Sync {
    async fn route(&self, request: &ChatRequest, profile: &TaskProfile) -> RoutingDecision;
    fn update(&self, decision: &RoutingDecision, outcome: &ResponseOutcome);
}
```

The `update` method receives quality scores and latency data after each
response, enabling adaptive routing.

#### Pipeline Task Types

The task classifier (Stage 1) categorizes requests into these types, which
routers can use for model selection:

| TaskType         | Description                                   |
|------------------|-----------------------------------------------|
| `Chat`           | General conversation / chitchat               |
| `CodeGeneration` | Writing new code                              |
| `CodeReview`     | Reviewing existing code                       |
| `Research`       | Searching for information                     |
| `Creative`       | Creative writing (stories, poems, etc.)       |
| `Analysis`       | Analytical reasoning, summarization           |
| `ToolUse`        | Explicit tool invocation                      |
| `Unknown`        | Could not determine the task type             |

#### Retry Policy

All provider calls can be wrapped with automatic retry via `RetryPolicy`:

| Parameter          | Default    | Description                                        |
|--------------------|------------|----------------------------------------------------|
| `max_retries`      | `3`        | Maximum retry attempts.                            |
| `base_delay`       | `1s`       | Base delay between retries (exponential backoff).  |
| `max_delay`        | `30s`      | Maximum delay cap.                                 |
| `jitter_fraction`  | `0.25`     | Random jitter added to each delay (0.0--1.0).      |

Retryable errors: HTTP 429 (rate limited), 500, 502, 503, 504, timeouts, and
network errors. Non-retryable: auth failures, model-not-found, invalid
responses.

---

## channels

Chat channel configurations. Each channel has an `enabled` flag that defaults to
`false`. Only enabled channels are started at runtime.

### channels.telegram

```json
{
  "channels": {
    "telegram": {
      "enabled": true,
      "token": "",
      "allowFrom": ["user1", "user2"],
      "proxy": null
    }
  }
}
```

| Field       | Type         | Default | Description                                    |
|-------------|--------------|---------|------------------------------------------------|
| `enabled`   | boolean      | `false` | Whether Telegram is active.                    |
| `token`     | string       | `""`    | Bot token from `@BotFather`.                   |
| `tokenEnv`  | string/null  | `null`  | Env var holding the bot token. Used when `token` is empty. |
| `allowFrom` | string array | `[]`    | Allowed user IDs/usernames. Empty = allow all. |
| `proxy`     | string/null  | `null`  | HTTP/SOCKS5 proxy URL.                         |

### channels.slack

```json
{
  "channels": {
    "slack": {
      "enabled": false,
      "mode": "socket",
      "webhookPath": "/slack/events",
      "botToken": "",
      "appToken": "",
      "userTokenReadOnly": true,
      "groupPolicy": "mention",
      "groupAllowFrom": [],
      "dm": {
        "enabled": true,
        "policy": "open",
        "allowFrom": []
      }
    }
  }
}
```

| Field                | Type         | Default            | Description                                          |
|----------------------|--------------|--------------------|------------------------------------------------------|
| `enabled`            | boolean      | `false`            | Whether Slack is active.                             |
| `mode`               | string       | `"socket"`         | Connection mode (only `"socket"` is supported).      |
| `webhookPath`        | string       | `"/slack/events"`  | Webhook path for event subscriptions.                |
| `botToken`           | string       | `""`               | Bot token (`xoxb-...`).                              |
| `botTokenEnv`        | string/null  | `null`             | Env var holding the bot token. Used when `botToken` is empty. |
| `appToken`           | string       | `""`               | App-level token (`xapp-...`).                        |
| `appTokenEnv`        | string/null  | `null`             | Env var holding the app token. Used when `appToken` is empty. |
| `userTokenReadOnly`  | boolean      | `true`             | Whether the user token is read-only.                 |
| `groupPolicy`        | string       | `"mention"`        | Group message policy: `"mention"`, `"open"`, or `"allowlist"`. |
| `groupAllowFrom`     | string array | `[]`               | Allowed channel IDs when `groupPolicy` is `"allowlist"`. |
| `dm.enabled`         | boolean      | `true`             | Whether DMs are enabled.                             |
| `dm.policy`          | string       | `"open"`           | DM policy: `"open"` or `"allowlist"`.                |
| `dm.allowFrom`       | string array | `[]`               | Allowed Slack user IDs when DM policy is `"allowlist"`. |

### channels.discord

```json
{
  "channels": {
    "discord": {
      "enabled": false,
      "token": "",
      "allowFrom": [],
      "gatewayUrl": "wss://gateway.discord.gg/?v=10&encoding=json",
      "intents": 37377
    }
  }
}
```

| Field        | Type         | Default                                             | Description                                     |
|--------------|--------------|-----------------------------------------------------|-------------------------------------------------|
| `enabled`    | boolean      | `false`                                             | Whether Discord is active.                      |
| `token`      | string       | `""`                                                | Bot token from Discord Developer Portal.        |
| `tokenEnv`   | string/null  | `null`                                              | Env var holding the bot token. Used when `token` is empty. |
| `allowFrom`  | string array | `[]`                                                | Allowed user IDs. Empty = allow all.            |
| `gatewayUrl` | string       | `"wss://gateway.discord.gg/?v=10&encoding=json"`   | Gateway WebSocket URL.                          |
| `intents`    | integer      | `37377`                                             | Gateway intents bitmask (GUILDS + GUILD_MESSAGES + DIRECT_MESSAGES + MESSAGE_CONTENT). |

### channels.whatsapp

```json
{
  "channels": {
    "whatsapp": {
      "enabled": false,
      "bridgeUrl": "ws://localhost:3001",
      "bridgeToken": "",
      "allowFrom": []
    }
  }
}
```

| Field         | Type         | Default                  | Description                                      |
|---------------|--------------|--------------------------|--------------------------------------------------|
| `enabled`     | boolean      | `false`                  | Whether WhatsApp is active.                      |
| `bridgeUrl`   | string       | `"ws://localhost:3001"`  | WebSocket bridge URL.                            |
| `bridgeToken` | string       | `""`                     | Shared token for bridge authentication.          |
| `allowFrom`   | string array | `[]`                     | Allowed phone numbers. Empty = allow all.        |

### channels.feishu

```json
{
  "channels": {
    "feishu": {
      "enabled": false,
      "appId": "",
      "appSecret": "",
      "encryptKey": "",
      "verificationToken": "",
      "allowFrom": []
    }
  }
}
```

| Field               | Type         | Default | Description                                    |
|---------------------|--------------|---------|------------------------------------------------|
| `enabled`           | boolean      | `false` | Whether Feishu/Lark is active.                 |
| `appId`             | string       | `""`    | App ID from Feishu Open Platform.              |
| `appSecret`         | string       | `""`    | App Secret from Feishu Open Platform.          |
| `encryptKey`        | string       | `""`    | Encrypt key for event subscription.            |
| `verificationToken` | string       | `""`    | Verification token for event subscription.     |
| `allowFrom`         | string array | `[]`    | Allowed user open_ids. Empty = allow all.      |

### channels.dingtalk

```json
{
  "channels": {
    "dingtalk": {
      "enabled": false,
      "clientId": "",
      "clientSecret": "",
      "allowFrom": []
    }
  }
}
```

| Field          | Type         | Default | Description                                 |
|----------------|--------------|---------|---------------------------------------------|
| `enabled`      | boolean      | `false` | Whether DingTalk is active.                 |
| `clientId`     | string       | `""`    | AppKey.                                     |
| `clientSecret` | string       | `""`    | AppSecret.                                  |
| `allowFrom`    | string array | `[]`    | Allowed staff IDs. Empty = allow all.       |

### channels.mochat

```json
{
  "channels": {
    "mochat": {
      "enabled": false,
      "baseUrl": "https://mochat.io",
      "socketUrl": "",
      "socketPath": "/socket.io",
      "socketDisableMsgpack": false,
      "socketReconnectDelayMs": 1000,
      "socketMaxReconnectDelayMs": 10000,
      "socketConnectTimeoutMs": 10000,
      "refreshIntervalMs": 30000,
      "watchTimeoutMs": 25000,
      "watchLimit": 100,
      "retryDelayMs": 500,
      "maxRetryAttempts": 0,
      "clawToken": "",
      "agentUserId": "",
      "sessions": [],
      "panels": [],
      "allowFrom": [],
      "mention": { "requireInGroups": false },
      "groups": {},
      "replyDelayMode": "non-mention",
      "replyDelayMs": 120000
    }
  }
}
```

| Field                       | Type         | Default            | Description                                        |
|-----------------------------|--------------|--------------------|----------------------------------------------------|
| `enabled`                   | boolean      | `false`            | Whether Mochat is active.                          |
| `baseUrl`                   | string       | `"https://mochat.io"` | Mochat API base URL.                            |
| `socketUrl`                 | string       | `""`               | WebSocket URL for real-time messaging.             |
| `socketPath`                | string       | `"/socket.io"`     | Socket.IO path.                                    |
| `socketDisableMsgpack`      | boolean      | `false`            | Disable msgpack encoding for Socket.IO.            |
| `socketReconnectDelayMs`    | integer      | `1000`             | Reconnect delay in milliseconds.                   |
| `socketMaxReconnectDelayMs` | integer      | `10000`            | Maximum reconnect delay in milliseconds.           |
| `socketConnectTimeoutMs`    | integer      | `10000`            | Connection timeout in milliseconds.                |
| `refreshIntervalMs`         | integer      | `30000`            | Room refresh interval in milliseconds.             |
| `watchTimeoutMs`            | integer      | `25000`            | Watch timeout in milliseconds.                     |
| `watchLimit`                | integer      | `100`              | Maximum messages per watch cycle.                  |
| `retryDelayMs`              | integer      | `500`              | Retry delay in milliseconds.                       |
| `maxRetryAttempts`          | integer      | `0`                | Maximum retry attempts (0 = unlimited).            |
| `clawToken`                 | string       | `""`               | Authentication token.                              |
| `agentUserId`               | string       | `""`               | Agent user ID for the bot.                         |
| `sessions`                  | string array | `[]`               | Session IDs to monitor.                            |
| `panels`                    | string array | `[]`               | Panel IDs to monitor.                              |
| `allowFrom`                 | string array | `[]`               | Allowed user IDs. Empty = allow all.               |
| `mention.requireInGroups`   | boolean      | `false`            | Whether mentions are required in group messages.   |
| `groups`                    | object       | `{}`               | Per-group rules keyed by group ID. Each value has `requireMention` (boolean). |
| `replyDelayMode`            | string       | `"non-mention"`    | Reply delay mode: `"off"` or `"non-mention"`.      |
| `replyDelayMs`              | integer      | `120000`           | Reply delay in milliseconds.                       |

### channels.email

```json
{
  "channels": {
    "email": {
      "enabled": false,
      "consentGranted": false,
      "imapHost": "",
      "imapPort": 993,
      "imapUsername": "",
      "imapPassword": "",
      "imapMailbox": "INBOX",
      "imapUseSsl": true,
      "smtpHost": "",
      "smtpPort": 587,
      "smtpUsername": "",
      "smtpPassword": "",
      "smtpUseTls": true,
      "smtpUseSsl": false,
      "fromAddress": "",
      "autoReplyEnabled": true,
      "pollIntervalSeconds": 30,
      "markSeen": true,
      "maxBodyChars": 12000,
      "subjectPrefix": "Re: ",
      "allowFrom": []
    }
  }
}
```

| Field                | Type         | Default    | Description                                          |
|----------------------|--------------|------------|------------------------------------------------------|
| `enabled`            | boolean      | `false`    | Whether email channel is active.                     |
| `consentGranted`     | boolean      | `false`    | Explicit owner permission to access mailbox data.    |
| `imapHost`           | string       | `""`       | IMAP server hostname.                                |
| `imapPort`           | integer      | `993`      | IMAP server port.                                    |
| `imapUsername`       | string       | `""`       | IMAP username.                                       |
| `imapPassword`       | string       | `""`       | IMAP password.                                       |
| `imapMailbox`        | string       | `"INBOX"`  | IMAP mailbox to watch.                               |
| `imapUseSsl`         | boolean      | `true`     | Use SSL for IMAP.                                    |
| `smtpHost`           | string       | `""`       | SMTP server hostname.                                |
| `smtpPort`           | integer      | `587`      | SMTP server port.                                    |
| `smtpUsername`       | string       | `""`       | SMTP username.                                       |
| `smtpPassword`       | string       | `""`       | SMTP password.                                       |
| `smtpUseTls`         | boolean      | `true`     | Use STARTTLS for SMTP.                               |
| `smtpUseSsl`         | boolean      | `false`    | Use implicit SSL for SMTP.                           |
| `fromAddress`        | string       | `""`       | From address for outgoing emails.                    |
| `autoReplyEnabled`   | boolean      | `true`     | If false, inbound email is read but no reply is sent.|
| `pollIntervalSeconds`| integer      | `30`       | Polling interval in seconds.                         |
| `markSeen`           | boolean      | `true`     | Mark messages as seen after processing.              |
| `maxBodyChars`       | integer      | `12000`    | Maximum email body characters to process.            |
| `subjectPrefix`      | string       | `"Re: "`   | Prefix for reply subjects.                           |
| `allowFrom`          | string array | `[]`       | Allowed sender emails. Empty = allow all.            |

### channels.qq

```json
{
  "channels": {
    "qq": {
      "enabled": false,
      "appId": "",
      "secret": "",
      "allowFrom": []
    }
  }
}
```

| Field       | Type         | Default | Description                                 |
|-------------|--------------|---------|---------------------------------------------|
| `enabled`   | boolean      | `false` | Whether QQ bot is active.                   |
| `appId`     | string       | `""`    | Bot AppID.                                  |
| `secret`    | string       | `""`    | Bot AppSecret.                              |
| `allowFrom` | string array | `[]`    | Allowed user openids. Empty = allow all.    |

### Custom Channel Plugins

Unknown channel keys in the `channels` object are captured in an `extra` map for
forward compatibility. This allows third-party channel plugins to store their
configuration alongside built-in channels:

```json
{
  "channels": {
    "my_custom_channel": {
      "url": "wss://custom.io",
      "token": "abc"
    }
  }
}
```

---

## gateway

HTTP server settings for the REST API gateway.

```json
{
  "gateway": {
    "host": "0.0.0.0",
    "port": 18790,
    "heartbeatIntervalMinutes": 0,
    "heartbeatPrompt": "heartbeat"
  }
}
```

| Field                      | Type    | Default       | Description                                          |
|----------------------------|---------|---------------|------------------------------------------------------|
| `host`                     | string  | `"0.0.0.0"`   | Bind address.                                        |
| `port`                     | integer | `18790`        | Listen port.                                         |
| `heartbeatIntervalMinutes` | integer | `0`            | Heartbeat interval in minutes (0 = disabled).        |
| `heartbeatPrompt`          | string  | `"heartbeat"`  | Text sent as the heartbeat prompt.                   |

---

## tools

Tool configurations covering web search, shell execution, MCP server
connections, and security policies.

```json
{
  "tools": {
    "web": {
      "search": {
        "apiKey": "",
        "maxResults": 5
      }
    },
    "exec": {
      "timeout": 60
    },
    "restrictToWorkspace": false,
    "mcpServers": {},
    "commandPolicy": { ... },
    "urlPolicy": { ... }
  }
}
```

### tools.web.search

| Field        | Type    | Default | Description                                   |
|--------------|---------|---------|-----------------------------------------------|
| `apiKey`     | string  | `""`    | Search API key (e.g. Brave Search).           |
| `maxResults` | integer | `5`     | Maximum number of search results per query.   |

### tools.exec

| Field     | Type    | Default | Description                         |
|-----------|---------|---------|-------------------------------------|
| `timeout` | integer | `60`    | Command timeout in seconds.         |

### tools.restrictToWorkspace

| Field                | Type    | Default | Description                                              |
|----------------------|---------|---------|----------------------------------------------------------|
| `restrictToWorkspace`| boolean | `false` | Restrict all tool access to the workspace directory.     |

### tools.mcpServers

MCP (Model Context Protocol) server connections. Each entry is keyed by a server
name and specifies either a stdio command or an HTTP URL.

```json
{
  "tools": {
    "mcpServers": {
      "my-tools": {
        "command": "npx",
        "args": ["-y", "my-mcp-server"],
        "env": { "API_KEY": "secret" }
      },
      "remote": {
        "url": "http://localhost:3000/mcp"
      }
    }
  }
}
```

| Field     | Type         | Default | Description                                        |
|-----------|--------------|---------|----------------------------------------------------|
| `command` | string       | `""`    | Command to run (stdio transport, e.g. `"npx"`).   |
| `args`    | string array | `[]`    | Command arguments (stdio transport).               |
| `env`     | object       | `{}`    | Extra environment variables (stdio transport).     |
| `url`     | string       | `""`    | Streamable HTTP endpoint URL (HTTP transport).     |

If `command` is set, stdio transport is used. If only `url` is set, HTTP
transport is used. If neither is set, the server entry is skipped.

MCP tools are registered with namespaced names: `{server_name}__{tool_name}`.

### tools.commandPolicy

Controls which commands the `exec_shell` and `spawn` tools can execute.
See the [Security Reference](security.md) for the full dangerous-pattern list.

```json
{
  "tools": {
    "commandPolicy": {
      "mode": "allowlist",
      "allowlist": ["echo", "ls", "cargo", "npm"],
      "denylist": []
    }
  }
}
```

| Field       | Type         | Default        | Description                                                |
|-------------|--------------|----------------|------------------------------------------------------------|
| `mode`      | string       | `"allowlist"`  | Policy mode: `"allowlist"` (recommended) or `"denylist"`.  |
| `allowlist` | string array | `[]`           | Permitted command basenames in allowlist mode. Empty = use built-in defaults. |
| `denylist`  | string array | `[]`           | Blocked command patterns in denylist mode. Empty = use built-in defaults. |

In **allowlist** mode (default), only explicitly permitted commands can run. In
**denylist** mode, any command not matching a blocked pattern is allowed.
Dangerous patterns (e.g. `rm -rf /`, `sudo`, `mkfs`) are always blocked
regardless of mode.

### tools.urlPolicy

Controls which URLs the `web_fetch` tool can access. Provides SSRF protection
by blocking private networks, loopback addresses, and cloud metadata endpoints.
See the [Security Reference](security.md#url-safety--ssrf-protection) for
details.

```json
{
  "tools": {
    "urlPolicy": {
      "enabled": true,
      "allowPrivate": false,
      "allowedDomains": [],
      "blockedDomains": []
    }
  }
}
```

| Field            | Type         | Default | Description                                                |
|------------------|--------------|---------|------------------------------------------------------------|
| `enabled`        | boolean      | `true`  | Whether URL safety validation is active.                   |
| `allowPrivate`   | boolean      | `false` | Allow requests to private/internal IP ranges.              |
| `allowedDomains` | string array | `[]`    | Domains that bypass all safety checks.                     |
| `blockedDomains` | string array | `[]`    | Domains that are always blocked.                           |

---

## delegation

Task delegation routing configuration. Controls how tasks are dispatched between
local execution and Claude AI. Multi-agent orchestration uses MCP servers +
skills (the subprocess `Flow` target was retired in WEFT-179).

Source: `crates/clawft-types/src/delegation.rs`

```json
{
  "delegation": {
    "claude_enabled": false,
    "claudeModel": "claude-sonnet-4-20250514",
    "maxTurns": 10,
    "maxTokens": 4096,
    "rules": [
      {
        "pattern": "(?i)deploy",
        "target": "claude"
      },
      {
        "pattern": "(?i)simple.*query",
        "target": "local"
      }
    ],
    "excludedTools": ["shell_exec"]
  }
}
```

| Field               | Type         | Default                        | Description                                        |
|---------------------|--------------|--------------------------------|----------------------------------------------------|
| `claude_enabled`    | boolean      | `false`                        | Whether Claude AI delegation is enabled.           |
| `claudeModel`       | string       | `"claude-sonnet-4-20250514"`   | Claude model identifier for delegated tasks.       |
| `maxTurns`          | integer      | `10`                           | Maximum conversation turns per delegated task.     |
| `maxTokens`         | integer      | `4096`                         | Maximum tokens per Claude response.                |
| `rules`             | array        | `[]`                           | Ordered routing rules. First match wins.           |
| `excludedTools`     | string array | `[]`                           | Tool names that should never be delegated.         |

> **Retired (WEFT-179):** `claudeFlowEnabled` / `claude_flow_enabled` is ignored
> if present — remove it. Rule `"target": "flow"` still deserializes as
> `"claude"`; prefer writing `"claude"`.

### Delegation Rules

Each rule maps a regex pattern to a target:

| Field     | Type   | Description                                      |
|-----------|--------|--------------------------------------------------|
| `pattern` | string | Regex pattern matched against the task description. |
| `target`  | string | Where to route: `"local"`, `"claude"`, or `"auto"` (PascalCase aliases accepted). Retired `"flow"` maps to `"claude"`. |

### Delegation Targets

| Target   | Description                                              |
|----------|----------------------------------------------------------|
| `Local`  | Execute locally via the built-in tool pipeline.          |
| `Claude` | Delegate to Claude AI.                                   |
| `Flow`   | Delegate to Claude Flow orchestration.                   |
| `Auto`   | Automatically decide based on complexity heuristics (default). |

When no rule matches a task, the `Auto` target is used.

---

## routing

Tiered model routing with permission-based access control, cost budgets, and
rate limiting. This section is opt-in: existing configs without a `routing`
section continue to use the `StaticRouter` unchanged.

Source: `crates/clawft-types/src/routing.rs`, `crates/clawft-core/src/pipeline/tiered_router.rs`

**Default behavior**: Workspaces are **private by default**. Unidentified users
receive `zero_trust` permissions (free-tier models only, no tools, aggressive
rate limits). Admin users see and access everything. This ensures that adding
tiered routing never accidentally exposes expensive models or dangerous tools to
untrusted users.

```json
{
  "routing": {
    "mode": "tiered",
    "tiers": [ ... ],
    "selectionStrategy": "preference_order",
    "fallbackModel": "groq/llama-3.1-8b",
    "permissions": { ... },
    "escalation": { ... },
    "costBudgets": { ... },
    "rateLimiting": { ... }
  }
}
```

### routing.mode

| Field  | Type   | Default    | Description                                           |
|--------|--------|------------|-------------------------------------------------------|
| `mode` | string | `"static"` | Routing mode: `"static"` (Level 0) or `"tiered"` (Level 1). |

When `mode` is `"static"`, the `StaticRouter` is used (always routes to
`agents.defaults.model`). When `"tiered"`, the `TieredRouter` selects models
based on task complexity, user permissions, and cost budgets.

### routing.max_grantable_level

| Field                 | Type    | Default | Description |
|-----------------------|---------|---------|-------------|
| `max_grantable_level` | integer | `1`     | Maximum permission level a workspace overlay may grant (`0` = zero-trust, `1` = user, `2` = admin). Alias: `maxGrantableLevel`. |

Used by workspace ceiling validation (`validate_workspace_ceiling` /
WEFT-47). Missing field keeps the historical default of `1` so existing
configs remain safe-by-default. Raise to `2` only when operators intend
workspace configs to assign admin-level grants.

### routing.tiers

Model tiers define named groups of models with associated complexity ranges and
cost information. Tiers are ordered cheapest to most expensive.

```json
{
  "routing": {
    "tiers": [
      {
        "name": "free",
        "models": ["openrouter/meta-llama/llama-3.1-8b-instruct:free", "groq/llama-3.1-8b"],
        "complexity_range": [0.0, 0.3],
        "cost_per_1k_tokens": 0.0,
        "max_context_tokens": 8192
      },
      {
        "name": "standard",
        "models": ["anthropic/claude-haiku-3.5", "openai/gpt-4o-mini", "groq/llama-3.3-70b"],
        "complexity_range": [0.0, 0.7],
        "cost_per_1k_tokens": 0.001,
        "max_context_tokens": 16384
      },
      {
        "name": "premium",
        "models": ["anthropic/claude-sonnet-4-20250514", "openai/gpt-4o"],
        "complexity_range": [0.3, 1.0],
        "cost_per_1k_tokens": 0.01,
        "max_context_tokens": 200000
      },
      {
        "name": "elite",
        "models": ["anthropic/claude-opus-4-5", "openai/o1"],
        "complexity_range": [0.7, 1.0],
        "cost_per_1k_tokens": 0.05,
        "max_context_tokens": 200000
      }
    ]
  }
}
```

| Field                | Type         | Default  | Description                                                   |
|----------------------|--------------|----------|---------------------------------------------------------------|
| `name`               | string       | required | Tier name (e.g. `"free"`, `"standard"`, `"premium"`, `"elite"`). |
| `models`             | string array | required | Models in this tier, in preference order. Format: `"provider/model"`. |
| `complexity_range`   | [float, float] | required | Complexity range [min, max] this tier is suitable for (0.0--1.0). |
| `cost_per_1k_tokens` | float        | required | Approximate cost per 1K tokens (blended input/output, USD). Used for budget estimation. |
| `max_context_tokens` | integer      | `200000` | Maximum context tokens supported by models in this tier.      |

Complexity ranges may overlap intentionally. A request with complexity 0.5 could
route to `standard` or `premium` depending on user permissions and budget. The
router picks the highest-quality tier the user is allowed and can afford.

#### Default Tier Summary

| Tier       | Complexity | Cost/1K  | Use Cases                                         |
|------------|-----------|----------|---------------------------------------------------|
| `free`     | 0.0--0.3  | $0.000   | Chitchat, simple Q&A, greetings                   |
| `standard` | 0.0--0.7  | $0.001   | Summaries, basic code, research                   |
| `premium`  | 0.3--1.0  | $0.010   | Complex code, analysis, multi-step reasoning      |
| `elite`    | 0.7--1.0  | $0.050   | Architecture design, novel algorithms, long-form   |

### routing.selectionStrategy

| Field               | Type   | Default              | Description                                         |
|---------------------|--------|----------------------|-----------------------------------------------------|
| `selectionStrategy` | string | `"preference_order"` | How to pick a model within a tier when multiple are available. |

| Strategy           | Description                                              |
|--------------------|----------------------------------------------------------|
| `preference_order` | Use the first available model (order from config).       |
| `round_robin`      | Rotate through models in the tier across requests.       |
| `lowest_cost`      | Pick the cheapest model in the tier.                     |
| `random`           | Random selection (useful for load distribution).         |

### routing.fallbackModel

| Field           | Type        | Default | Description                                                 |
|-----------------|-------------|---------|-------------------------------------------------------------|
| `fallbackModel` | string/null | `null`  | Model to use when all tiers are exhausted or budget-blocked. |

### routing.permissions

Permission configuration controls what each user can access. **Workspaces are
private by default**: unidentified users get `zero_trust` (Level 0) with
access only to free-tier models, no tools, and strict limits. Admin users
(Level 2) have full access to all models, tools, and features.

Permissions are resolved by layering (lowest to highest priority):

```
1. Built-in defaults for the resolved level
2. Global config:    routing.permissions.<level_name>
3. Workspace config: routing.permissions.<level_name> (project override)
4. Per-channel:      routing.permissions.channels.<channel_name>
5. Per-user:         routing.permissions.users.<user_id>
```

#### Permission Levels

| Level | Name          | Audience                            | Default Tier Access              |
|-------|---------------|-------------------------------------|----------------------------------|
| 0     | `zero_trust`  | Anonymous, unknown, untrusted users | `free` only                      |
| 1     | `user`        | Authenticated users (in `allowFrom`) | `free`, `standard`              |
| 2     | `admin`       | Operators, owners, CLI local user   | `free`, `standard`, `premium`, `elite` |

#### Level Defaults

```json
{
  "routing": {
    "permissions": {
      "zero_trust": {
        "level": 0,
        "max_tier": "free",
        "tool_access": [],
        "max_context_tokens": 4096,
        "max_output_tokens": 1024,
        "rate_limit": 10,
        "streaming_allowed": false,
        "escalation_allowed": false,
        "escalation_threshold": 1.0,
        "model_override": false,
        "cost_budget_daily_usd": 0.10,
        "cost_budget_monthly_usd": 2.00
      },
      "user": {
        "level": 1,
        "max_tier": "standard",
        "tool_access": [
          "read_file", "write_file", "edit_file", "list_dir",
          "web_search", "web_fetch", "message"
        ],
        "max_context_tokens": 16384,
        "max_output_tokens": 4096,
        "rate_limit": 60,
        "streaming_allowed": true,
        "escalation_allowed": true,
        "escalation_threshold": 0.6,
        "model_override": false,
        "cost_budget_daily_usd": 5.00,
        "cost_budget_monthly_usd": 100.00
      },
      "admin": {
        "level": 2,
        "max_tier": "elite",
        "tool_access": ["*"],
        "max_context_tokens": 200000,
        "max_output_tokens": 16384,
        "rate_limit": 0,
        "streaming_allowed": true,
        "escalation_allowed": true,
        "escalation_threshold": 0.0,
        "model_override": true,
        "cost_budget_daily_usd": 0.0,
        "cost_budget_monthly_usd": 0.0
      }
    }
  }
}
```

#### Permission Dimensions

Each dimension is independently configurable. The level provides sensible
defaults; explicit values override.

| Dimension                 | Type         | Description                                                     |
|---------------------------|--------------|-----------------------------------------------------------------|
| `level`                   | integer      | Permission level (0, 1, or 2).                                  |
| `max_tier`                | string       | Maximum model tier this user can access.                        |
| `model_access`            | string array | Explicit model allowlist. Supports globs: `"anthropic/*"`. Empty = all in allowed tiers. |
| `model_denylist`          | string array | Explicit model denylist. Checked after allowlist.               |
| `tool_access`             | string array | Tool names this user can invoke. `["*"]` = all tools.          |
| `tool_denylist`           | string array | Tool names explicitly denied even if `tool_access` allows.     |
| `max_context_tokens`      | integer      | Maximum input context tokens.                                   |
| `max_output_tokens`       | integer      | Maximum output tokens per response.                             |
| `rate_limit`              | integer      | Requests per minute. 0 = unlimited.                             |
| `streaming_allowed`       | boolean      | Whether SSE streaming responses are allowed.                    |
| `escalation_allowed`      | boolean      | Whether complexity-based escalation to a higher tier is allowed. |
| `escalation_threshold`    | float        | Complexity threshold (0.0--1.0) above which escalation triggers. |
| `model_override`          | boolean      | Whether the user can manually override model selection.         |
| `cost_budget_daily_usd`   | float        | Daily cost budget in USD. 0.0 = unlimited.                      |
| `cost_budget_monthly_usd` | float        | Monthly cost budget in USD. 0.0 = unlimited.                    |
| `custom_permissions`      | object       | Extensible custom permission dimensions for plugins.            |

#### Per-User Overrides

```json
{
  "routing": {
    "permissions": {
      "users": {
        "alice_telegram_123": {
          "level": 2
        },
        "bob_discord_456": {
          "level": 1,
          "cost_budget_daily_usd": 2.00,
          "tool_access": ["read_file", "list_dir", "web_search"]
        }
      }
    }
  }
}
```

User IDs are platform-specific identifiers (Telegram user ID, Slack user ID,
Discord user ID). Per-user overrides take the highest precedence.

#### Per-Channel Overrides

```json
{
  "routing": {
    "permissions": {
      "channels": {
        "cli": { "level": 2 },
        "telegram": { "level": 1 },
        "discord": { "level": 0 },
        "slack": { "level": 1 }
      }
    }
  }
}
```

The CLI channel defaults to `admin` (Level 2) since the local user owns the
machine. Remote channels default to `zero_trust` (Level 0) unless explicitly
configured. This enforces the **private-by-default** principle: untrusted
channels must be explicitly granted higher permissions.

#### Permission Resolution with allowFrom

The existing `allowFrom` fields on channel configs continue to function as
binary access control (first gate). Permissions are the second gate:

| `allowFrom` status            | Router behavior                              |
|-------------------------------|----------------------------------------------|
| Empty (allow all)             | All users get channel-default permission level |
| User is in `allowFrom`        | User gets `user` level (Level 1) minimum     |
| User NOT in non-empty list    | Rejected by channel plugin (never reaches router) |
| User in `permissions.users`   | User gets the explicitly configured level    |
| CLI channel                   | Always `admin` (Level 2) unless overridden   |

#### Workspace-Level Permission Overrides

Project-level `.clawft/config.json` can override routing permissions via deep
merge. This allows a workspace to restrict or expand access independently:

```json
{
  "routing": {
    "permissions": {
      "channels": {
        "cli": { "level": 2 },
        "telegram": { "level": 0 }
      }
    },
    "cost_budgets": {
      "global_daily_limit_usd": 10.0
    }
  }
}
```

### routing.escalation

Controls whether the router can promote a request to a higher tier than the
user's `max_tier` when task complexity exceeds the threshold.

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

| Field                  | Type    | Default | Description                                            |
|------------------------|---------|---------|--------------------------------------------------------|
| `enabled`              | boolean | `true`  | Whether complexity-based escalation is active.         |
| `threshold`            | float   | `0.6`   | Complexity score above which escalation triggers.      |
| `max_escalation_tiers` | integer | `1`     | Maximum number of tiers above `max_tier` to escalate.  |

Example: A Level 1 user with `max_tier = "standard"` and threshold 0.6 sends a
request classified at complexity 0.8. The `standard` tier covers [0.0, 0.7], so
it does not match. With escalation, the router promotes to `premium` for this
request only.

### routing.costBudgets

Global cost budget settings. Per-user budgets are set in the permission
dimensions (`cost_budget_daily_usd`, `cost_budget_monthly_usd`).

```json
{
  "routing": {
    "costBudgets": {
      "global_daily_limit_usd": 50.0,
      "global_monthly_limit_usd": 500.0,
      "tracking_persistence": true,
      "reset_hour_utc": 0
    }
  }
}
```

| Field                     | Type    | Default | Description                                          |
|---------------------------|---------|---------|------------------------------------------------------|
| `global_daily_limit_usd`  | float   | `0.0`   | Global daily spend cap in USD. 0.0 = unlimited.      |
| `global_monthly_limit_usd`| float   | `0.0`   | Global monthly spend cap in USD. 0.0 = unlimited.    |
| `tracking_persistence`    | boolean | `true`  | Whether to persist cost tracking to disk.            |
| `reset_hour_utc`          | integer | `0`     | Hour (UTC) at which daily budgets reset.             |

When a user's request would exceed their budget, the router falls back to a
cheaper tier. If no tier fits, the `fallbackModel` is used. If that is also
over budget, the request is rejected with a budget-exhausted reason.

### routing.rateLimiting

Per-user rate limiting using a sliding window.

```json
{
  "routing": {
    "rateLimiting": {
      "window_seconds": 60,
      "strategy": "sliding_window"
    }
  }
}
```

| Field            | Type    | Default            | Description                                   |
|------------------|---------|--------------------|-----------------------------------------------|
| `window_seconds` | integer | `60`               | Rate limit window size in seconds.            |
| `strategy`       | string  | `"sliding_window"` | Rate limiting strategy.                       |

Per-user rate limits are set in the permission dimensions (`rate_limit` =
requests per `window_seconds`). 0 = unlimited.

### routing -- Custom Permission Dimensions

The `custom_permissions` field on any permission level supports arbitrary
key-value data for plugins and future features:

```json
{
  "custom_permissions": {
    "max_file_size_bytes": 10485760,
    "allowed_mcp_servers": ["filesystem", "web-search"],
    "vision_enabled": true,
    "audio_enabled": false,
    "max_concurrent_subagents": 3
  }
}
```

Plugin code accesses custom permissions via the `custom_permissions` map. Unknown
keys are preserved and forwarded without validation.

---

## Complete Example

A production configuration with tiered routing and permissions:

```json
{
  "agents": {
    "defaults": {
      "model": "anthropic/claude-sonnet-4-5-20250514",
      "maxTokens": 4096,
      "temperature": 0.5
    }
  },
  "providers": {
    "anthropic": { "apiKey": "" },
    "openai": { "apiKey": "" },
    "openrouter": { "apiBase": "https://openrouter.ai/api/v1" },
    "groq": { "apiKey": "" }
  },
  "gateway": {
    "port": 8080
  },
  "channels": {
    "telegram": {
      "enabled": true,
      "token": "",
      "allowFrom": ["123456789"]
    }
  },
  "tools": {
    "commandPolicy": {
      "mode": "allowlist",
      "allowlist": ["cargo", "npm", "git", "ls", "echo"]
    },
    "urlPolicy": {
      "enabled": true,
      "blockedDomains": ["evil.example.com"]
    },
    "mcpServers": {
      "claude-flow": {
        "command": "npx",
        "args": ["--no-install", "@claude-flow/cli", "mcp", "start"]
      }
    }
  },
  "delegation": {
    "claude_enabled": true,
    "claudeModel": "claude-sonnet-4-20250514",
    "rules": [
      { "pattern": "(?i)complex|architect|design", "target": "Claude" },
      { "pattern": "(?i)deploy|release|ci", "target": "Flow" }
    ]
  },
  "routing": {
    "mode": "tiered",
    "tiers": [
      {
        "name": "free",
        "models": ["groq/llama-3.1-8b"],
        "complexity_range": [0.0, 0.3],
        "cost_per_1k_tokens": 0.0
      },
      {
        "name": "standard",
        "models": ["anthropic/claude-haiku-3.5", "openai/gpt-4o-mini"],
        "complexity_range": [0.0, 0.7],
        "cost_per_1k_tokens": 0.001
      },
      {
        "name": "premium",
        "models": ["anthropic/claude-sonnet-4-20250514", "openai/gpt-4o"],
        "complexity_range": [0.3, 1.0],
        "cost_per_1k_tokens": 0.01
      }
    ],
    "fallbackModel": "groq/llama-3.1-8b",
    "permissions": {
      "channels": {
        "cli": { "level": 2 },
        "telegram": { "level": 1 }
      },
      "users": {
        "123456789": { "level": 2 }
      }
    },
    "costBudgets": {
      "global_daily_limit_usd": 25.0
    }
  }
}
```

Set API keys via environment variables:

```bash
export ANTHROPIC_API_KEY="sk-ant-..."
export OPENROUTER_API_KEY="sk-or-..."
export OPENAI_API_KEY="sk-..."
export GROQ_API_KEY="gsk_..."
export GOOGLE_GEMINI_API_KEY="AIza..."

# Channel tokens (when using token_env / bot_token_env / app_token_env)
export DISCORD_BOT_TOKEN="MTIz..."
export SLACK_BOT_TOKEN="xoxb-..."
export SLACK_APP_TOKEN="xapp-..."
export TELEGRAM_BOT_TOKEN="123456789:ABCdef..."
```

---

## See Also

- [Configuration sections](config-sections.md) -- Generated reference for
  `[kernel]`, `[kernel.mesh]`, `[router]`, the project manifest, `cog.toml`
  and `[mesh_limits]`.
- [Daemon RPC reference](rpc.md) -- Generated list of daemon methods and the
  capability each needs.
- [CLI reference](cli.md) -- Generated `weaver` and `weft` command reference.
- [Providers Guide](../guides/providers.md) -- Detailed guide on provider
  routing, custom providers, and multi-provider strategies.
- [Tools Reference](tools.md) -- Built-in tool documentation and MCP
  integration.
- [Security Reference](security.md) -- Command policy, URL policy, and
  workspace containment details.
- [Tiered Router Plan](.planning/08-tiered-router.md) -- Full design document
  for the tiered routing and permission system.
