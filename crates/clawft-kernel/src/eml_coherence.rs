//! O(1) coherence approximation via EML (exp(x) - ln(y)) master formula.
//!
//! Predicts algebraic connectivity (lambda_2) from graph statistics
//! without running the expensive O(k*m) Lanczos eigenvalue iteration.
//! Based on: Odrzywolel 2026, "All elementary functions from a single operator"
//!
//! # Two-Tier Coherence Pattern (DEMOCRITUS)
//!
//! The intended usage follows a two-tier pattern:
//! - **Every tick**: `coherence_fast()` via the EML model (~0.1 us)
//! - **When drift exceeds threshold**: `spectral_analysis()` via Lanczos (~500 us),
//!   then `model.record()` to feed the training buffer
//! - **Every 1000 exact samples**: `model.train()` to refine parameters
//!
//! This module does NOT modify the cognitive tick loop. Callers are
//! responsible for implementing the two-tier cadence.
//!
//! # Architecture
//!
//! Delegates to [`eml_core::EmlModel`] for the generic EML machinery.
//! This module provides the WeftOS-specific wrappers:
//! - [`GraphFeatures`] — extracts features from a [`CausalGraph`]
//! - [`CoherencePrediction`] — domain-specific prediction output
//! - [`EmlCoherenceModel`] — thin wrapper around `eml_core::EmlModel`

use serde::{Deserialize, Serialize};

use crate::causal::CausalGraph;

// Re-export the core operator for callers that used it directly.
pub use eml_core::EmlEvent;
pub use eml_core::{eml, eml_safe, softmax3};

// ---------------------------------------------------------------------------
// CoherencePrediction
// ---------------------------------------------------------------------------

/// Multi-output coherence prediction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoherencePrediction {
    /// Primary: predicted algebraic connectivity (lambda_2).
    pub lambda_2: f64,
    /// Estimated Fiedler vector norm (spread of the weak cut).
    pub fiedler_norm: f64,
    /// Uncertainty estimate (lambda_2 confidence interval width).
    pub uncertainty: f64,
}

// ---------------------------------------------------------------------------
// GraphFeatures
// ---------------------------------------------------------------------------

/// Cheap-to-extract graph statistics used as input features for the
/// EML coherence model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphFeatures {
    /// Number of nodes |V|.
    pub node_count: f64,
    /// Number of edges |E|.
    pub edge_count: f64,
    /// Average degree: 2*|E| / |V| (undirected interpretation).
    pub avg_degree: f64,
    /// Maximum degree across all nodes.
    pub max_degree: f64,
    /// Minimum degree across all nodes.
    pub min_degree: f64,
    /// Edge density: 2*|E| / (|V| * (|V|-1)).
    pub density: f64,
    /// Number of connected components.
    pub component_count: f64,
}

impl GraphFeatures {
    /// Extract features from a [`CausalGraph`] in O(n) time.
    pub fn from_causal_graph(graph: &CausalGraph) -> Self {
        let n = graph.node_count() as f64;
        let m = graph.edge_count() as f64;

        if n < 1.0 {
            return Self {
                node_count: 0.0,
                edge_count: 0.0,
                avg_degree: 0.0,
                max_degree: 0.0,
                min_degree: 0.0,
                density: 0.0,
                component_count: 0.0,
            };
        }

        let ids = graph.node_ids();
        let mut max_deg: usize = 0;
        let mut min_deg: usize = usize::MAX;
        for &id in &ids {
            let d = graph.degree(id);
            if d > max_deg {
                max_deg = d;
            }
            if d < min_deg {
                min_deg = d;
            }
        }
        if ids.is_empty() {
            min_deg = 0;
        }

        let avg_degree = if n > 0.0 { 2.0 * m / n } else { 0.0 };
        let density = if n > 1.0 {
            2.0 * m / (n * (n - 1.0))
        } else {
            0.0
        };

        // WEFT-510: O(1) incremental component count (union-find on CausalGraph).
        let component_count = graph.component_count_fast() as f64;

        Self {
            node_count: n,
            edge_count: m,
            avg_degree,
            max_degree: max_deg as f64,
            min_degree: min_deg as f64,
            density,
            component_count,
        }
    }

    /// Normalize features to [0, 1] range for numerical stability.
    fn normalized(&self) -> [f64; 7] {
        [
            self.node_count / 10000.0,
            self.edge_count / 50000.0,
            self.avg_degree / 100.0,
            self.max_degree / 1000.0,
            self.density,
            self.component_count / 100.0,
            self.min_degree / 50.0,
        ]
    }
}

impl eml_core::FeatureVector for GraphFeatures {
    fn as_features(&self) -> Vec<f64> {
        self.normalized().to_vec()
    }

    fn feature_count() -> usize {
        7
    }
}

// ---------------------------------------------------------------------------
// EmlCoherenceModel
// ---------------------------------------------------------------------------

/// Number of trainable parameters in the depth-4 multi-head EML formula.
const PARAM_COUNT_V2: usize = 50;

/// Depth-4 multi-head EML master formula for O(1) coherence prediction.
///
/// This is a thin wrapper around [`eml_core::EmlModel`] that provides
/// the WeftOS-specific [`GraphFeatures`] and [`CoherencePrediction`] types.
///
/// Backward compatible: supports both depth-3 (34 params, single output)
/// and depth-4 (50 params, multi-head) architectures.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmlCoherenceModel {
    /// The underlying generic EML model.
    inner: eml_core::EmlModel,
    /// Prediction error history (for drift detection).
    #[serde(skip)]
    error_history: Vec<f64>,
}

impl Default for EmlCoherenceModel {
    fn default() -> Self {
        Self::new()
    }
}

impl EmlCoherenceModel {
    /// Create a new untrained depth-4 multi-head model with zeroed parameters.
    pub fn new() -> Self {
        let mut inner = eml_core::EmlModel::new(4, 7, 3);
        inner.set_model_name("coherence");
        Self {
            inner,
            error_history: Vec::new(),
        }
    }

    /// Create a new untrained depth-3 legacy model (34 params).
    pub fn new_v1() -> Self {
        let mut inner = eml_core::EmlModel::new(3, 7, 1);
        inner.set_model_name("coherence_v1");
        Self {
            inner,
            error_history: Vec::new(),
        }
    }

    /// Drain accumulated EML lifecycle events for ExoChain forwarding.
    pub fn drain_events(&mut self) -> Vec<EmlEvent> {
        self.inner.drain_events()
    }

    /// Whether this model uses the depth-4 multi-head architecture.
    pub fn is_multi_head(&self) -> bool {
        self.inner.param_count() == PARAM_COUNT_V2
    }

    /// Whether the model has been trained to convergence.
    pub fn is_trained(&self) -> bool {
        self.inner.is_trained()
    }

    /// Number of training samples collected so far.
    pub fn training_sample_count(&self) -> usize {
        self.inner.training_sample_count()
    }

    /// Mean of the recent error history (empty => 0.0).
    pub fn mean_error(&self) -> f64 {
        if self.error_history.is_empty() {
            return 0.0;
        }
        self.error_history.iter().sum::<f64>() / self.error_history.len() as f64
    }

    // -------------------------------------------------------------------
    // Prediction
    // -------------------------------------------------------------------

    /// O(1) multi-head coherence prediction from graph features.
    ///
    /// Returns a [`CoherencePrediction`] with lambda_2, fiedler_norm, and
    /// uncertainty. Falls back to a density-based estimate if untrained.
    /// For depth-3 legacy models, fiedler_norm and uncertainty are
    /// synthetic estimates derived from lambda_2.
    pub fn predict(&self, features: &GraphFeatures) -> CoherencePrediction {
        if !self.inner.is_trained() {
            // Fallback: density * avg_degree is a rough proxy for
            // algebraic connectivity in random graphs.
            let lambda_2 = features.density * features.avg_degree;
            return CoherencePrediction {
                lambda_2,
                fiedler_norm: lambda_2.sqrt().max(0.0),
                uncertainty: lambda_2 * 0.5,
            };
        }

        let inputs = features.normalized();
        let heads = self.inner.predict(&inputs);

        if self.inner.head_count() == 1 {
            // Depth-3 legacy: single head, synthetic secondary outputs
            let lambda_2 = heads[0];
            CoherencePrediction {
                lambda_2,
                fiedler_norm: lambda_2.sqrt().max(0.0),
                uncertainty: lambda_2 * 0.5,
            }
        } else {
            CoherencePrediction {
                lambda_2: heads[0],
                fiedler_norm: heads.get(1).copied().unwrap_or(0.0),
                uncertainty: heads.get(2).copied().unwrap_or(0.0),
            }
        }
    }

    /// Convenience: returns only the primary lambda_2 value.
    ///
    /// Use this when you only need the algebraic connectivity scalar
    /// (backward compatible with callers that expected `f64`).
    pub fn predict_lambda2(&self, features: &GraphFeatures) -> f64 {
        self.predict(features).lambda_2
    }

    // -------------------------------------------------------------------
    // Training
    // -------------------------------------------------------------------

    /// Record a training point (called after every exact Lanczos computation).
    ///
    /// Only records lambda_2; use [`record_full`] to also supply Fiedler norm
    /// and uncertainty ground truth.
    pub fn record(&mut self, features: GraphFeatures, lambda_2: f64) {
        self.record_full(features, lambda_2, None, None);
    }

    /// Record a full training point with optional Fiedler norm and uncertainty.
    pub fn record_full(
        &mut self,
        features: GraphFeatures,
        lambda_2: f64,
        fiedler_norm: Option<f64>,
        uncertainty: Option<f64>,
    ) {
        // Track prediction error for drift detection
        let predicted = self.predict(&features);
        self.error_history
            .push((predicted.lambda_2 - lambda_2).abs());
        if self.error_history.len() > 100 {
            self.error_history.remove(0);
        }

        let inputs = features.normalized();
        let targets = if self.inner.head_count() == 1 {
            vec![Some(lambda_2)]
        } else {
            vec![Some(lambda_2), fiedler_norm, uncertainty]
        };
        self.inner.record(&inputs, &targets);
    }

    /// Train the model when enough data is collected.
    ///
    /// Uses random restart + coordinate descent (gradient-free
    /// optimization suitable for 50 parameters).
    ///
    /// Returns `true` if the model converged (MSE < 0.01).
    pub fn train(&mut self) -> bool {
        self.inner.train()
    }
}

// ---------------------------------------------------------------------------
// CausalGraph integration
// ---------------------------------------------------------------------------

impl CausalGraph {
    /// O(1) approximate coherence from EML model.
    ///
    /// Returns a full [`CoherencePrediction`] with lambda_2, Fiedler norm,
    /// and uncertainty. Falls back to density-based estimate if model not
    /// trained.
    pub fn coherence_fast(&self, model: &EmlCoherenceModel) -> CoherencePrediction {
        let features = GraphFeatures::from_causal_graph(self);
        model.predict(&features)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::causal::{CausalEdgeType, CausalGraph};

    // -- eml operator -------------------------------------------------------

    #[test]
    fn eml_identity() {
        // eml(0, 1) = exp(0) - ln(1) = 1 - 0 = 1
        let result = eml(0.0, 1.0);
        assert!(
            (result - 1.0).abs() < 1e-12,
            "eml(0, 1) should be 1.0, got {result}"
        );
    }

    #[test]
    fn eml_exp_only() {
        // eml(1, 1) = exp(1) - ln(1) = e - 0 = e
        let result = eml(1.0, 1.0);
        assert!(
            (result - std::f64::consts::E).abs() < 1e-12,
            "eml(1, 1) should be e, got {result}"
        );
    }

    #[test]
    fn eml_ln_only() {
        // eml(0, e) = exp(0) - ln(e) = 1 - 1 = 0
        let result = eml(0.0, std::f64::consts::E);
        assert!(
            result.abs() < 1e-12,
            "eml(0, e) should be 0.0, got {result}"
        );
    }

    // -- GraphFeatures extraction -------------------------------------------

    #[test]
    fn features_empty_graph() {
        let g = CausalGraph::new();
        let f = GraphFeatures::from_causal_graph(&g);
        assert_eq!(f.node_count, 0.0);
        assert_eq!(f.edge_count, 0.0);
        assert_eq!(f.density, 0.0);
        assert_eq!(f.component_count, 0.0);
    }

    #[test]
    fn features_triangle() {
        let g = CausalGraph::new();
        let a = g.add_node("A".into(), serde_json::json!({}));
        let b = g.add_node("B".into(), serde_json::json!({}));
        let c = g.add_node("C".into(), serde_json::json!({}));
        g.link(a, b, CausalEdgeType::Causes, 1.0, 0, 0);
        g.link(b, c, CausalEdgeType::Causes, 1.0, 0, 0);
        g.link(c, a, CausalEdgeType::Causes, 1.0, 0, 0);

        let f = GraphFeatures::from_causal_graph(&g);
        assert_eq!(f.node_count, 3.0);
        assert_eq!(f.edge_count, 3.0);
        assert!((f.avg_degree - 2.0).abs() < 1e-9);
        assert_eq!(f.component_count, 1.0);
        // density = 2*3 / (3*2) = 1.0
        assert!((f.density - 1.0).abs() < 1e-9);
    }

    #[test]
    fn features_disconnected() {
        let g = CausalGraph::new();
        let _a = g.add_node("A".into(), serde_json::json!({}));
        let _b = g.add_node("B".into(), serde_json::json!({}));

        let f = GraphFeatures::from_causal_graph(&g);
        assert_eq!(f.node_count, 2.0);
        assert_eq!(f.edge_count, 0.0);
        assert_eq!(f.component_count, 2.0);
        assert_eq!(f.min_degree, 0.0);
        assert_eq!(f.max_degree, 0.0);
    }

    // -- EmlCoherenceModel prediction (untrained fallback) ------------------

    #[test]
    fn predict_untrained_fallback() {
        let model = EmlCoherenceModel::new();
        assert!(!model.is_trained());

        let features = GraphFeatures {
            node_count: 10.0,
            edge_count: 20.0,
            avg_degree: 4.0,
            max_degree: 6.0,
            min_degree: 2.0,
            density: 0.444,
            component_count: 1.0,
        };
        let result = model.predict(&features);
        // Fallback: density * avg_degree
        let expected = 0.444 * 4.0;
        assert!(
            (result.lambda_2 - expected).abs() < 1e-9,
            "untrained fallback: expected {expected}, got {}",
            result.lambda_2
        );
    }

    #[test]
    fn predict_untrained_returns_multi_head() {
        let model = EmlCoherenceModel::new();
        let features = GraphFeatures {
            node_count: 10.0,
            edge_count: 20.0,
            avg_degree: 4.0,
            max_degree: 6.0,
            min_degree: 2.0,
            density: 0.444,
            component_count: 1.0,
        };
        let pred = model.predict(&features);
        // All three heads should produce values
        assert!(pred.lambda_2 >= 0.0);
        assert!(pred.fiedler_norm >= 0.0);
        assert!(pred.uncertainty >= 0.0);
    }

    #[test]
    fn predict_lambda2_convenience() {
        let model = EmlCoherenceModel::new();
        let features = GraphFeatures {
            node_count: 10.0,
            edge_count: 20.0,
            avg_degree: 4.0,
            max_degree: 6.0,
            min_degree: 2.0,
            density: 0.444,
            component_count: 1.0,
        };
        let lambda2 = model.predict_lambda2(&features);
        let full = model.predict(&features);
        assert!(
            (lambda2 - full.lambda_2).abs() < 1e-12,
            "predict_lambda2 should match predict().lambda_2"
        );
    }

    // -- Backward compat: 34-param models still work -----------------------

    #[test]
    fn backward_compat_v1_model() {
        let mut model = EmlCoherenceModel::new_v1();
        assert!(!model.is_multi_head());

        let features = GraphFeatures {
            node_count: 10.0,
            edge_count: 20.0,
            avg_degree: 4.0,
            max_degree: 6.0,
            min_degree: 2.0,
            density: 0.444,
            component_count: 1.0,
        };

        // Untrained fallback still works
        let pred = model.predict(&features);
        let expected = 0.444 * 4.0;
        assert!(
            (pred.lambda_2 - expected).abs() < 1e-9,
            "v1 untrained fallback should match"
        );
        // Synthetic fiedler_norm and uncertainty
        assert!(pred.fiedler_norm >= 0.0);
        assert!(pred.uncertainty >= 0.0);

        // Record + train should work on v1 model
        for i in 0..60 {
            let f = GraphFeatures {
                node_count: (i + 3) as f64,
                edge_count: (i + 2) as f64,
                avg_degree: 2.0,
                max_degree: 3.0,
                min_degree: 1.0,
                density: 0.5,
                component_count: 1.0,
            };
            model.record(f, 1.0);
        }
        // Should not panic
        let _ = model.train();
    }

    // -- Multi-head model basics -------------------------------------------

    #[test]
    fn new_model_is_multi_head() {
        let model = EmlCoherenceModel::new();
        assert!(model.is_multi_head());
    }

    #[test]
    fn uncertainty_is_non_negative() {
        let model = EmlCoherenceModel::new();
        // Test across various feature combinations
        for n in [3.0, 10.0, 50.0, 100.0] {
            for d in [0.1, 0.5, 1.0] {
                let e = n * (n - 1.0) * d / 2.0;
                let features = GraphFeatures {
                    node_count: n,
                    edge_count: e,
                    avg_degree: (n - 1.0) * d,
                    max_degree: (n - 1.0) * d * 1.5,
                    min_degree: ((n - 1.0) * d * 0.5).max(0.0),
                    density: d,
                    component_count: 1.0,
                };
                let pred = model.predict(&features);
                assert!(
                    pred.uncertainty >= 0.0,
                    "uncertainty must be non-negative for n={n}, d={d}: got {}",
                    pred.uncertainty
                );
            }
        }
    }

    // -- EmlCoherenceModel record + training --------------------------------

    #[test]
    fn record_increments_count() {
        let mut model = EmlCoherenceModel::new();
        assert_eq!(model.training_sample_count(), 0);

        let f = GraphFeatures {
            node_count: 5.0,
            edge_count: 4.0,
            avg_degree: 1.6,
            max_degree: 2.0,
            min_degree: 1.0,
            density: 0.4,
            component_count: 1.0,
        };
        model.record(f, 0.5);
        assert_eq!(model.training_sample_count(), 1);
    }

    #[test]
    fn record_full_stores_targets() {
        let mut model = EmlCoherenceModel::new();
        let f = GraphFeatures {
            node_count: 5.0,
            edge_count: 4.0,
            avg_degree: 1.6,
            max_degree: 2.0,
            min_degree: 1.0,
            density: 0.4,
            component_count: 1.0,
        };
        model.record_full(f, 0.5, Some(1.2), Some(0.3));
        assert_eq!(model.training_sample_count(), 1);
    }

    #[test]
    fn train_insufficient_data_returns_false() {
        let mut model = EmlCoherenceModel::new();
        // Add only 10 samples (need 50)
        for i in 0..10 {
            let f = GraphFeatures {
                node_count: i as f64,
                edge_count: i as f64,
                avg_degree: 2.0,
                max_degree: 3.0,
                min_degree: 1.0,
                density: 0.5,
                component_count: 1.0,
            };
            model.record(f, 1.0);
        }
        assert!(!model.train());
        assert!(!model.is_trained());
    }

    // -- Convergence test with known graph families -------------------------

    #[test]
    fn convergence_on_known_graphs() {
        let mut model = EmlCoherenceModel::new();

        // Complete graph K_n: lambda_2 = n, density = 1.0
        for n in 3..30 {
            let nf = n as f64;
            let e = nf * (nf - 1.0) / 2.0;
            let lambda_2 = nf;
            let features = GraphFeatures {
                node_count: nf,
                edge_count: e,
                avg_degree: nf - 1.0,
                max_degree: nf - 1.0,
                min_degree: nf - 1.0,
                density: 1.0,
                component_count: 1.0,
            };
            model.record(features, lambda_2);
        }

        // Star graph S_n: lambda_2 = 1
        for n in 3..30 {
            let nf = n as f64;
            let features = GraphFeatures {
                node_count: nf,
                edge_count: nf - 1.0,
                avg_degree: 2.0 * (nf - 1.0) / nf,
                max_degree: nf - 1.0,
                min_degree: 1.0,
                density: 2.0 * (nf - 1.0) / (nf * (nf - 1.0)),
                component_count: 1.0,
            };
            model.record(features, 1.0);
        }

        // Cycle graph C_n: lambda_2 = 2(1 - cos(2*pi/n))
        for n in 3..30 {
            let nf = n as f64;
            let lambda_2 = 2.0 * (1.0 - (2.0 * std::f64::consts::PI / nf).cos());
            let features = GraphFeatures {
                node_count: nf,
                edge_count: nf,
                avg_degree: 2.0,
                max_degree: 2.0,
                min_degree: 2.0,
                density: 2.0 * nf / (nf * (nf - 1.0)),
                component_count: 1.0,
            };
            model.record(features, lambda_2);
        }

        // Path graph P_n: lambda_2 = 2(1 - cos(pi/n))
        for n in 3..30 {
            let nf = n as f64;
            let lambda_2 = 2.0 * (1.0 - (std::f64::consts::PI / nf).cos());
            let features = GraphFeatures {
                node_count: nf,
                edge_count: nf - 1.0,
                avg_degree: 2.0 * (nf - 1.0) / nf,
                max_degree: 2.0,
                min_degree: 1.0,
                density: 2.0 * (nf - 1.0) / (nf * (nf - 1.0)),
                component_count: 1.0,
            };
            model.record(features, lambda_2);
        }

        // Erdos-Renyi G(n, p)
        for n in [20, 50, 100, 200] {
            for &p in &[0.1, 0.2, 0.3, 0.5, 0.7] {
                let nf = n as f64;
                let e = nf * (nf - 1.0) * p / 2.0;
                let avg_deg = (nf - 1.0) * p;
                let lambda_2 = (nf * p - 2.0 * (nf * p * (1.0 - p)).sqrt()).max(0.0);
                let features = GraphFeatures {
                    node_count: nf,
                    edge_count: e,
                    avg_degree: avg_deg,
                    max_degree: avg_deg * 1.5,
                    min_degree: (avg_deg * 0.5).max(0.0),
                    density: p,
                    component_count: 1.0,
                };
                model.record(features, lambda_2);
            }
        }

        assert!(
            model.training_sample_count() >= 50,
            "should have enough training data: {}",
            model.training_sample_count()
        );

        // Train
        let _converged = model.train();

        // Model should be functional after training attempt
        let k5 = GraphFeatures {
            node_count: 5.0,
            edge_count: 10.0,
            avg_degree: 4.0,
            max_degree: 4.0,
            min_degree: 4.0,
            density: 1.0,
            component_count: 1.0,
        };
        let pred = model.predict(&k5);
        assert!(pred.lambda_2.is_finite(), "prediction should be finite");
    }

    #[test]
    fn depth4_convergence_with_full_targets() {
        let mut model = EmlCoherenceModel::new();
        assert!(model.is_multi_head());

        // Generate 100 samples with all three targets
        for n in 3..53 {
            let nf = n as f64;
            let e = nf * (nf - 1.0) / 2.0;
            let lambda_2 = nf; // K_n
            let fiedler_norm = (nf - 1.0).sqrt();
            let uncertainty = 0.1 * lambda_2;
            let features = GraphFeatures {
                node_count: nf,
                edge_count: e,
                avg_degree: nf - 1.0,
                max_degree: nf - 1.0,
                min_degree: nf - 1.0,
                density: 1.0,
                component_count: 1.0,
            };
            model.record_full(features, lambda_2, Some(fiedler_norm), Some(uncertainty));
        }

        for n in 3..53 {
            let nf = n as f64;
            let lambda_2 = 2.0 * (1.0 - (2.0 * std::f64::consts::PI / nf).cos());
            let fiedler_norm = lambda_2.sqrt();
            let uncertainty = 0.05 * lambda_2;
            let features = GraphFeatures {
                node_count: nf,
                edge_count: nf,
                avg_degree: 2.0,
                max_degree: 2.0,
                min_degree: 2.0,
                density: 2.0 * nf / (nf * (nf - 1.0)),
                component_count: 1.0,
            };
            model.record_full(features, lambda_2, Some(fiedler_norm), Some(uncertainty));
        }

        assert!(model.training_sample_count() >= 100);

        // Should not panic and should produce valid predictions
        let _ = model.train();
        let pred = model.predict(&GraphFeatures {
            node_count: 10.0,
            edge_count: 45.0,
            avg_degree: 9.0,
            max_degree: 9.0,
            min_degree: 9.0,
            density: 1.0,
            component_count: 1.0,
        });
        assert!(pred.lambda_2.is_finite());
    }

    // -- CausalGraph::coherence_fast integration ----------------------------

    #[test]
    fn coherence_fast_on_triangle() {
        let g = CausalGraph::new();
        let a = g.add_node("A".into(), serde_json::json!({}));
        let b = g.add_node("B".into(), serde_json::json!({}));
        let c = g.add_node("C".into(), serde_json::json!({}));
        g.link(a, b, CausalEdgeType::Causes, 1.0, 0, 0);
        g.link(b, c, CausalEdgeType::Causes, 1.0, 0, 0);
        g.link(c, a, CausalEdgeType::Causes, 1.0, 0, 0);

        let model = EmlCoherenceModel::new();
        let fast = g.coherence_fast(&model);
        // Untrained: density * avg_degree = 1.0 * 2.0 = 2.0
        assert!(
            (fast.lambda_2 - 2.0).abs() < 1e-9,
            "coherence_fast untrained triangle: expected 2.0, got {}",
            fast.lambda_2
        );
        // Multi-head: fiedler_norm and uncertainty should also be present
        assert!(fast.fiedler_norm >= 0.0);
        assert!(fast.uncertainty >= 0.0);
    }

    #[test]
    fn coherence_fast_empty() {
        let g = CausalGraph::new();
        let model = EmlCoherenceModel::new();
        let fast = g.coherence_fast(&model);
        assert!(
            fast.lambda_2.abs() < 1e-12,
            "coherence_fast on empty graph should be 0"
        );
    }

    // -- Helper function tests ----------------------------------------------

    #[test]
    fn softmax3_sums_to_one() {
        let (a, b, c) = softmax3(1.0, 2.0, 3.0);
        let sum = a + b + c;
        assert!(
            (sum - 1.0).abs() < 1e-12,
            "softmax3 should sum to 1.0, got {sum}"
        );
    }

    #[test]
    fn softmax3_equal_inputs() {
        let (a, b, c) = softmax3(0.0, 0.0, 0.0);
        assert!((a - 1.0 / 3.0).abs() < 1e-12);
        assert!((b - 1.0 / 3.0).abs() < 1e-12);
        assert!((c - 1.0 / 3.0).abs() < 1e-12);
    }

    #[test]
    fn eml_safe_does_not_panic() {
        // Extreme values should not panic
        let _ = eml_safe(100.0, 0.0);
        let _ = eml_safe(-100.0, -5.0);
        let _ = eml_safe(0.0, f64::MIN_POSITIVE);
        let _ = eml_safe(f64::NAN, 1.0); // NaN propagation is acceptable
    }

    #[test]
    fn error_history_tracks_drift() {
        let mut model = EmlCoherenceModel::new();
        let f = GraphFeatures {
            node_count: 5.0,
            edge_count: 5.0,
            avg_degree: 2.0,
            max_degree: 2.0,
            min_degree: 2.0,
            density: 0.5,
            component_count: 1.0,
        };

        model.record(f.clone(), 1.0);
        model.record(f.clone(), 2.0);
        assert_eq!(model.error_history.len(), 2);
        assert!(model.mean_error() >= 0.0);
    }
}
