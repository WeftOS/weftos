# eml-core

EML (exp-ln) universal function approximation — O(1) learned functions from data.

Part of [WeftOS](https://github.com/WeftOS/weftos), version 0.1.0.

## Use

```toml
[dependencies]
eml-core = "0.1.0"
```

```rust
use eml_core::EmlModel;

// Create a depth-4 model with 3 inputs and 1 output head
let mut model = EmlModel::new(4, 3, 1);

// Record training data (y = x0 + x1 + x2)
for i in 0..100 {
    let x = [i as f64 / 100.0, i as f64 / 50.0, i as f64 / 200.0];
    let y = x[0] + x[1] + x[2];
    model.record(&x, &[Some(y)]);
}

// Train
let _converged = model.train();

// Predict
let prediction = model.predict_primary(&[0.5, 1.0, 0.25]);
assert!(prediction.is_finite());
```

## Features

- `experimental-attention`

## Documentation

- API docs: https://docs.rs/eml-core
- Project docs: https://weftos.weavelogic.ai/docs

## License

MIT OR Apache-2.0
