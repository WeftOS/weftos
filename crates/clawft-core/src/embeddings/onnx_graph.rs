//! One runner for a local ONNX text-encoder graph, on either inference engine.
//!
//! * `onnx-embeddings`: ONNX Runtime through `ort` (prebuilt, linked at build
//!   time). Used by `scripts/build.sh native` on the triples pyke publishes a
//!   runtime for.
//! * `tract-embeddings`: `tract-onnx`, a pure-Rust ONNX engine (the engine
//!   ruvector's `ruvector-embed-core` uses for its non-ort backend). It needs no
//!   native library, so it builds for musl (static), x86_64-apple-darwin and
//!   every other target; the cargo-dist archives and the Docker image use it.
//!
//! Both run the same `.onnx` file and return the same last hidden state (the
//! nomic-embed-text-v1.5 vectors agree to cosine >= 0.99999 across engines;
//! `docs/adr/adr-132-pure-rust-release-embedder.md`). When both features are on,
//! ort is used.
//!
//! The graph takes `input_ids`, `attention_mask` and, when it declares one,
//! `token_type_ids`, each `[1, seq]` i64. [`OnnxGraph::run`] returns the shape
//! and row-major data of `last_hidden_state` (or the first output).

use std::path::Path;

/// Which engine this build runs ONNX graphs on.
pub const ENGINE: &str = if cfg!(feature = "onnx-embeddings") { "onnxruntime" } else { "tract" };

/// A loaded encoder graph (see the module docs).
pub struct OnnxGraph {
    has_type_ids: bool,
    inner: Engine,
}

#[cfg(feature = "onnx-embeddings")]
struct Engine {
    session: std::sync::Mutex<ort::session::Session>,
    out_name: String,
}

#[cfg(all(feature = "tract-embeddings", not(feature = "onnx-embeddings")))]
struct Engine {
    plan: std::sync::Arc<tract_onnx::prelude::TypedRunnableModel>,
    /// Graph input names in declaration order.
    inputs: Vec<String>,
    /// Index of `last_hidden_state` among the outputs (0 if not named so).
    out_index: usize,
}

impl OnnxGraph {
    /// Load `model` (an `.onnx` file). `intra_threads` sets ONNX Runtime's
    /// intra-op thread count (`None` keeps its default); tract runs on the
    /// calling thread. Errors are plain text naming the file.
    pub fn load(model: &Path, intra_threads: Option<usize>) -> Result<Self, String> {
        let inner = Engine::load(model, intra_threads)
            .map_err(|e| format!("ONNX model {} failed to load ({ENGINE}): {e}", model.display()))?;
        let has_type_ids = inner.input_names().iter().any(|n| n == "token_type_ids");
        Ok(Self { has_type_ids, inner })
    }

    /// Whether the graph takes a `token_type_ids` input.
    pub fn has_type_ids(&self) -> bool {
        self.has_type_ids
    }

    /// Run one `[1, seq]` row; returns `(shape, data)` of the hidden state.
    /// `types` is ignored when the graph has no `token_type_ids` input.
    pub fn run(&self, ids: Vec<i64>, mask: Vec<i64>, types: Vec<i64>) -> Result<(Vec<usize>, Vec<f32>), String> {
        if ids.len() != mask.len() || (self.has_type_ids && types.len() != ids.len()) {
            return Err(format!("input lengths differ: ids {} mask {} types {}", ids.len(), mask.len(), types.len()));
        }
        self.inner.run(ids, mask, if self.has_type_ids { Some(types) } else { None })
    }
}

#[cfg(feature = "onnx-embeddings")]
impl Engine {
    fn load(model: &Path, intra_threads: Option<usize>) -> Result<Self, String> {
        let s = |e: &dyn std::fmt::Display| e.to_string();
        let mut builder = ort_env().and_then(ort::session::Session::builder).map_err(|e| s(&e))?;
        if let Some(n) = intra_threads {
            builder = builder.with_intra_threads(n).map_err(|e| s(&e))?;
        }
        let session = builder.commit_from_file(model).map_err(|e| s(&e))?;
        let out_name = match session.outputs().iter().find(|o| o.name() == "last_hidden_state") {
            Some(o) => o.name().to_string(),
            None => session.outputs().first().map(|o| o.name().to_string()).ok_or("graph has no outputs")?,
        };
        Ok(Self { session: std::sync::Mutex::new(session), out_name })
    }

    fn input_names(&self) -> Vec<String> {
        let g = self.session.lock().unwrap_or_else(|e| e.into_inner());
        g.inputs().iter().map(|i| i.name().to_string()).collect()
    }

    fn run(&self, ids: Vec<i64>, mask: Vec<i64>, types: Option<Vec<i64>>) -> Result<(Vec<usize>, Vec<f32>), String> {
        let shape = vec![1_i64, ids.len() as i64];
        let t = |v: Vec<i64>, what: &str| {
            ort::value::Tensor::from_array((shape.clone(), v)).map_err(|e| format!("{what} tensor: {e}"))
        };
        let (ids_t, mask_t) = (t(ids, "input_ids")?, t(mask, "attention_mask")?);
        // `run` needs `&mut Session`; the lock is held only across the call.
        let mut sess = self.session.lock().unwrap_or_else(|e| e.into_inner());
        let outputs = match types {
            Some(ty) => sess.run(ort::inputs![
                "input_ids" => ids_t,
                "attention_mask" => mask_t,
                "token_type_ids" => t(ty, "token_type_ids")?,
            ]),
            None => sess.run(ort::inputs!["input_ids" => ids_t, "attention_mask" => mask_t]),
        }
        .map_err(|e| format!("inference error: {e}"))?;
        let (shp, data) = outputs[self.out_name.as_str()]
            .try_extract_tensor::<f32>()
            .map_err(|e| format!("extract error: {e}"))?;
        Ok((shp.iter().map(|&d| d.max(0) as usize).collect(), data.to_vec()))
    }
}

/// The process-wide ONNX Runtime environment. ort rc.15 allows one
/// `ort::init().build()` per process and offers no way to fetch it back, so the
/// first crate to need it builds it and every other crate reuses it through
/// `clawft_types::singleton` (core, kernel and the voice crates share a daemon).
#[cfg(feature = "onnx-embeddings")]
pub(crate) fn ort_env() -> ort::Result<&'static ort::environment::Environment> {
    clawft_types::singleton::process_singleton(|| ort::init().build())
}

#[cfg(all(feature = "tract-embeddings", not(feature = "onnx-embeddings")))]
impl Engine {
    fn load(model: &Path, _intra_threads: Option<usize>) -> Result<Self, String> {
        use tract_onnx::prelude::*;
        let s = |e: &dyn std::fmt::Display| format!("{e:#}");
        let mut m = tract_onnx::onnx().model_for_path(model).map_err(|e| s(&e))?;
        // Declare every input as [batch_size, sequence_length] i64 using the
        // graph's OWN symbol names. HF exports carry those names in their
        // value_info, so a fixed shape (or a fresh symbol) fails to unify at the
        // first Gather; reusing them gives one plan for every sequence length.
        let (b, sq) = (m.symbols.sym("batch_size"), m.symbols.sym("sequence_length"));
        for i in 0..m.inputs.len() {
            let fact = InferenceFact::dt_shape(i64::datum_type(), tvec![b.to_dim(), sq.to_dim()]);
            m = m.with_input_fact(i, fact).map_err(|e| s(&e))?;
        }
        let inputs: Vec<String> = m.inputs.iter().map(|o| m.node(o.node).name.clone()).collect();
        let out_index = m
            .outputs
            .iter()
            .position(|o| m.outlet_label(*o).unwrap_or(m.node(o.node).name.as_str()) == "last_hidden_state")
            .unwrap_or(0);
        let plan = m.into_optimized().map_err(|e| s(&e))?.into_runnable().map_err(|e| s(&e))?;
        Ok(Self { plan, inputs, out_index })
    }

    fn input_names(&self) -> Vec<String> {
        self.inputs.clone()
    }

    fn run(&self, ids: Vec<i64>, mask: Vec<i64>, types: Option<Vec<i64>>) -> Result<(Vec<usize>, Vec<f32>), String> {
        use tract_onnx::prelude::*;
        let n = ids.len();
        let tensor = |v: Vec<i64>| -> Result<TValue, String> {
            let a = tract_ndarray::Array2::from_shape_vec((1, n), v).map_err(|e| e.to_string())?;
            Ok(Tensor::from(a).into())
        };
        let (mut ids, mut mask, mut types) = (Some(ids), Some(mask), types);
        let mut feed: TVec<TValue> = tvec![];
        for name in &self.inputs {
            let v = match name.as_str() {
                "input_ids" => ids.take(),
                "attention_mask" => mask.take(),
                "token_type_ids" => types.take(),
                other => return Err(format!("unexpected graph input {other:?}")),
            };
            feed.push(tensor(v.ok_or_else(|| format!("graph input {name:?} given twice or missing"))?)?);
        }
        let out = self.plan.run(feed).map_err(|e| format!("inference error: {e:#}"))?;
        let t = out.get(self.out_index).ok_or("graph produced no output")?;
        let view = t.to_plain_array_view::<f32>().map_err(|e| format!("extract error: {e:#}"))?;
        Ok((view.shape().to_vec(), view.iter().copied().collect()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_model_is_an_error_naming_the_file_and_engine() {
        let err = OnnxGraph::load(Path::new("/nonexistent/onnx/model.onnx"), None).err().expect("must fail");
        assert!(err.contains("/nonexistent/onnx/model.onnx"), "{err}");
        assert!(err.contains(ENGINE), "{err}");
    }

    #[cfg(feature = "onnx-embeddings")]
    #[test]
    fn ort_env_is_built_once_and_shared() {
        let a = ort_env().expect("environment builds");
        let b = ort_env().expect("environment is reused");
        assert!(std::ptr::eq(a, b));
    }
}
