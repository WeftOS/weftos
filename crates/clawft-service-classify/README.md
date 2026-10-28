# clawft-service-classify

WeftOS audio-classification Stage — sits between mic PCM and STT inference. Initial impl is energy-based VAD; the ClassifierBackend trait keeps the emission shape generic so a llama.cpp-hosted classifier (music / speech / noise / ...) can swap in without changing any subscriber's wire-format code.

WeftOS audio-classification service.

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
clawft-service-classify = "0.8.4"
```

## Documentation

- API docs: https://docs.rs/clawft-service-classify
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
