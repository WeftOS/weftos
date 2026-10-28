# clawft-core

The core engine of the clawft agent framework: the agent loop, the message bus,
context management and memory. Providers, channels and tools plug into it, and
the `weft` CLI drives it. Use it to embed an agent in your own program.

See [agent loop](https://weftos.weavelogic.ai/docs/clawft/agent-loop) and
[architecture](https://weftos.weavelogic.ai/docs/clawft/architecture).

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
clawft-core = "0.8.4"
```

## Features

- `full` (default)
- `native` (default)
- `real-tokenizer`
- `browser`
- `vector-memory`
- `rvf`
- `signing`
- `embedding-router`
- `hybrid-rerank`

## Documentation

- API docs: https://docs.rs/clawft-core
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
