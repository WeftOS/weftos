# weftos-pq-sign

Real ML-DSA-65 (FIPS 204) and versioned Ed25519 + ML-DSA-65 dual signatures for WeftOS, with an explicit legacy-placeholder policy (ADR-129).

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
weftos-pq-sign = "0.8.4"
```

The default `mldsa` feature links the PQClean ML-DSA-65 backend. Without it the types compile but every sign or verify fails closed.

## Documentation

- API docs: https://docs.rs/weftos-pq-sign
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
