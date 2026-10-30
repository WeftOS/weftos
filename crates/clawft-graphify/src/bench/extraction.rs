//! Extraction throughput harness (GRAPH-041 / WEFT-370).
//!
//! Default path uses a [`MockExtractor`] that builds synthetic
//! [`ExtractionResult`]s — no tree-sitter, no disk, no LLM. Real extractors
//! implement [`FileExtractor`] and plug into the same timing API.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::build;
use crate::model::{ExtractionResult, KnowledgeGraph};

use super::fixtures::{synthetic_extraction, synthetic_extractions};

/// Errors while driving an extraction bench sample.
#[derive(Debug, thiserror::Error)]
pub enum ExtractionBenchError {
    /// Extractor returned an error for a file.
    #[error("extractor error: {0}")]
    Extractor(String),
    /// No successful samples collected.
    #[error("no successful extraction samples")]
    Empty,
}

/// Seam for plugging real or mock file extractors into the harness.
pub trait FileExtractor {
    /// Human-readable label (e.g. `"mock"`, `"ast-rust"`).
    fn name(&self) -> &str;

    /// Extract entities/relationships from one logical file.
    ///
    /// `path` is a synthetic or real path; `content` may be empty for mocks.
    fn extract_file(&mut self, path: &str, content: &str) -> Result<ExtractionResult, String>;
}

/// Mock extractor: returns a synthetic extraction in constant-ish time.
#[derive(Debug, Clone)]
pub struct MockExtractor {
    /// Entities generated per file.
    pub entities_per_file: usize,
    /// Optional artificial delay (for soak / latency injection tests).
    pub delay: Duration,
    name: String,
}

impl Default for MockExtractor {
    fn default() -> Self {
        Self {
            entities_per_file: 8,
            delay: Duration::ZERO,
            name: "mock".into(),
        }
    }
}

impl MockExtractor {
    /// Create a mock with the given entities-per-file count.
    pub fn new(entities_per_file: usize) -> Self {
        Self {
            entities_per_file,
            ..Self::default()
        }
    }

    /// Named mock (shows up in sample labels).
    pub fn named(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// Inject a fixed delay per file (optional soak paths).
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }
}

impl FileExtractor for MockExtractor {
    fn name(&self) -> &str {
        &self.name
    }

    fn extract_file(&mut self, path: &str, _content: &str) -> Result<ExtractionResult, String> {
        if !self.delay.is_zero() {
            std::thread::sleep(self.delay);
        }
        // Derive a stable file index from the path when possible.
        let idx = path
            .rsplit('_')
            .next()
            .and_then(|s| s.trim_end_matches(".rs").parse::<usize>().ok())
            .unwrap_or(0);
        Ok(synthetic_extraction(idx, self.entities_per_file))
    }
}

/// One timed extraction batch.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionSample {
    /// Extractor label.
    pub extractor: String,
    /// Files processed in this sample.
    pub files: usize,
    /// Entities produced.
    pub entities: usize,
    /// Relationships produced.
    pub relationships: usize,
    /// Wall-clock duration for the batch.
    #[serde(with = "super::duration_ms")]
    pub duration: Duration,
    /// Optional free-form note.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl ExtractionSample {
    /// Files processed per second (0 if duration is zero).
    pub fn files_per_sec(&self) -> f64 {
        if self.duration.is_zero() {
            return f64::INFINITY;
        }
        self.files as f64 / self.duration.as_secs_f64()
    }

    /// Attach a note.
    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }
}

/// Aggregate stats over extraction samples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ExtractionStats {
    /// Number of samples.
    pub count: usize,
    /// Total files across samples.
    pub total_files: usize,
    /// Total entities.
    pub total_entities: usize,
    /// Sum of sample durations.
    #[serde(with = "super::duration_ms")]
    pub total_duration: Duration,
    /// Mean files/sec across samples (not global ratio).
    pub mean_files_per_sec: f64,
    /// Best (highest) files/sec.
    pub max_files_per_sec: f64,
    /// Worst files/sec.
    pub min_files_per_sec: f64,
}

impl ExtractionStats {
    /// Build stats from samples; `None` if empty.
    pub fn from_samples(samples: &[ExtractionSample]) -> Option<Self> {
        if samples.is_empty() {
            return None;
        }
        let mut total_files = 0usize;
        let mut total_entities = 0usize;
        let mut total_duration = Duration::ZERO;
        let mut rates = Vec::with_capacity(samples.len());
        for s in samples {
            total_files += s.files;
            total_entities += s.entities;
            total_duration += s.duration;
            rates.push(s.files_per_sec());
        }
        let mean = rates.iter().sum::<f64>() / rates.len() as f64;
        let max = rates.iter().cloned().fold(f64::NEG_INFINITY, f64::max);
        let min = rates.iter().cloned().fold(f64::INFINITY, f64::min);
        Some(Self {
            count: samples.len(),
            total_files,
            total_entities,
            total_duration,
            mean_files_per_sec: mean,
            max_files_per_sec: max,
            min_files_per_sec: min,
        })
    }
}

/// Timed result of assembling extractions into a [`KnowledgeGraph`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildSample {
    /// Files (extractions) merged.
    pub files: usize,
    /// Resulting entity count.
    pub entities: usize,
    /// Resulting relationship count.
    pub relationships: usize,
    /// Wall-clock merge duration.
    #[serde(with = "super::duration_ms")]
    pub duration: Duration,
}

/// Extraction throughput harness.
#[derive(Debug, Default)]
pub struct ExtractionHarness {
    samples: Vec<ExtractionSample>,
    build_samples: Vec<BuildSample>,
}

impl ExtractionHarness {
    /// Create an empty harness.
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a pre-measured sample (fixture / external timer).
    pub fn push_sample(&mut self, sample: ExtractionSample) {
        self.samples.push(sample);
    }

    /// Run `n_files` through the extractor and record one sample.
    pub fn run_batch<E: FileExtractor>(
        &mut self,
        extractor: &mut E,
        n_files: usize,
    ) -> Result<ExtractionSample, ExtractionBenchError> {
        let start = Instant::now();
        let mut entities = 0usize;
        let mut relationships = 0usize;
        for i in 0..n_files {
            let path = format!("bench/file_{i:05}.rs");
            let result = extractor
                .extract_file(&path, "")
                .map_err(ExtractionBenchError::Extractor)?;
            entities += result.entities.len();
            relationships += result.relationships.len();
        }
        let sample = ExtractionSample {
            extractor: extractor.name().to_owned(),
            files: n_files,
            entities,
            relationships,
            duration: start.elapsed(),
            note: None,
        };
        self.samples.push(sample.clone());
        Ok(sample)
    }

    /// Convenience: mock batch without managing an extractor handle.
    pub fn run_mock_batch(
        &mut self,
        n_files: usize,
        entities_per_file: usize,
    ) -> Result<ExtractionSample, ExtractionBenchError> {
        let mut mock = MockExtractor::new(entities_per_file);
        self.run_batch(&mut mock, n_files)
    }

    /// Time `build::build` over synthetic extractions of size `n_files`.
    pub fn run_build_batch(
        &mut self,
        n_files: usize,
        entities_per_file: usize,
    ) -> (Vec<ExtractionResult>, BuildSample, KnowledgeGraph) {
        let extractions = synthetic_extractions(n_files, entities_per_file);
        let start = Instant::now();
        let kg = build::build(&extractions);
        let sample = BuildSample {
            files: n_files,
            entities: kg.entity_count(),
            relationships: kg.relationship_count(),
            duration: start.elapsed(),
        };
        self.build_samples.push(sample.clone());
        (extractions, sample, kg)
    }

    /// Aggregate extraction stats.
    pub fn stats(&self) -> Result<ExtractionStats, ExtractionBenchError> {
        ExtractionStats::from_samples(&self.samples).ok_or(ExtractionBenchError::Empty)
    }

    /// Borrow collected samples.
    pub fn samples(&self) -> &[ExtractionSample] {
        &self.samples
    }

    /// Borrow build samples.
    pub fn build_samples(&self) -> &[BuildSample] {
        &self.build_samples
    }

    /// Clear all samples.
    pub fn clear(&mut self) {
        self.samples.clear();
        self.build_samples.clear();
    }
}

/// CI-scale defaults: small enough for sub-second tests.
pub const CI_FILE_COUNT: usize = 32;
/// Entities per synthetic file in CI.
pub const CI_ENTITIES_PER_FILE: usize = 4;
/// GRAPH-041 target scale (criterion / ignored soak only).
pub const TARGET_FILE_COUNT_1K: usize = 1_000;
/// GRAPH-041 acceptance scale (optional soak).
pub const TARGET_FILE_COUNT_10K: usize = 10_000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mock_batch_records_stats() {
        let mut h = ExtractionHarness::new();
        let sample = h.run_mock_batch(16, 4).unwrap();
        assert_eq!(sample.files, 16);
        assert_eq!(sample.entities, 64);
        assert!(sample.files_per_sec() > 0.0);
        let stats = h.stats().unwrap();
        assert_eq!(stats.count, 1);
        assert_eq!(stats.total_files, 16);
    }

    #[test]
    fn build_batch_produces_graph() {
        let mut h = ExtractionHarness::new();
        let (_, sample, kg) = h.run_build_batch(8, 3);
        assert_eq!(sample.files, 8);
        assert_eq!(kg.entity_count(), 24);
        assert!(!h.build_samples().is_empty());
    }

    #[test]
    fn with_note_sets_and_replaces() {
        let sample = ExtractionSample {
            extractor: "mock".into(),
            files: 1,
            entities: 1,
            relationships: 0,
            duration: Duration::from_millis(1),
            note: None,
        };
        let sample = sample.with_note("first");
        assert_eq!(sample.note.as_deref(), Some("first"));
        let sample = sample.with_note("second");
        assert_eq!(sample.note.as_deref(), Some("second"));
    }
}
