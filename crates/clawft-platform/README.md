# clawft-platform

Platform abstraction layer for clawft.

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.8.4.

## Use

```toml
[dependencies]
clawft-platform = "0.8.4"
```

```rust
use clawft_platform::{Platform, NativePlatform};
use clawft_platform::http::HttpClient;
use std::collections::HashMap;

async fn example() {
let platform = NativePlatform::new();
let response = platform.http()
    .get("https://example.com", &HashMap::new())
    .await
    .unwrap();
assert!(response.is_success());
}
```

## Features

- `native` (default)
- `browser`
- `browser-opfs`

## Documentation

- API docs: https://docs.rs/clawft-platform
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
