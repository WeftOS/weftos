# Quickstart

This guide walks you through installing clawft from source, creating a
configuration file, and running your first agent session.

## Prerequisites

- **Rust 1.93+** (edition 2024). Install via [rustup](https://rustup.rs/):

  ```bash
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
  rustup update
  ```

  Verify with `rustc --version` -- you need at least `1.93.0`.

- **Cargo** (ships with Rust).

## Installation

Clone the repository and build in release mode:

```bash
git clone https://github.com/WeftOS/weftos.git
cd clawft
scripts/build.sh native    # Or: cargo build --release
```

The binary is compiled to `target/release/weft`. You can copy it onto your
`PATH` or run it directly:

```bash
# Option A: copy to a directory on your PATH
cp target/release/weft ~/.local/bin/

# Option B: run from the repo
./target/release/weft --version
```

## Configuration

clawft looks for its configuration file using a three-step discovery chain:

1. The path in the `CLAWFT_CONFIG` environment variable (if set).
2. `~/.clawft/config.json`
3. Falls back to built-in defaults if no file is found.

Create a minimal config:

```bash
mkdir -p ~/.clawft
```

Write the following to `~/.clawft/config.json`:

```json
{
  "agents": {
    "defaults": {
      "model": "openai/gpt-4o",
      "workspace": "~/.clawft/workspace",
      "max_tool_iterations": 10
    }
  },
  "gateway": {
    "heartbeat_interval_minutes": 0
  },
  "channels": {},
  "tools": {}
}
```

The `model` field uses the format `provider/model-name`. See the next section
for available providers.

## Choosing a Provider

clawft ships with seven built-in LLM providers. Every model is referenced by
a compound identifier in the form `provider/model-name` -- for example
`openai/gpt-4o` or `anthropic/claude-sonnet-4-5-20250514`.

| Provider     | Env Variable            | Example Model                                |
|--------------|-------------------------|----------------------------------------------|
| OpenAI       | `OPENAI_API_KEY`        | `openai/gpt-4o`                              |
| Anthropic    | `ANTHROPIC_API_KEY`     | `anthropic/claude-sonnet-4-5-20250514`       |
| Groq         | `GROQ_API_KEY`          | `groq/llama-3.3-70b-versatile`               |
| DeepSeek     | `DEEPSEEK_API_KEY`      | `deepseek/deepseek-chat`                     |
| Mistral      | `MISTRAL_API_KEY`       | `mistral/mistral-large-latest`               |
| Together     | `TOGETHER_API_KEY`      | `together/meta-llama/Llama-3.3-70B-Instruct` |
| OpenRouter   | `OPENROUTER_API_KEY`    | `openrouter/google/gemini-2.5-pro-preview`   |

To change the default model, update the `model` field in your config file:

```json
"agents": {
  "defaults": {
    "model": "anthropic/claude-sonnet-4-5-20250514"
  }
}
```

You can also override the model per-request with the `--model` flag (shown in
[Your First Message](#your-first-message) below).

For provider-specific options, rate-limit configuration, and advanced routing,
see the [Providers Guide](../guides/providers.md).

## Set Your API Key

clawft reads API keys from environment variables at request time. Export the
key that corresponds to the provider you configured:

```bash
# OpenAI
export OPENAI_API_KEY="sk-..."

# Anthropic
export ANTHROPIC_API_KEY="sk-ant-..."

# Groq
export GROQ_API_KEY="gsk_..."
```

Only the key for your active provider is required. Add the export to your
shell profile (`~/.bashrc`, `~/.zshrc`, etc.) to persist it across sessions.

## Using Local LLMs

If you prefer to run models locally, clawft can connect to any
OpenAI-compatible server. For example, with [Ollama](https://ollama.com):

```json
{
  "providers": {
    "ollama": {
      "type": "openai",
      "base_url": "http://localhost:11434/v1",
      "api_key": "ollama"
    }
  },
  "agents": {
    "defaults": {
      "model": "ollama/llama3"
    }
  }
}
```

No API key environment variable is needed for local providers. See the
[Providers Guide](../guides/providers.md) for detailed local-LLM setup.

## Your First Message

Send a single message using the `-m` flag. The agent processes the prompt,
prints the response, and exits:

```bash
weft agent -m "What is Rust?"
```

You can override the configured model for a one-off request:

```bash
weft agent --model anthropic/claude-sonnet-4-20250514 -m "Explain ownership in Rust."
```

## Interactive Mode

Start an interactive REPL session by running `weft agent` with no message flag:

```bash
weft agent
```

Type your messages at the prompt. The following slash commands are available
inside the session:

| Command  | Description                  |
|----------|------------------------------|
| `/help`  | Show available commands      |
| `/tools` | List registered tools        |
| `/clear` | Clear the current session    |
| `/exit`  | Quit the interactive session |

Example session:

```
$ weft agent
> What are Rust lifetimes?
Lifetimes are a way for the Rust compiler to track how long references
are valid...

> /tools
Registered tools:
  - file_read
  - file_write
  - shell_exec
  ...

> /exit
```

## Setting Up a Gateway

Gateway mode connects messaging channels (Telegram, Slack, Discord) to the
agent loop. Inbound messages from channels are processed by the agent, and
responses are dispatched back to the originating channel.

Start the gateway:

```bash
weft gateway
```

With no channels enabled, the gateway will start but won't receive any
messages. The next section shows how to add one.

You can point at an alternate config file:

```bash
weft gateway --config /path/to/config.json
```

## Adding a Channel: Telegram

1. Create a bot with [BotFather](https://t.me/BotFather) on Telegram and copy
   the bot token.

2. Update `~/.clawft/config.json` to enable the Telegram channel:

   ```json
   {
     "agents": {
       "defaults": {
         "model": "openai/gpt-4o",
         "workspace": "~/.clawft/workspace",
         "max_tool_iterations": 10
       }
     },
     "channels": {
       "telegram": {
         "enabled": true,
         "token": "YOUR_BOT_TOKEN",
         "allow_from": []
       }
     },
     "tools": {}
   }
   ```

   Setting `allow_from` to an empty list allows messages from all users. To
   restrict access, add Telegram user IDs or usernames:

   ```json
   "allow_from": ["123456789", "my_username"]
   ```

3. Start the gateway:

   ```bash
   weft gateway
   ```

4. Open your bot in Telegram and send a message. The agent will respond in the
   chat.

## Checking Status

Use the `status` subcommand to verify that your configuration loaded correctly
and see which provider is active:

```bash
weft status
```

For more detail:

```bash
weft status --detailed
```

To inspect channel configuration specifically:

```bash
weft channels status
```

## Verbose Logging

Pass the `--verbose` flag (before the subcommand) to enable debug-level output.
This is useful for troubleshooting configuration or connectivity issues:

```bash
weft --verbose agent -m "test"
weft --verbose gateway
```

You can also control log output via the `RUST_LOG` environment variable:

```bash
RUST_LOG=debug weft gateway
```

## Next Steps

- [Configuration Reference](../reference/) -- full schema documentation for
  `config.json`, including all channel types, provider settings, MCP servers,
  and tool configuration.
- [Providers Guide](../guides/providers.md) -- detailed setup for all seven
  built-in providers, local LLMs, and custom endpoints.
- [Routing and Pipeline](../guides/routing.md) -- configure multi-model
  routing, fallback chains, and the agent processing pipeline.
- [Tool Calls](../guides/tool-calls.md) -- register tools, handle tool
  invocations, and integrate MCP tool servers.
- [RVF Integration](../guides/rvf.md) -- connect clawft to the Rustic
  Validation Framework for structured output validation.
- [Architecture Overview](../architecture/) -- how the agent pipeline, message
  bus, and channel system fit together.
- [Adding MCP Servers](../guides/tool-calls.md#mcp-tools) -- connect external
  tool servers to extend the agent's capabilities.
