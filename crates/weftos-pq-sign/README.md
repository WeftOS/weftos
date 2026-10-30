# weftos-pq-sign

Real ML-DSA-65 (FIPS 204) and versioned Ed25519 + ML-DSA-65 dual signatures for WeftOS, with an explicit legacy-placeholder policy (ADR-129).

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
weftos-pq-sign = "0.8.4"
```

ML-DSA-65 comes from RustCrypto's pure-Rust [`ml-dsa`](https://crates.io/crates/ml-dsa) crate, so every target, wasm32 included, gets the real algorithm. Private keys are stored as the 32-byte FIPS 204 seed (key file format v2).

## Documentation

- API docs: https://docs.rs/weftos-pq-sign
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
