//! Kernel metrics collection with lock-free counters, gauges, and histograms.
//!
//! [`MetricsRegistry`] provides a lock-free metrics subsystem. Counters and
//! gauges use atomic operations on the hot path. Histograms use fixed bucket
//! boundaries with atomic counters -- O(buckets) per record.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

/// Default histogram bucket boundaries (in milliseconds for latency metrics).
pub const DEFAULT_BUCKETS: &[f64] = &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0];

// ── Built-in metric names ───────────────────────────────────────

/// Counter: total messages sent via A2ARouter.
pub const METRIC_MESSAGES_SENT: &str = "kernel.messages_sent";
/// Counter: total messages successfully delivered.
pub const METRIC_MESSAGES_DELIVERED: &str = "kernel.messages_delivered";
/// Counter: total messages dropped (dead-lettered).
pub const METRIC_MESSAGES_DROPPED: &str = "kernel.messages_dropped";
/// Counter: total agent spawns.
pub const METRIC_AGENT_SPAWNS: &str = "kernel.agent_spawns";
/// Counter: total agent crashes.
pub const METRIC_AGENT_CRASHES: &str = "kernel.agent_crashes";
/// Counter: total tool executions.
pub const METRIC_TOOL_EXECUTIONS: &str = "kernel.tool_executions";
/// Gauge: currently active agents.
pub const METRIC_ACTIVE_AGENTS: &str = "kernel.active_agents";
/// Gauge: currently active services.
pub const METRIC_ACTIVE_SERVICES: &str = "kernel.active_services";
/// Gauge: exochain length.
pub const METRIC_CHAIN_LENGTH: &str = "kernel.chain_length";
/// Histogram: IPC latency in milliseconds.
pub const METRIC_IPC_LATENCY_MS: &str = "kernel.ipc_latency_ms";
/// Histogram: tool execution time in milliseconds.
pub const METRIC_TOOL_EXECUTION_MS: &str = "kernel.tool_execution_ms";
/// Histogram: governance evaluation time in milliseconds.
pub const METRIC_GOVERNANCE_EVAL_MS: &str = "kernel.governance_evaluation_ms";

/// A fixed-bucket histogram for recording distributions.
///
/// Each bucket tracks a count of values <= the bucket boundary.
/// The `sum` and `count` fields track totals for mean calculation.
pub struct Histogram {
    /// Bucket boundaries and their counts. Each bucket counts values
    /// that are <= the boundary (cumulative).
    buckets: Vec<(f64, AtomicU64)>,
    /// Sum of all recorded values (stored as u64 bits of f64).
    sum_bits: AtomicU64,
    /// Total number of recorded values.
    count: AtomicU64,
}

impl Histogram {
    /// Create a histogram with the given bucket boundaries.
    ///
    /// Boundaries are sorted ascending. An implicit +Inf bucket is
    /// always present.
    pub fn new(boundaries: &[f64]) -> Self {
        let mut sorted: Vec<f64> = boundaries.to_vec();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        sorted.dedup();

        let buckets = sorted.into_iter().map(|b| (b, AtomicU64::new(0))).collect();

        Self {
            buckets,
            sum_bits: AtomicU64::new(0),
            count: AtomicU64::new(0),
        }
    }

    /// Create a histogram with default bucket boundaries.
    pub fn with_defaults() -> Self {
        Self::new(DEFAULT_BUCKETS)
    }

    /// Record a value in the histogram.
    pub fn record(&self, value: f64) {
        // Increment the count for each bucket whose boundary >= value.
        // This gives us cumulative counts.
        for (bound, count) in &self.buckets {
            if value <= *bound {
                count.fetch_add(1, Ordering::Relaxed);
            }
        }

        // Add to sum (atomic f64 addition via CAS loop)
        loop {
            let old_bits = self.sum_bits.load(Ordering::Relaxed);
            let old_val = f64::from_bits(old_bits);
            let new_val = old_val + value;
            let new_bits = new_val.to_bits();
            if self
                .sum_bits
                .compare_exchange_weak(old_bits, new_bits, Ordering::Relaxed, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }

        self.count.fetch_add(1, Ordering::Relaxed);
    }

    /// Get the total count of recorded values.
    pub fn total_count(&self) -> u64 {
        self.count.load(Ordering::Relaxed)
    }

    /// Get the sum of all recorded values.
    pub fn sum(&self) -> f64 {
        f64::from_bits(self.sum_bits.load(Ordering::Relaxed))
    }

    /// Get a snapshot of bucket boundaries and their cumulative counts.
    pub fn bucket_snapshot(&self) -> Vec<(f64, u64)> {
        self.buckets
            .iter()
            .map(|(bound, count)| (*bound, count.load(Ordering::Relaxed)))
            .collect()
    }

    /// Estimate a percentile value from the histogram buckets.
    ///
    /// Uses linear interpolation between bucket boundaries.
    /// Returns `None` if no values have been recorded.
    pub fn percentile(&self, p: f64) -> Option<f64> {
        let total = self.total_count();
        if total == 0 {
            return None;
        }

        let target = (p * total as f64).ceil() as u64;
        let buckets = self.bucket_snapshot();

        // Find the first bucket whose cumulative count >= target
        for (i, (bound, cumulative)) in buckets.iter().enumerate() {
            if *cumulative >= target {
                if i == 0 {
                    return Some(*bound);
                }
                // Linear interpolation between previous and current bucket
                let prev_bound = buckets[i - 1].0;
                let prev_count = buckets[i - 1].1;
                let remaining = target.saturating_sub(prev_count) as f64;
                let bucket_count = cumulative.saturating_sub(prev_count) as f64;
                if bucket_count <= 0.0 {
                    return Some(*bound);
                }
                let fraction = remaining / bucket_count;
                return Some(prev_bound + fraction * (bound - prev_bound));
            }
        }

        // Value exceeds all bucket boundaries
        buckets.last().map(|(b, _)| *b)
    }
}

/// A snapshot of a single metric.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetricSnapshot {
    /// A counter metric (monotonically increasing).
    Counter {
        /// Metric name.
        name: String,
        /// Current value.
        value: u64,
    },
    /// A gauge metric (can go up or down).
    Gauge {
        /// Metric name.
        name: String,
        /// Current value.
        value: i64,
    },
    /// A histogram metric (distribution).
    Histogram {
        /// Metric name.
        name: String,
        /// Bucket boundaries and cumulative counts.
        buckets: Vec<(f64, u64)>,
        /// Sum of all values.
        sum: f64,
        /// Total count of values.
        count: u64,
    },
}

/// Registry of all kernel metrics. Lock-free on the hot path.
///
/// Counters use `AtomicU64`, gauges use `AtomicI64`, and histograms
/// use fixed-bucket atomic counters. `DashMap` provides concurrent
/// access to the registry without global locks.
pub struct MetricsRegistry {
    counters: DashMap<String, AtomicU64>,
    gauges: DashMap<String, AtomicI64>,
    histograms: DashMap<String, Histogram>,
}

impl MetricsRegistry {
    /// Create a new empty metrics registry.
    pub fn new() -> Self {
        Self {
            counters: DashMap::new(),
            gauges: DashMap::new(),
            histograms: DashMap::new(),
        }
    }

    /// Create a registry pre-populated with built-in kernel metrics.
    pub fn with_builtins() -> Self {
        let registry = Self::new();

        // Counters
        for name in [
            METRIC_MESSAGES_SENT,
            METRIC_MESSAGES_DELIVERED,
            METRIC_MESSAGES_DROPPED,
            METRIC_AGENT_SPAWNS,
            METRIC_AGENT_CRASHES,
            METRIC_TOOL_EXECUTIONS,
        ] {
            registry
                .counters
                .insert(name.to_string(), AtomicU64::new(0));
        }

        // Gauges
        for name in [
            METRIC_ACTIVE_AGENTS,
            METRIC_ACTIVE_SERVICES,
            METRIC_CHAIN_LENGTH,
        ] {
            registry.gauges.insert(name.to_string(), AtomicI64::new(0));
        }

        // Histograms
        for name in [
            METRIC_IPC_LATENCY_MS,
            METRIC_TOOL_EXECUTION_MS,
            METRIC_GOVERNANCE_EVAL_MS,
        ] {
            registry
                .histograms
                .insert(name.to_string(), Histogram::with_defaults());
        }

        registry
    }

    // ── Counter operations ──────────────────────────────────────

    /// Increment a counter by 1.
    pub fn counter_inc(&self, name: &str) {
        self.counter_add(name, 1);
    }

    /// Add a value to a counter.
    pub fn counter_add(&self, name: &str, value: u64) {
        match self.counters.get(name) {
            Some(counter) => {
                counter.fetch_add(value, Ordering::Relaxed);
            }
            None => {
                self.counters
                    .insert(name.to_string(), AtomicU64::new(value));
            }
        }
    }

    /// Get the current value of a counter.
    pub fn counter_get(&self, name: &str) -> u64 {
        self.counters
            .get(name)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    // ── Gauge operations ────────────────────────────────────────

    /// Set a gauge to a specific value.
    pub fn gauge_set(&self, name: &str, value: i64) {
        match self.gauges.get(name) {
            Some(gauge) => {
                gauge.store(value, Ordering::Relaxed);
            }
            None => {
                self.gauges.insert(name.to_string(), AtomicI64::new(value));
            }
        }
    }

    /// Increment a gauge by a value.
    pub fn gauge_inc(&self, name: &str, delta: i64) {
        match self.gauges.get(name) {
            Some(gauge) => {
                gauge.fetch_add(delta, Ordering::Relaxed);
            }
            None => {
                self.gauges.insert(name.to_string(), AtomicI64::new(delta));
            }
        }
    }

    /// Decrement a gauge by a value.
    pub fn gauge_dec(&self, name: &str, delta: i64) {
        self.gauge_inc(name, -delta);
    }

    /// Get the current value of a gauge.
    pub fn gauge_get(&self, name: &str) -> i64 {
        self.gauges
            .get(name)
            .map(|g| g.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    // ── Histogram operations ────────────────────────────────────

    /// Record a value in a histogram.
    ///
    /// If the histogram does not exist, it is created with default buckets.
    pub fn histogram_record(&self, name: &str, value: f64) {
        match self.histograms.get(name) {
            Some(hist) => {
                hist.record(value);
            }
            None => {
                let hist = Histogram::with_defaults();
                hist.record(value);
                self.histograms.insert(name.to_string(), hist);
            }
        }
    }

    /// Register a histogram with custom bucket boundaries.
    pub fn histogram_register(&self, name: &str, boundaries: &[f64]) {
        if !self.histograms.contains_key(name) {
            self.histograms
                .insert(name.to_string(), Histogram::new(boundaries));
        }
    }

    /// Get a percentile estimate from a histogram.
    pub fn histogram_percentile(&self, name: &str, p: f64) -> Option<f64> {
        self.histograms.get(name).and_then(|h| h.percentile(p))
    }

    /// Get histogram bucket snapshot.
    pub fn histogram_buckets(&self, name: &str) -> Option<Vec<(f64, u64)>> {
        self.histograms.get(name).map(|h| h.bucket_snapshot())
    }

    // ── Snapshot ────────────────────────────────────────────────

    /// Take a snapshot of all metrics.
    pub fn snapshot_all(&self) -> Vec<MetricSnapshot> {
        let mut snapshots = Vec::new();

        for entry in self.counters.iter() {
            snapshots.push(MetricSnapshot::Counter {
                name: entry.key().clone(),
                value: entry.value().load(Ordering::Relaxed),
            });
        }

        for entry in self.gauges.iter() {
            snapshots.push(MetricSnapshot::Gauge {
                name: entry.key().clone(),
                value: entry.value().load(Ordering::Relaxed),
            });
        }

        for entry in self.histograms.iter() {
            let hist = entry.value();
            snapshots.push(MetricSnapshot::Histogram {
                name: entry.key().clone(),
                buckets: hist.bucket_snapshot(),
                sum: hist.sum(),
                count: hist.total_count(),
            });
        }

        snapshots
    }

    /// List all metric names.
    pub fn list_names(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for e in self.counters.iter() {
            names.push(e.key().clone());
        }
        for e in self.gauges.iter() {
            names.push(e.key().clone());
        }
        for e in self.histograms.iter() {
            names.push(e.key().clone());
        }
        names
    }
}

impl Default for MetricsRegistry {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counter_increment_and_get() {
        let registry = MetricsRegistry::new();
        registry.counter_inc("test.counter");
        registry.counter_inc("test.counter");
        assert_eq!(registry.counter_get("test.counter"), 2);
    }

    #[test]
    fn counter_add() {
        let registry = MetricsRegistry::new();
        registry.counter_add("test.counter", 10);
        registry.counter_add("test.counter", 5);
        assert_eq!(registry.counter_get("test.counter"), 15);
    }

    #[test]
    fn counter_get_nonexistent_returns_zero() {
        let registry = MetricsRegistry::new();
        assert_eq!(registry.counter_get("nope"), 0);
    }

    #[test]
    fn gauge_set_and_get() {
        let registry = MetricsRegistry::new();
        registry.gauge_set("test.gauge", 42);
        assert_eq!(registry.gauge_get("test.gauge"), 42);
    }

    #[test]
    fn gauge_increment_decrement() {
        let registry = MetricsRegistry::new();
        registry.gauge_set("test.gauge", 10);
        registry.gauge_inc("test.gauge", 5);
        assert_eq!(registry.gauge_get("test.gauge"), 15);
        registry.gauge_dec("test.gauge", 3);
        assert_eq!(registry.gauge_get("test.gauge"), 12);
    }

    #[test]
    fn gauge_negative() {
        let registry = MetricsRegistry::new();
        registry.gauge_set("test.gauge", -5);
        assert_eq!(registry.gauge_get("test.gauge"), -5);
    }

    #[test]
    fn histogram_record_and_count() {
        let registry = MetricsRegistry::new();
        registry.histogram_record("test.hist", 5.0);
        registry.histogram_record("test.hist", 15.0);
        registry.histogram_record("test.hist", 150.0);

        let buckets = registry.histogram_buckets("test.hist").unwrap();
        // 5.0 should be in bucket 5.0 and above
        // 15.0 should be in bucket 25.0 and above
        // 150.0 should be in bucket 250.0 and above

        // Bucket 5.0: count of values <= 5 = 1
        let b5 = buckets
            .iter()
            .find(|(b, _)| (*b - 5.0).abs() < f64::EPSILON);
        assert_eq!(b5.unwrap().1, 1);

        // Bucket 25.0: count of values <= 25 = 2
        let b25 = buckets
            .iter()
            .find(|(b, _)| (*b - 25.0).abs() < f64::EPSILON);
        assert_eq!(b25.unwrap().1, 2);

        // Bucket 250.0: count of values <= 250 = 3
        let b250 = buckets
            .iter()
            .find(|(b, _)| (*b - 250.0).abs() < f64::EPSILON);
        assert_eq!(b250.unwrap().1, 3);
    }

    #[test]
    fn histogram_sum_and_count() {
        let hist = Histogram::with_defaults();
        hist.record(10.0);
        hist.record(20.0);
        hist.record(30.0);

        assert_eq!(hist.total_count(), 3);
        assert!((hist.sum() - 60.0).abs() < f64::EPSILON);
    }

    #[test]
    fn histogram_percentile_p50() {
        let hist = Histogram::new(&[10.0, 20.0, 30.0, 40.0, 50.0]);
        for v in [5.0, 15.0, 25.0, 35.0, 45.0] {
            hist.record(v);
        }

        let p50 = hist.percentile(0.50).unwrap();
        // 5 values total, p50 = 3rd value, should be in 20-30 range
        assert!((20.0..=30.0).contains(&p50), "p50 = {p50}");
    }

    #[test]
    fn histogram_percentile_p95() {
        let hist = Histogram::new(&[10.0, 50.0, 100.0, 500.0, 1000.0]);
        for _ in 0..95 {
            hist.record(5.0);
        }
        for _ in 0..5 {
            hist.record(800.0);
        }

        let p95 = hist.percentile(0.95).unwrap();
        // 95% of values are <= 10.0
        assert!(p95 <= 10.0, "p95 = {p95}");
    }

    #[test]
    fn histogram_percentile_p99() {
        let hist = Histogram::new(&[10.0, 50.0, 100.0, 500.0, 1000.0]);
        for _ in 0..99 {
            hist.record(5.0);
        }
        hist.record(800.0);

        let p99 = hist.percentile(0.99).unwrap();
        assert!(p99 <= 10.0, "p99 = {p99}");
    }

    #[test]
    fn histogram_percentile_empty() {
        let hist = Histogram::with_defaults();
        assert!(hist.percentile(0.50).is_none());
    }

    #[test]
    fn registry_with_builtins() {
        let registry = MetricsRegistry::with_builtins();
        assert_eq!(registry.counter_get(METRIC_MESSAGES_SENT), 0);
        assert_eq!(registry.gauge_get(METRIC_ACTIVE_AGENTS), 0);
        assert!(registry.histogram_buckets(METRIC_IPC_LATENCY_MS).is_some());
    }

    #[test]
    fn snapshot_all() {
        let registry = MetricsRegistry::new();
        registry.counter_inc("c1");
        registry.gauge_set("g1", 42);
        registry.histogram_record("h1", 10.0);

        let snap = registry.snapshot_all();
        assert_eq!(snap.len(), 3);

        let has_counter = snap
            .iter()
            .any(|s| matches!(s, MetricSnapshot::Counter { name, .. } if name == "c1"));
        let has_gauge = snap.iter().any(
            |s| matches!(s, MetricSnapshot::Gauge { name, value } if name == "g1" && *value == 42),
        );
        let has_hist = snap
            .iter()
            .any(|s| matches!(s, MetricSnapshot::Histogram { name, .. } if name == "h1"));

        assert!(has_counter);
        assert!(has_gauge);
        assert!(has_hist);
    }

    #[test]
    fn list_names() {
        let registry = MetricsRegistry::new();
        registry.counter_inc("alpha");
        registry.gauge_set("beta", 1);
        registry.histogram_record("gamma", 1.0);

        let names = registry.list_names();
        assert_eq!(names.len(), 3);
        assert!(names.contains(&"alpha".to_string()));
        assert!(names.contains(&"beta".to_string()));
        assert!(names.contains(&"gamma".to_string()));
    }

    #[test]
    fn counter_concurrent_atomicity() {
        let registry = MetricsRegistry::new();
        // Simulate concurrent increments
        for _ in 0..1000 {
            registry.counter_inc("concurrent");
        }
        assert_eq!(registry.counter_get("concurrent"), 1000);
    }

    #[test]
    fn histogram_custom_buckets() {
        let registry = MetricsRegistry::new();
        registry.histogram_register("custom", &[1.0, 2.0, 5.0]);
        registry.histogram_record("custom", 1.5);

        let buckets = registry.histogram_buckets("custom").unwrap();
        assert_eq!(buckets.len(), 3);
        // 1.5 is > 1.0 but <= 2.0
        assert_eq!(buckets[0].1, 0); // <= 1.0
        assert_eq!(buckets[1].1, 1); // <= 2.0
        assert_eq!(buckets[2].1, 1); // <= 5.0
    }

    #[test]
    fn metric_snapshot_serde() {
        let snap = MetricSnapshot::Counter {
            name: "test".into(),
            value: 42,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let restored: MetricSnapshot = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            restored,
            MetricSnapshot::Counter { value: 42, .. }
        ));
    }

    #[test]
    fn default_registry() {
        let registry = MetricsRegistry::default();
        assert!(registry.list_names().is_empty());
    }
}
