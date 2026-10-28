# clawft-graphify

Knowledge graph builder for WeftOS: extracts entities and relationships from code (tree-sitter) and documents, clusters and exports them.

`clawft-graphify` -- Knowledge graph builder for WeftOS.

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
clawft-graphify = "0.8.4"
```

## Features

- `ast-extract`
- `semantic-extract`
- `vision-extract`
- `code-domain` (default)
- `forensic-domain`
- `html-export`
- `neo4j-export`
- `kernel-bridge`
- `http-client`
- `rdf-ingest` (default)
- `mcp`
- `sase-cluster`
- `full`
- `lang-python`
- `lang-javascript`
- `lang-typescript`
- `lang-rust`
- `lang-go`
- `lang-java`
- `lang-c`
- `lang-cpp`
- `lang-ruby`
- `lang-csharp`
- `lang-all`

## Documentation

- API docs: https://docs.rs/clawft-graphify
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
