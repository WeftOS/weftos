# clawft-service-agent

WeftOS agent service — wraps clawft-core::agent::AgentLoop with per-conversation locking, cancellation, and a single dispatch entry point shaped for the agent.chat JSON-RPC handler.

WeftOS agent service — daemon-side wrapper around `clawft_core::agent::AgentLoop` that owns per-conversation locking, cancellation tokens, and a single `dispatch` entry point shaped for the `agent.chat` JSON-RPC handler.

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
clawft-service-agent = "0.8.4"
```

## Documentation

- API docs: https://docs.rs/clawft-service-agent
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
