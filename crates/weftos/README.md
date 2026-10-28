# weftos

WeftOS as a library: the kernel (process table, typed IPC, ExoChain audit chain,
governance, mesh) behind one crate with feature flags. Use it to embed the kernel
in your own project, or to get the `weftos` kernel daemon with `cargo install weftos`.
The `weft` and `weaver` CLIs are not published on crates.io; see the
[install guide](https://weftos.weavelogic.ai/docs/weftos/getting-started/installation).

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
weftos = "0.8.4"
```

## Where it fits

The agent framework (`clawft-*`) runs on the kernel. The diagram and the status
of each part are in the [repository README](https://github.com/WeftOS/weftos#how-the-pieces-fit).

## Features

- `native` (default)
- `exochain`
- `cluster`
- `mesh`
- `ecc`
- `wasm-sandbox`
- `containers`
- `os-patterns`
- `full`

## Documentation

- API docs: https://docs.rs/weftos
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
