# clawft-kernel

The WeftOS kernel crate: boot sequence, process table, typed IPC, service
registry, governance, the ExoChain audit chain, and the mesh, placement,
exposure and cogbus services that the node daemon runs. Use it to build on the
kernel directly; the operator CLI is `weaver`.

Layers, from the bottom: boot (K0), process (K1), IPC (K2), services (K3-K5),
then mesh and placement. ExoChain is cross-cutting. The layer model, module
groups and the feature matrix are in
[Kernel Architecture](https://weftos.weavelogic.ai/docs/weftos/architecture).

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
clawft-kernel = "0.8.4"
```

## Features

- `test-support`
- `native` (default)
- `cluster` (default)
- `exochain` (default)
- `tilezero` (default)
- `mesh` (default)
- `quic` (default)
- `mesh-rvf`
- `ecc` (default)
- `quantum-pasqal`
- `quantum-braket`
- `diskann`
- `rabitq-bench`
- `onnx-embeddings`
- `wasm-sandbox`
- `os-patterns`
- `treesitter`
- `sensor`
- `containers`
- `workload-runtime` (default)
- `http-api`

## Documentation

- API docs: https://docs.rs/clawft-kernel
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
