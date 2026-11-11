//! WeaverEngine: ECC-powered codebase modeling service (K3c-G1).
//!
//! The WeaverEngine is a [`SystemService`] that drives the ECC cognitive
//! substrate to model real-world data sources (git logs, file trees, CI
//! pipelines, documentation). It manages [`ModelingSession`]s, evaluates
//! confidence via the causal graph, and records its own decisions in the
//! Meta-Loom for self-improvement tracking.
//!
//! This module requires the `ecc` feature.

use std::collections::{HashMap, HashSet, VecDeque};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::causal::{CausalEdgeType, CausalGraph};
use crate::embedding::{EmbeddingProvider, MockEmbeddingProvider};
use crate::health::HealthStatus;
use crate::hnsw_service::HnswService;
use crate::impulse::{ImpulseQueue, ImpulseType};
use crate::service::{ServiceType, SystemService};

// ---------------------------------------------------------------------------
// WeaverCommand (IPC messages from CLI / agents)
// ---------------------------------------------------------------------------

/// Commands sent to the WeaverEngine via IPC.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WeaverCommand {
    /// Start a new modeling session.
    SessionStart {
        domain: String,
        git_path: Option<PathBuf>,
        context: Option<String>,
        goal: Option<String>,
    },
    /// Resume an existing session.
    SessionResume { domain: String },
    /// Stop a session.
    SessionStop { domain: String },
    /// Watch session progress (streaming).
    SessionWatch { domain: String },
    /// Add a data source to a session.
    SourceAdd {
        domain: String,
        source_type: String,
        root: Option<PathBuf>,
        watch: bool,
    },
    /// List sources for a session.
    SourceList { domain: String },
    /// Query confidence.
    Confidence {
        domain: String,
        edge: Option<String>,
        verbose: bool,
    },
    /// Export model.
    Export {
        domain: String,
        min_confidence: f64,
        output: PathBuf,
    },
    /// Import model.
    Import { domain: String, input: PathBuf },
    /// Query meta-loom status.
    MetaStatus { domain: String },
    /// List learned strategies.
    MetaStrategies,
    /// Export knowledge base.
    MetaExportKb { output: PathBuf },
    /// Stitch two domains.
    Stitch {
        source: String,
        target: String,
        output: String,
    },
}

// ---------------------------------------------------------------------------
// WeaverResponse
// ---------------------------------------------------------------------------

/// Responses from the WeaverEngine to CLI / agents.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum WeaverResponse {
    /// Session started successfully.
    SessionStarted { domain: String, session_id: String },
    /// Session stopped.
    SessionStopped { domain: String },
    /// Session resumed.
    SessionResumed { domain: String },
    /// Confidence report.
    ConfidenceReport(ConfidenceReport),
    /// Source added.
    SourceAdded { domain: String, source_type: String },
    /// Sources listed.
    Sources(Vec<String>),
    /// Model exported.
    Exported { path: PathBuf, edges: usize },
    /// Model imported.
    Imported { domain: String },
    /// Learned strategies.
    Strategies(Vec<StrategyPattern>),
    /// Knowledge base exported.
    KbExported { path: PathBuf },
    /// Error.
    Error(String),
}

// ---------------------------------------------------------------------------
// DataSource
// ---------------------------------------------------------------------------

/// A data source that can be ingested by the WeaverEngine.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DataSource {
    /// Git commit history.
    GitLog { path: PathBuf },
    /// File system tree.
    FileTree { root: PathBuf },
    /// CI pipeline events.
    CiPipeline { url: String },
    /// Issue tracker feed.
    IssueTracker { url: String },
    /// Documentation corpus.
    Documentation { root: PathBuf },
    /// SPARC planning artifacts.
    SparcPlan { root: PathBuf },
    /// User-defined stream.
    CustomStream { name: String },
}

impl DataSource {
    /// Human-readable type name.
    pub fn type_name(&self) -> &str {
        match self {
            Self::GitLog { .. } => "git_log",
            Self::FileTree { .. } => "file_tree",
            Self::CiPipeline { .. } => "ci_pipeline",
            Self::IssueTracker { .. } => "issue_tracker",
            Self::Documentation { .. } => "documentation",
            Self::SparcPlan { .. } => "sparc_plan",
            Self::CustomStream { .. } => "custom_stream",
        }
    }
}

// ---------------------------------------------------------------------------
// ModelingSession
// ---------------------------------------------------------------------------

/// An active or suspended modeling session for a single domain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelingSession {
    /// Unique session identifier.
    pub id: String,
    /// Domain name (e.g., project name).
    pub domain: String,
    /// When the session was started.
    pub started_at: DateTime<Utc>,
    /// Current overall confidence (0.0 .. 1.0).
    pub confidence: f64,
    /// Identified confidence gaps.
    pub gaps: Vec<ConfidenceGap>,
    /// Data sources that have been ingested.
    pub sources_ingested: Vec<String>,
    /// Number of cognitive ticks processed.
    pub tick_count: u64,
    /// Remaining budget for this session.
    pub budget_remaining_ms: u64,
    /// Whether the session is currently active.
    pub active: bool,
    /// Arbitrary metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

// ---------------------------------------------------------------------------
// ConfidenceGap / ConfidenceReport
// ---------------------------------------------------------------------------

/// A gap in the model's confidence for a specific domain area.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceGap {
    /// Area name.
    pub domain: String,
    /// Current confidence level.
    pub current_confidence: f64,
    /// Target confidence level.
    pub target_confidence: f64,
    /// Suggested sources to improve confidence.
    pub suggested_sources: Vec<String>,
}

/// Full confidence report for a modeling session.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceReport {
    /// Overall confidence.
    pub overall: f64,
    /// Per-domain gap analysis.
    pub gaps: Vec<ConfidenceGap>,
    /// Modeling suggestions.
    pub suggestions: Vec<ModelingSuggestion>,
}

/// Suggestions for improving model quality.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ModelingSuggestion {
    /// Add a new data source.
    AddSource { source_type: String, reason: String },
    /// Refine an edge type relationship.
    RefineEdgeType { from: String, to: String },
    /// Split a category into subcategories.
    SplitCategory { category: String },
    /// Increase observation window.
    ExtendObservation { domain: String },
}

// ---------------------------------------------------------------------------
// ExportedModel (K3c-G4)
// ---------------------------------------------------------------------------

/// Serialized model for edge deployment or offline analysis.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedModel {
    /// Schema version.
    pub version: String,
    /// Domain this model was built for.
    pub domain: String,
    /// When the export was created.
    pub exported_at: DateTime<Utc>,
    /// Overall confidence at export time.
    pub confidence: f64,
    /// Node type specifications.
    pub node_types: Vec<NodeTypeSpec>,
    /// Edge type specifications.
    pub edge_types: Vec<EdgeTypeSpec>,
    /// Exported causal nodes.
    pub causal_nodes: Vec<ExportedCausalNode>,
    /// Exported causal edges.
    pub causal_edges: Vec<ExportedCausalEdge>,
    /// Arbitrary metadata.
    pub metadata: HashMap<String, serde_json::Value>,
}

/// Node type specification in an exported model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeTypeSpec {
    /// Type name.
    pub name: String,
    /// Embedding strategy identifier.
    pub embedding_strategy: String,
    /// Vector dimensions.
    pub dimensions: usize,
}

/// Edge type specification in an exported model.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeTypeSpec {
    /// Source node type.
    pub from_type: String,
    /// Target node type.
    pub to_type: String,
    /// Edge type name.
    pub edge_type: String,
    /// Confidence for this edge type.
    pub confidence: f64,
}

/// Exported causal node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedCausalNode {
    /// Node label.
    pub label: String,
    /// Node metadata.
    pub metadata: serde_json::Value,
}

/// Exported causal edge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExportedCausalEdge {
    /// Source node label.
    pub source_label: String,
    /// Target node label.
    pub target_label: String,
    /// Edge type.
    pub edge_type: String,
    /// Edge weight.
    pub weight: f32,
}

// ---------------------------------------------------------------------------
// MetaLoomEvent (K3c-G5)
// ---------------------------------------------------------------------------

/// Records a Weaver modeling decision in the Meta-Loom.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaLoomEvent {
    /// Domain of the active session.
    pub session_domain: String,
    /// Type of modeling decision.
    pub decision_type: MetaDecisionType,
    /// Confidence before the decision.
    pub confidence_before: f64,
    /// Confidence after (filled in by next tick).
    pub confidence_after: Option<f64>,
    /// Human-readable rationale.
    pub rationale: String,
    /// When the decision was made.
    pub timestamp: DateTime<Utc>,
}

/// Classification of meta-loom decisions.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum MetaDecisionType {
    /// A new data source was added.
    SourceAdded { source_type: String },
    /// A new edge type relationship was created.
    EdgeTypeCreated {
        from: String,
        to: String,
        edge_type: String,
    },
    /// An edge type was removed.
    EdgeTypeRemoved { from: String, to: String },
    /// Embedding strategy changed for a node type.
    EmbeddingStrategyChanged {
        node_type: String,
        old: String,
        new: String,
    },
    /// Tick interval was adjusted.
    TickIntervalAdjusted { old_ms: u64, new_ms: u64 },
    /// Model version was bumped.
    ModelVersionBumped { from: u32, to: u32 },
    /// A new strategy was learned.
    StrategyLearned { pattern: String },
}

// ---------------------------------------------------------------------------
// StrategyPattern / WeaverKnowledgeBase
// ---------------------------------------------------------------------------

/// A learned modeling strategy from cross-domain experience.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyPattern {
    /// Decision type that led to improvement.
    pub decision_type: String,
    /// Domain context where it was learned.
    pub context: String,
    /// Confidence improvement observed.
    pub improvement: f64,
    /// When the strategy was learned.
    pub timestamp: DateTime<Utc>,
}

/// Cross-domain knowledge base that accumulates successful strategies.
pub struct WeaverKnowledgeBase {
    /// Learned strategies.
    strategies: RwLock<Vec<StrategyPattern>>,
    /// Strategy count.
    strategy_count: AtomicU64,
}

impl Default for WeaverKnowledgeBase {
    fn default() -> Self {
        Self {
            strategies: RwLock::new(Vec::new()),
            strategy_count: AtomicU64::new(0),
        }
    }
}

impl WeaverKnowledgeBase {
    /// Create a new, empty knowledge base.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a successful strategy.
    pub fn record_strategy(&self, pattern: StrategyPattern) {
        if let Ok(mut strategies) = self.strategies.write() {
            strategies.push(pattern);
            self.strategy_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// List all learned strategies.
    pub fn list_strategies(&self) -> Vec<StrategyPattern> {
        self.strategies
            .read()
            .map(|s| s.clone())
            .unwrap_or_default()
    }

    /// Find strategies relevant to a given domain (simple substring match).
    pub fn strategies_for(&self, domain: &str) -> Vec<StrategyPattern> {
        self.strategies
            .read()
            .map(|all| {
                all.iter()
                    .filter(|s| s.context.contains(domain) || domain.contains(&s.context))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Export the full knowledge base as JSON.
    pub fn export(&self) -> serde_json::Value {
        serde_json::to_value(self.list_strategies()).unwrap_or_default()
    }

    /// Total number of learned strategies.
    pub fn count(&self) -> u64 {
        self.strategy_count.load(Ordering::Relaxed)
    }
}

// ---------------------------------------------------------------------------
// TickResult
// ---------------------------------------------------------------------------

/// Outcome of a single cognitive tick for the WeaverEngine.
#[non_exhaustive]
#[derive(Debug)]
pub enum TickResult {
    /// No active session; engine is idle.
    Idle,
    /// Budget exhausted before work could be done.
    BudgetExhausted,
    /// Progress was made.
    Progress {
        /// Current overall confidence.
        confidence: f64,
        /// Number of remaining gaps.
        gaps_remaining: usize,
    },
}

// ---------------------------------------------------------------------------
// WeaverError
// ---------------------------------------------------------------------------

/// Errors produced by the WeaverEngine.
#[non_exhaustive]
#[derive(Debug)]
pub enum WeaverError {
    /// I/O error reading a file.
    Io(std::io::Error),
    /// JSON parsing error.
    Json(serde_json::Error),
    /// Domain logic error.
    Domain(String),
}

impl fmt::Display for WeaverError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(e) => write!(f, "weaver I/O error: {e}"),
            Self::Json(e) => write!(f, "weaver JSON error: {e}"),
            Self::Domain(msg) => write!(f, "weaver error: {msg}"),
        }
    }
}

impl std::error::Error for WeaverError {}

impl From<std::io::Error> for WeaverError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

impl From<serde_json::Error> for WeaverError {
    fn from(e: serde_json::Error) -> Self {
        Self::Json(e)
    }
}

// ---------------------------------------------------------------------------
// IngestResult
// ---------------------------------------------------------------------------

/// Statistics from ingesting a graph file into the WeaverEngine.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct IngestResult {
    /// Number of causal graph nodes created.
    pub nodes_added: usize,
    /// Number of causal graph edges created.
    pub edges_added: usize,
    /// Number of HNSW embeddings created.
    pub embeddings_created: usize,
    /// Source identifier (e.g., "git-history", "module-deps").
    pub source: String,
}

// ---------------------------------------------------------------------------
// GitPoller (incremental git change detection)
// ---------------------------------------------------------------------------

/// Incremental git polling state — detects new commits since last check.
pub struct GitPoller {
    /// Repository path.
    repo_path: PathBuf,
    /// Last known commit hash.
    last_known_hash: Option<String>,
    /// Branch to poll.
    branch: String,
    /// Polling enabled flag.
    enabled: bool,
}

impl GitPoller {
    /// Create a new poller for the given repository path and branch.
    pub fn new(repo_path: PathBuf, branch: String) -> Self {
        Self {
            repo_path,
            last_known_hash: None,
            branch,
            enabled: true,
        }
    }

    /// Check for new commits since last poll.
    /// Returns the number of new commits found (0 if none or on error).
    pub fn poll(&mut self) -> usize {
        if !self.enabled {
            return 0;
        }

        let repo_str = self.repo_path.to_str().unwrap_or(".");
        let output = std::process::Command::new("git")
            .args(["-C", repo_str, "rev-parse", "HEAD"])
            .output();

        match output {
            Ok(out) if out.status.success() => {
                let current_hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if self.last_known_hash.as_deref() == Some(&current_hash) {
                    return 0;
                }

                let count = if let Some(ref last) = self.last_known_hash {
                    let count_output = std::process::Command::new("git")
                        .args([
                            "-C",
                            repo_str,
                            "rev-list",
                            "--count",
                            &format!("{}..{}", last, current_hash),
                        ])
                        .output();
                    match count_output {
                        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
                            .trim()
                            .parse()
                            .unwrap_or(1),
                        _ => 1,
                    }
                } else {
                    1 // First poll — at least 1 commit exists
                };

                self.last_known_hash = Some(current_hash);
                count
            }
            _ => 0,
        }
    }

    /// Get the last known commit hash.
    pub fn last_hash(&self) -> Option<&str> {
        self.last_known_hash.as_deref()
    }

    /// Get the branch being polled.
    pub fn branch(&self) -> &str {
        &self.branch
    }

    /// Whether polling is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enable or disable polling.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

// ---------------------------------------------------------------------------
// FileWatcher (mtime-based change detection)
// ---------------------------------------------------------------------------

/// Simple file change detector using modification timestamps.
///
/// Avoids the `notify` crate dependency by comparing cached mtimes on each
/// poll call. Only watches files that have been explicitly registered.
pub struct FileWatcher {
    /// Watched paths with their last known mtime.
    watched: HashMap<PathBuf, SystemTime>,
    /// Root directory to scan for initial registration.
    root: PathBuf,
    /// File patterns to match (e.g., `"*.rs"`, `"Cargo.toml"`).
    patterns: Vec<String>,
    /// Enabled flag.
    enabled: bool,
}

impl FileWatcher {
    /// Create a new file watcher for the given root and patterns.
    pub fn new(root: PathBuf, patterns: Vec<String>) -> Self {
        Self {
            watched: HashMap::new(),
            root,
            patterns,
            enabled: true,
        }
    }

    /// Scan watched files for mtime changes since last check.
    /// Returns paths of files that changed or were deleted.
    pub fn poll_changes(&mut self) -> Vec<PathBuf> {
        if !self.enabled {
            return vec![];
        }

        let mut changed = Vec::new();
        let entries: Vec<PathBuf> = self.watched.keys().cloned().collect();

        for path in entries {
            if let Ok(metadata) = std::fs::metadata(&path) {
                if let Ok(mtime) = metadata.modified()
                    && let Some(last_mtime) = self.watched.get(&path)
                    && mtime > *last_mtime
                {
                    changed.push(path.clone());
                    self.watched.insert(path, mtime);
                }
            } else {
                // File deleted
                changed.push(path.clone());
                self.watched.remove(&path);
            }
        }

        changed
    }

    /// Register a single file to watch.
    pub fn watch(&mut self, path: PathBuf) {
        if let Ok(metadata) = std::fs::metadata(&path)
            && let Ok(mtime) = metadata.modified()
        {
            self.watched.insert(path, mtime);
        }
    }

    /// Register all files matching patterns in the root directory (non-recursive).
    pub fn watch_directory(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    let matches = self.patterns.iter().any(|p| {
                        if p.starts_with("*.") {
                            name.ends_with(&p[1..])
                        } else {
                            name == p
                        }
                    });
                    if matches {
                        self.watch(path);
                    }
                }
            }
        }
    }

    /// Number of watched files.
    pub fn watched_count(&self) -> usize {
        self.watched.len()
    }

    /// Whether file watching is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Enable or disable file watching.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }
}

// ---------------------------------------------------------------------------
// CognitiveTickResult
// ---------------------------------------------------------------------------

/// Detailed outcome of a single cognitive tick processed by the WeaverEngine.
///
/// Complements the existing [`TickResult`] enum with per-tick metrics for the
/// CognitiveTick integration (git polling, file watching, ingestion progress).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CognitiveTickResult {
    /// Which tick number this result corresponds to.
    pub tick_number: u64,
    /// Actual wall-clock time consumed by this tick (ms).
    pub elapsed_ms: u32,
    /// Budget allocated for this tick (ms).
    pub budget_ms: u32,
    /// Number of new git commits detected during this tick.
    pub git_commits_found: usize,
    /// Number of source files that changed since last tick.
    pub files_changed: usize,
    /// Number of pending nodes processed during ingestion phase.
    pub nodes_processed: usize,
    /// Whether the confidence report was recomputed this tick.
    pub confidence_updated: bool,
    /// Whether the tick completed within its budget.
    pub within_budget: bool,
}

// ---------------------------------------------------------------------------
// ConfidenceHistory (Item 1: confidence history tracking)
// ---------------------------------------------------------------------------

/// What triggered a confidence measurement.
#[non_exhaustive]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ConfidenceTrigger {
    /// Every N ticks.
    Periodic,
    /// After a graph file was ingested.
    PostIngestion,
    /// Explicit evaluation request.
    Manual,
    /// After a modeling adjustment.
    StrategyChange,
}

/// A point-in-time confidence snapshot for history tracking.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceSnapshot {
    /// Timestamp of this measurement.
    pub timestamp: DateTime<Utc>,
    /// Tick number when measured.
    pub tick_number: u64,
    /// Overall confidence score.
    pub confidence: f64,
    /// Number of nodes in the graph.
    pub node_count: usize,
    /// Number of edges in the graph.
    pub edge_count: usize,
    /// Number of gaps identified.
    pub gap_count: usize,
    /// What triggered this measurement.
    pub trigger: ConfidenceTrigger,
}

/// Direction of a confidence trend.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrendDirection {
    /// Confidence is improving over time.
    Improving,
    /// Confidence is roughly stable.
    Stable,
    /// Confidence is declining over time.
    Declining,
}

/// Summary of confidence movement over a window of snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConfidenceTrend {
    /// Overall direction.
    pub direction: TrendDirection,
    /// Change over the window (last - first).
    pub delta: f64,
    /// Average confidence in the window.
    pub avg_confidence: f64,
    /// Number of samples in the window.
    pub samples: usize,
}

/// Ring-buffer of confidence snapshots.
pub struct ConfidenceHistory {
    snapshots: VecDeque<ConfidenceSnapshot>,
    max_entries: usize,
}

impl ConfidenceHistory {
    /// Create a new history with the given capacity.
    pub fn new(max_entries: usize) -> Self {
        Self {
            snapshots: VecDeque::with_capacity(max_entries),
            max_entries,
        }
    }

    /// Record a new snapshot, evicting the oldest if at capacity.
    pub fn record(&mut self, snapshot: ConfidenceSnapshot) {
        if self.snapshots.len() >= self.max_entries {
            self.snapshots.pop_front();
        }
        self.snapshots.push_back(snapshot);
    }

    /// Get the most recent snapshot.
    pub fn latest(&self) -> Option<&ConfidenceSnapshot> {
        self.snapshots.back()
    }

    /// Compute the trend over the last `last_n` snapshots.
    pub fn trend(&self, last_n: usize) -> ConfidenceTrend {
        let n = last_n.min(self.snapshots.len());
        if n == 0 {
            return ConfidenceTrend {
                direction: TrendDirection::Stable,
                delta: 0.0,
                avg_confidence: 0.0,
                samples: 0,
            };
        }

        let start = self.snapshots.len() - n;
        let window: Vec<&ConfidenceSnapshot> = self.snapshots.iter().skip(start).collect();

        let sum: f64 = window.iter().map(|s| s.confidence).sum();
        let avg = sum / n as f64;
        let first = window.first().map(|s| s.confidence).unwrap_or(0.0);
        let last = window.last().map(|s| s.confidence).unwrap_or(0.0);
        let delta = last - first;

        let direction = if delta > 0.01 {
            TrendDirection::Improving
        } else if delta < -0.01 {
            TrendDirection::Declining
        } else {
            TrendDirection::Stable
        };

        ConfidenceTrend {
            direction,
            delta,
            avg_confidence: avg,
            samples: n,
        }
    }

    /// Get all snapshots.
    pub fn all(&self) -> &VecDeque<ConfidenceSnapshot> {
        &self.snapshots
    }

    /// Number of recorded snapshots.
    pub fn len(&self) -> usize {
        self.snapshots.len()
    }

    /// Whether the history is empty.
    pub fn is_empty(&self) -> bool {
        self.snapshots.is_empty()
    }
}

// ---------------------------------------------------------------------------
// StrategyTracker (Item 4: strategy effectiveness tracking)
// ---------------------------------------------------------------------------

/// Handle returned by `begin_strategy` to pair with `complete_strategy`.
#[derive(Debug)]
pub struct StrategyHandle {
    /// Strategy name.
    pub name: String,
    /// Description of the change.
    pub description: String,
    /// Confidence at the start of the strategy.
    pub confidence_before: f64,
    /// When the strategy was started.
    pub started_at: DateTime<Utc>,
}

/// A record of a strategy change and its impact on confidence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyOutcome {
    /// What was changed.
    pub strategy: String,
    /// Description of the change.
    pub description: String,
    /// Confidence before the change.
    pub confidence_before: f64,
    /// Confidence after the change.
    pub confidence_after: f64,
    /// Delta (positive = improvement).
    pub delta: f64,
    /// When the change was made.
    pub timestamp: DateTime<Utc>,
    /// Whether this was beneficial (delta > 0.01).
    pub beneficial: bool,
}

/// Tracker that learns which strategy changes improve confidence.
pub struct StrategyTracker {
    outcomes: Vec<StrategyOutcome>,
    max_outcomes: usize,
}

impl StrategyTracker {
    /// Create a new tracker with the given capacity.
    pub fn new(max_outcomes: usize) -> Self {
        Self {
            outcomes: Vec::new(),
            max_outcomes,
        }
    }

    /// Begin tracking a strategy. Returns a handle for `complete_strategy`.
    pub fn begin_strategy(
        &self,
        name: &str,
        description: &str,
        current_confidence: f64,
    ) -> StrategyHandle {
        StrategyHandle {
            name: name.to_string(),
            description: description.to_string(),
            confidence_before: current_confidence,
            started_at: Utc::now(),
        }
    }

    /// Complete a strategy and record its outcome.
    pub fn complete_strategy(&mut self, handle: StrategyHandle, new_confidence: f64) {
        let delta = new_confidence - handle.confidence_before;
        let outcome = StrategyOutcome {
            strategy: handle.name,
            description: handle.description,
            confidence_before: handle.confidence_before,
            confidence_after: new_confidence,
            delta,
            timestamp: handle.started_at,
            beneficial: delta > 0.01,
        };

        if self.outcomes.len() >= self.max_outcomes {
            self.outcomes.remove(0);
        }
        self.outcomes.push(outcome);
    }

    /// Get the most effective strategies, sorted by delta descending.
    pub fn most_effective(&self, top_n: usize) -> Vec<&StrategyOutcome> {
        let mut sorted: Vec<&StrategyOutcome> = self.outcomes.iter().collect();
        sorted.sort_by(|a, b| {
            b.delta
                .partial_cmp(&a.delta)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        sorted.truncate(top_n);
        sorted
    }

    /// Get strategies that hurt confidence (negative delta).
    pub fn harmful_strategies(&self) -> Vec<&StrategyOutcome> {
        self.outcomes.iter().filter(|o| o.delta < -0.01).collect()
    }

    /// Recommend next strategy based on past effectiveness.
    ///
    /// Returns the name of the most effective beneficial strategy,
    /// or `None` if no beneficial strategies have been recorded.
    pub fn recommend(&self) -> Option<String> {
        self.outcomes
            .iter()
            .filter(|o| o.beneficial)
            .max_by(|a, b| {
                a.delta
                    .partial_cmp(&b.delta)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|o| o.strategy.clone())
    }

    /// All recorded outcomes.
    pub fn outcomes(&self) -> &[StrategyOutcome] {
        &self.outcomes
    }

    /// Number of recorded outcomes.
    pub fn len(&self) -> usize {
        self.outcomes.len()
    }

    /// Whether the tracker is empty.
    pub fn is_empty(&self) -> bool {
        self.outcomes.is_empty()
    }
}

// ---------------------------------------------------------------------------
// TickHistory / TickRecommendation (Item 6: tick interval recommendation)
// ---------------------------------------------------------------------------

/// Tick interval recommendation based on observed change patterns.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TickRecommendation {
    /// Recommended interval in milliseconds.
    pub recommended_ms: u32,
    /// Current interval.
    pub current_ms: u32,
    /// Reason for recommendation.
    pub reason: String,
    /// Observed changes per minute.
    pub changes_per_minute: f64,
    /// Confidence in this recommendation (0.0 - 1.0).
    pub recommendation_confidence: f64,
}

/// Ring-buffer of recent tick results for analysis.
pub struct TickHistory {
    results: VecDeque<CognitiveTickResult>,
    max_entries: usize,
}

impl TickHistory {
    /// Create a new tick history with the given capacity.
    pub fn new(max_entries: usize) -> Self {
        Self {
            results: VecDeque::with_capacity(max_entries),
            max_entries,
        }
    }

    /// Record a tick result, evicting the oldest if at capacity.
    pub fn record(&mut self, result: CognitiveTickResult) {
        if self.results.len() >= self.max_entries {
            self.results.pop_front();
        }
        self.results.push_back(result);
    }

    /// Compute the average changes per minute based on recorded ticks.
    ///
    /// Uses total elapsed time and total change events to derive rate.
    pub fn changes_per_minute(&self) -> f64 {
        if self.results.len() < 2 {
            return 0.0;
        }

        let total_changes: usize = self
            .results
            .iter()
            .map(|r| r.git_commits_found + r.files_changed)
            .sum();

        let total_elapsed_ms: u64 = self.results.iter().map(|r| r.elapsed_ms as u64).sum();
        if total_elapsed_ms == 0 {
            return 0.0;
        }

        let minutes = total_elapsed_ms as f64 / 60_000.0;
        if minutes < 0.001 {
            return 0.0;
        }

        total_changes as f64 / minutes
    }

    /// Compute average budget usage ratio (elapsed / budget).
    pub fn avg_budget_usage(&self) -> f64 {
        if self.results.is_empty() {
            return 0.0;
        }

        let sum: f64 = self
            .results
            .iter()
            .filter(|r| r.budget_ms > 0)
            .map(|r| r.elapsed_ms as f64 / r.budget_ms as f64)
            .sum();

        let count = self.results.iter().filter(|r| r.budget_ms > 0).count();

        if count == 0 {
            return 0.0;
        }

        sum / count as f64
    }

    /// Count consecutive idle ticks (no changes) at the tail.
    pub fn idle_ticks(&self) -> usize {
        self.results
            .iter()
            .rev()
            .take_while(|r| {
                r.git_commits_found == 0 && r.files_changed == 0 && r.nodes_processed == 0
            })
            .count()
    }

    /// Number of recorded results.
    pub fn len(&self) -> usize {
        self.results.len()
    }

    /// Whether the history is empty.
    pub fn is_empty(&self) -> bool {
        self.results.is_empty()
    }

    /// All recorded tick results.
    pub fn all(&self) -> &VecDeque<CognitiveTickResult> {
        &self.results
    }
}

// ---------------------------------------------------------------------------
// WeaverEngine
// ---------------------------------------------------------------------------

/// ECC-powered codebase modeling service.
///
/// Manages modeling sessions, drives confidence evaluation via the
/// causal graph, and records modeling decisions in the Meta-Loom.
pub struct WeaverEngine {
    /// Active modeling sessions keyed by domain.
    sessions: RwLock<HashMap<String, ModelingSession>>,
    /// Cross-domain knowledge base.
    knowledge_base: Arc<WeaverKnowledgeBase>,
    /// Embedding provider for vectorization.
    embedding_provider: Arc<dyn EmbeddingProvider>,
    /// Causal graph reference.
    causal_graph: Arc<CausalGraph>,
    /// HNSW service reference.
    #[allow(dead_code)]
    hnsw: Arc<HnswService>,
    /// Impulse queue for emitting meta-loom signals.
    impulse_queue: Option<Arc<ImpulseQueue>>,
    /// Meta-loom event history per domain.
    meta_loom_events: RwLock<HashMap<String, Vec<MetaLoomEvent>>>,
    /// Total ticks processed across all sessions.
    tick_count: AtomicU64,
    /// Git poller for incremental commit detection.
    git_poller: Option<GitPoller>,
    /// File watcher for source file change detection.
    file_watcher: Option<FileWatcher>,
    /// Ticks since the last confidence recomputation.
    ticks_since_confidence_update: u64,
    /// Last computed confidence report (cached).
    last_confidence: Option<ConfidenceReport>,
    /// Confidence history ring buffer (Item 1).
    confidence_history: ConfidenceHistory,
    /// Strategy effectiveness tracker (Item 4).
    strategy_tracker: StrategyTracker,
    /// Tick result history for interval recommendation (Item 6).
    tick_history: TickHistory,
    /// Current tick interval in milliseconds (Item 6).
    current_tick_interval_ms: u32,
    /// Optional learned tick-interval recommender (Finding #7). When
    /// `None` or untrained, [`Self::recommend_tick_interval`] uses
    /// the original four-tier step-function. When trained, the
    /// model's prediction overrides the step-function's choice.
    tick_interval_model: Option<crate::eml_kernel::TickIntervalModel>,
}

impl WeaverEngine {
    /// Create a new WeaverEngine with the given dependencies.
    pub fn new(
        causal_graph: Arc<CausalGraph>,
        hnsw: Arc<HnswService>,
        embedding_provider: Arc<dyn EmbeddingProvider>,
    ) -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            knowledge_base: Arc::new(WeaverKnowledgeBase::new()),
            embedding_provider,
            causal_graph,
            hnsw,
            impulse_queue: None,
            meta_loom_events: RwLock::new(HashMap::new()),
            tick_count: AtomicU64::new(0),
            git_poller: None,
            file_watcher: None,
            ticks_since_confidence_update: 0,
            last_confidence: None,
            confidence_history: ConfidenceHistory::new(500),
            strategy_tracker: StrategyTracker::new(200),
            tick_history: TickHistory::new(500),
            current_tick_interval_ms: 1000,
            tick_interval_model: None,
        }
    }

    /// Install a learned
    /// [`TickIntervalModel`](crate::eml_kernel::TickIntervalModel)
    /// (Finding #7). With a model installed,
    /// [`Self::recommend_tick_interval`] consults the model after
    /// computing its hardcoded step-function choice; the model
    /// overrides only when trained, so the fallback is bit-for-bit
    /// identical to today's behaviour.
    ///
    /// NOTE(eml-swap): wired — Finding #7 (TickIntervalModel).
    pub fn set_tick_interval_model(&mut self, model: crate::eml_kernel::TickIntervalModel) {
        self.tick_interval_model = Some(model);
    }

    /// Returns a reference to the optional tick-interval model.
    pub fn tick_interval_model(&self) -> Option<&crate::eml_kernel::TickIntervalModel> {
        self.tick_interval_model.as_ref()
    }

    /// Create a WeaverEngine with a mock embedding provider (for tests).
    pub fn new_with_mock(causal_graph: Arc<CausalGraph>, hnsw: Arc<HnswService>) -> Self {
        Self::new(causal_graph, hnsw, Arc::new(MockEmbeddingProvider::new(64)))
    }

    /// Set the impulse queue for emitting meta-loom signals.
    pub fn set_impulse_queue(&mut self, queue: Arc<ImpulseQueue>) {
        self.impulse_queue = Some(queue);
    }

    /// Get a reference to the knowledge base.
    pub fn knowledge_base(&self) -> &Arc<WeaverKnowledgeBase> {
        &self.knowledge_base
    }

    /// Get a reference to the embedding provider.
    pub fn embedding_provider(&self) -> &Arc<dyn EmbeddingProvider> {
        &self.embedding_provider
    }

    /// Get a reference to the causal graph.
    pub fn causal_graph(&self) -> &Arc<CausalGraph> {
        &self.causal_graph
    }

    /// Get a reference to the HNSW service.
    pub fn hnsw(&self) -> &Arc<HnswService> {
        &self.hnsw
    }

    // ── Graph file ingestion ──────────────────────────────────────

    /// Ingest a graph JSON file (git-history, module-deps, or decisions).
    ///
    /// Reads a `.weftos/graph/*.json` file, creates causal graph nodes for
    /// each entry, creates edges between related nodes, and inserts
    /// embeddings into the HNSW index for each node's text representation.
    pub fn ingest_graph_file(&self, path: &Path) -> Result<IngestResult, WeaverError> {
        let data = std::fs::read_to_string(path)?;
        let graph: serde_json::Value = serde_json::from_str(&data)?;

        let source = graph["source"].as_str().unwrap_or("unknown").to_string();

        let empty_vec = vec![];
        let nodes = graph["nodes"].as_array().unwrap_or(&empty_vec);
        let edges = graph["edges"].as_array().unwrap_or(&empty_vec);

        let mut nodes_added = 0usize;
        let mut edges_added = 0usize;
        let mut embeddings_created = 0usize;

        // Map from JSON node id (string) to causal graph NodeId.
        let mut id_map: HashMap<String, u64> = HashMap::with_capacity(nodes.len());

        // Phase 1: Create nodes.
        for node in nodes {
            let node_id_str = node["id"].as_str().unwrap_or("").to_string();
            if node_id_str.is_empty() {
                continue;
            }

            let label = if let Some(title) = node["title"].as_str() {
                format!("{source}/{node_id_str}: {title}")
            } else if let Some(subject) = node["subject"].as_str() {
                format!("{source}/{node_id_str}: {subject}")
            } else {
                format!("{source}/{node_id_str}")
            };

            let causal_id = self.causal_graph.add_node(label.clone(), node.clone());
            id_map.insert(node_id_str.clone(), causal_id);
            nodes_added += 1;

            // Create an HNSW embedding for the node's text.
            let embed_text = Self::node_to_embed_text(node, &source);
            if !embed_text.is_empty() {
                // Use synchronous hash-embed for ingestion (avoiding async).
                let embed_vec = self.sync_embed(&embed_text);
                self.hnsw
                    .insert(format!("{source}/{}", node_id_str), embed_vec, node.clone());
                embeddings_created += 1;
            }
        }

        // Phase 2: Create edges.
        for edge in edges {
            let from_str = edge["from"].as_str().unwrap_or("");
            let to_str = edge["to"].as_str().unwrap_or("");
            let edge_type_str = edge["type"].as_str().unwrap_or("Correlates");
            let weight = edge["weight"].as_f64().unwrap_or(1.0) as f32;

            let from_id = id_map.get(from_str).copied();
            let to_id = id_map.get(to_str).copied();

            if let (Some(src), Some(tgt)) = (from_id, to_id) {
                let edge_type = Self::parse_edge_type(edge_type_str);
                let linked = self.causal_graph.link(src, tgt, edge_type, weight, 0, 0);
                if linked {
                    edges_added += 1;
                }
            }
        }

        info!(
            source = %source,
            nodes_added,
            edges_added,
            embeddings_created,
            "graph file ingested"
        );

        Ok(IngestResult {
            nodes_added,
            edges_added,
            embeddings_created,
            source,
        })
    }

    /// Ingest a graph file with strategy tracking and confidence history.
    ///
    /// Wraps [`ingest_graph_file`] with before/after confidence measurement,
    /// recording the result in the [`StrategyTracker`] and
    /// [`ConfidenceHistory`].
    pub fn ingest_graph_file_tracked(&mut self, path: &Path) -> Result<IngestResult, WeaverError> {
        let confidence_before = self.compute_confidence().overall;
        let source_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        let handle = self.strategy_tracker.begin_strategy(
            &format!("ingest:{source_name}"),
            &format!("Ingesting graph file {}", path.display()),
            confidence_before,
        );

        let result = self.ingest_graph_file(path)?;

        let report = self.compute_confidence();
        let confidence_after = report.overall;

        // Record strategy outcome (Item 4).
        self.strategy_tracker
            .complete_strategy(handle, confidence_after);

        // Record post-ingestion confidence snapshot (Item 1).
        let snapshot = ConfidenceSnapshot {
            timestamp: Utc::now(),
            tick_number: self.tick_count.load(Ordering::Relaxed),
            confidence: confidence_after,
            node_count: self.causal_graph.node_count() as usize,
            edge_count: self.causal_graph.edge_count() as usize,
            gap_count: report.gaps.len(),
            trigger: ConfidenceTrigger::PostIngestion,
        };
        self.confidence_history.record(snapshot);

        // Also record a StrategyChange snapshot if confidence changed.
        if (confidence_after - confidence_before).abs() > 0.001 {
            let change_snapshot = ConfidenceSnapshot {
                timestamp: Utc::now(),
                tick_number: self.tick_count.load(Ordering::Relaxed),
                confidence: confidence_after,
                node_count: self.causal_graph.node_count() as usize,
                edge_count: self.causal_graph.edge_count() as usize,
                gap_count: report.gaps.len(),
                trigger: ConfidenceTrigger::StrategyChange,
            };
            self.confidence_history.record(change_snapshot);
        }

        Ok(result)
    }

    /// Convert a graph node's fields into embeddable text.
    fn node_to_embed_text(node: &serde_json::Value, source: &str) -> String {
        match source {
            "git-history" => {
                let subject = node["subject"].as_str().unwrap_or("");
                let author = node["author"].as_str().unwrap_or("");
                let files = node["files"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                format!("commit by {author}: {subject} files: {files}")
            }
            "module-dependencies" => {
                let id = node["id"].as_str().unwrap_or("");
                let deps = node["dependencies"]
                    .as_array()
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let lines = node["lines"].as_u64().unwrap_or(0);
                format!("module {id} ({lines} lines) depends on: {deps}")
            }
            "decisions-and-phases" => {
                let title = node["title"].as_str().unwrap_or("");
                let rationale = node["rationale"].as_str().unwrap_or("");
                let panel = node["panel"].as_str().unwrap_or("");
                format!("decision ({panel}): {title} — {rationale}")
            }
            _ => {
                // Fallback: serialize the whole node.
                serde_json::to_string(node).unwrap_or_default()
            }
        }
    }

    /// Parse an edge type string to a CausalEdgeType.
    fn parse_edge_type(s: &str) -> CausalEdgeType {
        match s {
            "Causes" => CausalEdgeType::Causes,
            "Inhibits" => CausalEdgeType::Inhibits,
            "Correlates" => CausalEdgeType::Correlates,
            "Enables" => CausalEdgeType::Enables,
            "Follows" => CausalEdgeType::Follows,
            "Contradicts" => CausalEdgeType::Contradicts,
            "TriggeredBy" => CausalEdgeType::TriggeredBy,
            "EvidenceFor" => CausalEdgeType::EvidenceFor,
            _ => CausalEdgeType::Correlates,
        }
    }

    /// Synchronous embedding using the mock fallback (for ingestion loops).
    ///
    /// This avoids the need for async in the ingestion path. The mock
    /// provider's `hash_embed` is deterministic and instant.
    fn sync_embed(&self, text: &str) -> Vec<f32> {
        use sha2::{Digest, Sha256};
        let dims = self.embedding_provider.dimensions();
        let mut hasher = Sha256::new();
        hasher.update(text.as_bytes());
        let hash = hasher.finalize();
        let mut vec = Vec::with_capacity(dims);
        for i in 0..dims {
            let byte = hash[i % 32];
            vec.push((byte as f32 / 128.0) - 1.0);
        }
        vec
    }

    // ── Confidence scoring from graph data ────────────────────────

    /// Compute confidence based on graph coverage.
    ///
    /// Examines the causal graph to determine what fraction of nodes have
    /// edges (both incoming and outgoing), the edge density, and identifies
    /// orphan nodes that lack causal connections.
    pub fn compute_confidence(&self) -> ConfidenceReport {
        let node_count = self.causal_graph.node_count() as usize;
        let edge_count = self.causal_graph.edge_count() as usize;

        if node_count == 0 {
            return ConfidenceReport {
                overall: 0.0,
                gaps: vec![ConfidenceGap {
                    domain: "graph".to_string(),
                    current_confidence: 0.0,
                    target_confidence: 0.8,
                    suggested_sources: vec![
                        "git_log".into(),
                        "module_deps".into(),
                        "decisions".into(),
                    ],
                }],
                suggestions: vec![ModelingSuggestion::AddSource {
                    source_type: "git_log".to_string(),
                    reason: "No graph data ingested yet".to_string(),
                }],
            };
        }

        // Edge density: ratio of actual edges to maximum possible.
        let max_edges = if node_count > 1 {
            node_count * (node_count - 1)
        } else {
            1
        };
        let edge_density = (edge_count as f64 / max_edges as f64).min(1.0);

        // Node connectivity: fraction of nodes that have at least one edge.
        // We sample by checking forward + reverse edges for each node id up to
        // the known count (sequential IDs starting from 1).
        let mut connected_nodes = 0usize;
        let mut orphan_labels: Vec<String> = Vec::new();
        let next_id = self.causal_graph.node_count() + 1;
        // Iterate over plausible node IDs. The CausalGraph allocates IDs
        // sequentially starting at 1 so scanning 1..next_id covers all.
        for nid in 1..next_id {
            if self.causal_graph.get_node(nid).is_some() {
                let fwd = self.causal_graph.get_forward_edges(nid);
                let rev = self.causal_graph.get_reverse_edges(nid);
                if !fwd.is_empty() || !rev.is_empty() {
                    connected_nodes += 1;
                } else if let Some(node) = self.causal_graph.get_node(nid) {
                    orphan_labels.push(node.label.clone());
                }
            }
        }

        let connectivity = if node_count > 0 {
            connected_nodes as f64 / node_count as f64
        } else {
            0.0
        };

        // Composite confidence: weighted average of components.
        // - Connectivity (40%): nodes with edges / total nodes
        // - Edge density (20%): capped contribution from edge density
        // - Node volume (20%): diminishing returns above 100 nodes
        // - Source diversity (20%): number of distinct source prefixes
        let volume_score = (node_count as f64 / 100.0).min(1.0);
        let density_capped = (edge_density * 50.0).min(1.0); // amplify sparse graphs

        let overall = (connectivity * 0.40
            + density_capped * 0.20
            + volume_score * 0.20
            + self.source_diversity_score() * 0.20)
            .min(1.0);

        // Build gaps.
        let mut gaps = Vec::new();
        if connectivity < 0.7 {
            gaps.push(ConfidenceGap {
                domain: "node_connectivity".to_string(),
                current_confidence: connectivity,
                target_confidence: 0.7,
                suggested_sources: vec!["module_deps".into(), "git_log".into()],
            });
        }
        if volume_score < 0.5 {
            gaps.push(ConfidenceGap {
                domain: "data_volume".to_string(),
                current_confidence: volume_score,
                target_confidence: 0.5,
                suggested_sources: vec!["git_log".into(), "file_tree".into()],
            });
        }

        // Suggestions from orphan nodes.
        let mut suggestions = Vec::new();
        if !orphan_labels.is_empty() {
            let sample: Vec<_> = orphan_labels.iter().take(5).cloned().collect();
            suggestions.push(ModelingSuggestion::AddSource {
                source_type: "causal_edges".to_string(),
                reason: format!(
                    "{} orphan nodes without edges (e.g., {})",
                    orphan_labels.len(),
                    sample.join(", ")
                ),
            });
        }

        ConfidenceReport {
            overall,
            gaps,
            suggestions,
        }
    }

    /// Count distinct source prefixes in node labels to gauge diversity.
    fn source_diversity_score(&self) -> f64 {
        let sessions = match self.sessions.read() {
            Ok(s) => s,
            Err(_) => return 0.0,
        };
        let total_sources: usize = sessions.values().map(|s| s.sources_ingested.len()).sum();
        // Diminishing returns: 3 sources = 1.0.
        (total_sources as f64 / 3.0).min(1.0)
    }

    // ── Model export to file ──────────────────────────────────────

    /// Export the current model state to a JSON file at the given path.
    ///
    /// Produces a `weave-model.json` that includes the causal graph nodes,
    /// edges, confidence report, and metadata.
    pub fn export_model_to_file(
        &self,
        domain: &str,
        min_confidence: f64,
        path: &Path,
    ) -> Result<ExportedModel, WeaverError> {
        let model = self
            .export_model(domain, min_confidence)
            .map_err(WeaverError::Domain)?;
        let json = serde_json::to_string_pretty(&model)?;
        std::fs::write(path, json)?;
        info!(domain, ?path, "model exported to file");
        Ok(model)
    }

    /// Import a model from a JSON file.
    pub fn import_model_from_file(&self, domain: &str, path: &Path) -> Result<(), WeaverError> {
        let data = std::fs::read_to_string(path)?;
        let model: ExportedModel = serde_json::from_str(&data)?;
        self.import_model(domain, model)
            .map_err(WeaverError::Domain)?;
        info!(domain, ?path, "model imported from file");
        Ok(())
    }

    // ── Session management ────────────────────────────────────────

    /// Start a new modeling session.
    pub fn start_session(
        &self,
        domain: &str,
        context: Option<&str>,
        _goal: Option<&str>,
    ) -> Result<String, String> {
        let session_id = uuid::Uuid::new_v4().to_string();
        let session = ModelingSession {
            id: session_id.clone(),
            domain: domain.to_string(),
            started_at: Utc::now(),
            confidence: 0.0,
            gaps: Vec::new(),
            sources_ingested: Vec::new(),
            tick_count: 0,
            budget_remaining_ms: 300_000, // 5 min default
            active: true,
            metadata: {
                let cap = if context.is_some() { 1 } else { 0 };
                let mut m = HashMap::with_capacity(cap);
                if let Some(ctx) = context {
                    m.insert(
                        "context".to_string(),
                        serde_json::Value::String(ctx.to_string()),
                    );
                }
                m
            },
        };

        let mut sessions = self.sessions.write().map_err(|e| e.to_string())?;
        if sessions.contains_key(domain) {
            return Err(format!("session already exists for domain: {domain}"));
        }
        sessions.insert(domain.to_string(), session);

        // Record meta-loom event.
        self.record_meta_loom(
            domain,
            MetaDecisionType::ModelVersionBumped { from: 0, to: 1 },
            "Session initialized",
            0.0,
        );

        // Emit impulse.
        self.emit_impulse(ImpulseType::Custom(0x32));

        info!(domain, session_id = %session_id, "weaver session started");
        Ok(session_id)
    }

    /// Stop a modeling session.
    pub fn stop_session(&self, domain: &str) -> Result<(), String> {
        let mut sessions = self.sessions.write().map_err(|e| e.to_string())?;
        let session = sessions
            .get_mut(domain)
            .ok_or_else(|| format!("no session for domain: {domain}"))?;
        session.active = false;
        info!(domain, "weaver session stopped");
        Ok(())
    }

    /// Resume a stopped session.
    pub fn resume_session(&self, domain: &str) -> Result<(), String> {
        let mut sessions = self.sessions.write().map_err(|e| e.to_string())?;
        let session = sessions
            .get_mut(domain)
            .ok_or_else(|| format!("no session for domain: {domain}"))?;
        session.active = true;
        info!(domain, "weaver session resumed");
        Ok(())
    }

    /// Get a snapshot of a session.
    pub fn get_session(&self, domain: &str) -> Option<ModelingSession> {
        self.sessions.read().ok()?.get(domain).cloned()
    }

    /// List all session domains.
    pub fn list_sessions(&self) -> Vec<String> {
        self.sessions
            .read()
            .map(|s| s.keys().cloned().collect())
            .unwrap_or_default()
    }

    // ── Source management ─────────────────────────────────────────

    /// Add a data source to a session.
    pub fn add_source(
        &self,
        domain: &str,
        source_type: &str,
        _root: Option<&PathBuf>,
    ) -> Result<(), String> {
        let mut sessions = self.sessions.write().map_err(|e| e.to_string())?;
        let session = sessions
            .get_mut(domain)
            .ok_or_else(|| format!("no session for domain: {domain}"))?;
        session.sources_ingested.push(source_type.to_string());

        // Record meta-loom event.
        drop(sessions); // release lock before recording
        self.record_meta_loom(
            domain,
            MetaDecisionType::SourceAdded {
                source_type: source_type.to_string(),
            },
            &format!("Added source: {source_type}"),
            self.get_session(domain)
                .map(|s| s.confidence)
                .unwrap_or(0.0),
        );

        // Emit impulse for source request.
        self.emit_impulse(ImpulseType::Custom(0x33));

        Ok(())
    }

    // ── Confidence evaluation ─────────────────────────────────────

    /// Evaluate confidence for a session domain.
    pub fn evaluate_confidence(&self, domain: &str) -> Result<ConfidenceReport, String> {
        let sessions = self.sessions.read().map_err(|e| e.to_string())?;
        let session = sessions
            .get(domain)
            .ok_or_else(|| format!("no session for domain: {domain}"))?;

        // Simple confidence model based on source count and graph size.
        let source_count = session.sources_ingested.len() as f64;
        let node_count = self.causal_graph.node_count() as f64;

        // Confidence grows with data, capped at 1.0.
        let base_confidence = (source_count * 0.15 + node_count * 0.01).min(1.0);

        let mut gaps = Vec::new();
        if source_count < 3.0 {
            gaps.push(ConfidenceGap {
                domain: domain.to_string(),
                current_confidence: base_confidence,
                target_confidence: 0.8,
                suggested_sources: vec!["git_log".into(), "file_tree".into()],
            });
        }

        let suggestions = if source_count < 2.0 {
            vec![ModelingSuggestion::AddSource {
                source_type: "git_log".into(),
                reason: "No git history ingested yet".into(),
            }]
        } else {
            Vec::new()
        };

        Ok(ConfidenceReport {
            overall: base_confidence,
            gaps,
            suggestions,
        })
    }

    // ── Cognitive tick handler ─────────────────────────────────────

    /// Process a single cognitive tick.
    ///
    /// Called by the CognitiveTick service during each tick cycle.
    /// Budget-aware: yields if budget is exhausted.
    pub fn tick(&self, budget: Duration) -> TickResult {
        let budget_start = Instant::now();

        let mut sessions = match self.sessions.write() {
            Ok(s) => s,
            Err(_) => return TickResult::Idle,
        };

        // Find an active session.
        let active_domain = sessions
            .iter()
            .find(|(_, s)| s.active)
            .map(|(d, _)| d.clone());

        let domain = match active_domain {
            Some(d) => d,
            None => return TickResult::Idle,
        };

        let session = match sessions.get_mut(&domain) {
            Some(s) => s,
            None => return TickResult::Idle,
        };

        // Phase 1: Evaluate confidence.
        let source_count = session.sources_ingested.len() as f64;
        let node_count = self.causal_graph.node_count() as f64;
        let confidence = (source_count * 0.15 + node_count * 0.01).min(1.0);
        session.confidence = confidence;

        // Phase 2: Identify gaps.
        let mut gaps = Vec::new();
        if confidence < 0.8 {
            gaps.push(ConfidenceGap {
                domain: domain.clone(),
                current_confidence: confidence,
                target_confidence: 0.8,
                suggested_sources: vec!["git_log".into(), "file_tree".into()],
            });
        }
        session.gaps = gaps.clone();

        if budget_start.elapsed() > budget {
            return TickResult::BudgetExhausted;
        }

        // Phase 3: Create a causal node to record the tick.
        let tick_label = format!("weaver.tick.{}.{}", domain, session.tick_count);
        self.causal_graph.add_node(
            tick_label,
            serde_json::json!({
                "domain": domain,
                "confidence": confidence,
                "tick": session.tick_count,
            }),
        );

        session.tick_count += 1;
        self.tick_count.fetch_add(1, Ordering::Relaxed);

        TickResult::Progress {
            confidence,
            gaps_remaining: gaps.len(),
        }
    }

    // ── Export / Import (K3c-G4) ──────────────────────────────────

    /// Export the model for a domain.
    pub fn export_model(&self, domain: &str, min_confidence: f64) -> Result<ExportedModel, String> {
        let sessions = self.sessions.read().map_err(|e| e.to_string())?;
        let session = sessions
            .get(domain)
            .ok_or_else(|| format!("no session for domain: {domain}"))?;

        // Collect all edges and filter by confidence.
        let edge_types: Vec<EdgeTypeSpec> = session
            .sources_ingested
            .iter()
            .enumerate()
            .map(|(i, src)| EdgeTypeSpec {
                from_type: "source".into(),
                to_type: "domain".into(),
                edge_type: format!("ingested_{src}"),
                confidence: (i as f64 + 1.0) * 0.2,
            })
            .filter(|e| e.confidence >= min_confidence)
            .collect();

        // Collect causal nodes from the graph.
        let mut causal_nodes = Vec::new();
        let mut causal_edges_out = Vec::new();
        let next_id = self.causal_graph.node_count() + 1;
        for nid in 1..next_id {
            if let Some(node) = self.causal_graph.get_node(nid) {
                causal_nodes.push(ExportedCausalNode {
                    label: node.label.clone(),
                    metadata: node.metadata.clone(),
                });
                // Collect forward edges from this node.
                for edge in self.causal_graph.get_forward_edges(nid) {
                    if let Some(target_node) = self.causal_graph.get_node(edge.target) {
                        causal_edges_out.push(ExportedCausalEdge {
                            source_label: node.label.clone(),
                            target_label: target_node.label.clone(),
                            edge_type: format!("{}", edge.edge_type),
                            weight: edge.weight,
                        });
                    }
                }
            }
        }

        Ok(ExportedModel {
            version: "1.0".to_string(),
            domain: domain.to_string(),
            exported_at: Utc::now(),
            confidence: session.confidence,
            node_types: vec![NodeTypeSpec {
                name: "default".into(),
                embedding_strategy: self.embedding_provider.model_name().to_string(),
                dimensions: self.embedding_provider.dimensions(),
            }],
            edge_types,
            causal_nodes,
            causal_edges: causal_edges_out,
            metadata: session.metadata.clone(),
        })
    }

    /// Import a previously exported model.
    pub fn import_model(&self, domain: &str, model: ExportedModel) -> Result<(), String> {
        // Version check.
        if !model.version.starts_with("1.") {
            return Err(format!(
                "incompatible model version: expected 1.x, got {}",
                model.version
            ));
        }

        let session = ModelingSession {
            id: uuid::Uuid::new_v4().to_string(),
            domain: domain.to_string(),
            started_at: Utc::now(),
            confidence: model.confidence,
            gaps: Vec::new(),
            sources_ingested: model
                .edge_types
                .iter()
                .map(|e| e.edge_type.clone())
                .collect(),
            tick_count: 0,
            budget_remaining_ms: 300_000,
            active: true,
            metadata: model.metadata,
        };

        let mut sessions = self.sessions.write().map_err(|e| e.to_string())?;
        sessions.insert(domain.to_string(), session);

        // Record meta-loom.
        drop(sessions);
        self.record_meta_loom(
            domain,
            MetaDecisionType::ModelVersionBumped { from: 0, to: 1 },
            "Imported from exported model",
            model.confidence,
        );

        info!(domain, "weaver model imported");
        Ok(())
    }

    // ── Command handler (IPC) ─────────────────────────────────────

    /// Handle a WeaverCommand received via IPC.
    pub fn handle_command(&self, cmd: WeaverCommand) -> WeaverResponse {
        match cmd {
            WeaverCommand::SessionStart {
                domain,
                context,
                goal,
                ..
            } => match self.start_session(&domain, context.as_deref(), goal.as_deref()) {
                Ok(session_id) => WeaverResponse::SessionStarted { domain, session_id },
                Err(e) => WeaverResponse::Error(e),
            },
            WeaverCommand::SessionStop { domain } => match self.stop_session(&domain) {
                Ok(()) => WeaverResponse::SessionStopped { domain },
                Err(e) => WeaverResponse::Error(e),
            },
            WeaverCommand::SessionResume { domain } => match self.resume_session(&domain) {
                Ok(()) => WeaverResponse::SessionResumed { domain },
                Err(e) => WeaverResponse::Error(e),
            },
            WeaverCommand::SourceAdd {
                domain,
                source_type,
                root,
                ..
            } => match self.add_source(&domain, &source_type, root.as_ref()) {
                Ok(()) => WeaverResponse::SourceAdded {
                    domain,
                    source_type,
                },
                Err(e) => WeaverResponse::Error(e),
            },
            WeaverCommand::SourceList { domain } => match self.get_session(&domain) {
                Some(s) => WeaverResponse::Sources(s.sources_ingested),
                None => WeaverResponse::Error(format!("no session for domain: {domain}")),
            },
            WeaverCommand::Confidence { domain, .. } => match self.evaluate_confidence(&domain) {
                Ok(report) => WeaverResponse::ConfidenceReport(report),
                Err(e) => WeaverResponse::Error(e),
            },
            WeaverCommand::Export {
                domain,
                min_confidence,
                output,
            } => match self.export_model(&domain, min_confidence) {
                Ok(model) => {
                    let edges = model.edge_types.len();
                    // In a real implementation, this would write to disk.
                    debug!(?output, edges, "model exported");
                    WeaverResponse::Exported {
                        path: output,
                        edges,
                    }
                }
                Err(e) => WeaverResponse::Error(e),
            },
            WeaverCommand::Import { domain, input } => {
                // In a real implementation, this would read from disk.
                debug!(?input, "model import requested");
                WeaverResponse::Imported { domain }
            }
            WeaverCommand::MetaStrategies => {
                let strategies = self.knowledge_base.list_strategies();
                WeaverResponse::Strategies(strategies)
            }
            WeaverCommand::MetaExportKb { output } => {
                debug!(?output, "KB export requested");
                WeaverResponse::KbExported { path: output }
            }
            _ => WeaverResponse::Error("command not implemented".into()),
        }
    }

    // ── Meta-Loom (K3c-G5) ───────────────────────────────────────

    /// Record a meta-loom event.
    fn record_meta_loom(
        &self,
        domain: &str,
        decision: MetaDecisionType,
        rationale: &str,
        confidence_before: f64,
    ) {
        let event = MetaLoomEvent {
            session_domain: domain.to_string(),
            decision_type: decision,
            confidence_before,
            confidence_after: None,
            rationale: rationale.to_string(),
            timestamp: Utc::now(),
        };

        // Record in causal graph under meta-loom namespace.
        let label = format!("meta-loom/{}", domain);
        self.causal_graph
            .add_node(label, serde_json::to_value(&event).unwrap_or_default());

        // Store in local event history.
        if let Ok(mut events) = self.meta_loom_events.write() {
            events.entry(domain.to_string()).or_default().push(event);
        }
    }

    /// Get meta-loom events for a domain.
    pub fn meta_loom_events(&self, domain: &str) -> Vec<MetaLoomEvent> {
        self.meta_loom_events
            .read()
            .ok()
            .and_then(|m| m.get(domain).cloned())
            .unwrap_or_default()
    }

    /// Emit an impulse if the queue is configured.
    fn emit_impulse(&self, impulse_type: ImpulseType) {
        if let Some(queue) = &self.impulse_queue {
            queue.emit(
                0x03, // CausalGraph
                [0u8; 32],
                0x03, // self-referential
                impulse_type,
                serde_json::Value::Null,
                0,
            );
        }
    }

    // ── CognitiveTick integration ──────────────────────────────────

    /// Handle a cognitive tick — process pending work within budget.
    ///
    /// Called by the CognitiveTick service each cycle. Performs git polling,
    /// file change detection, pending ingestion, and periodic confidence
    /// recomputation, all within the supplied time budget.
    pub fn on_tick(&mut self, budget_ms: u32) -> CognitiveTickResult {
        let start = Instant::now();
        let budget = Duration::from_millis(budget_ms as u64);
        let mut result = CognitiveTickResult {
            tick_number: self.tick_count.load(Ordering::Relaxed),
            budget_ms,
            ..Default::default()
        };

        // 1. Check for new git commits (if git polling enabled).
        if start.elapsed() < budget
            && let Some(new_commits) = self.poll_git()
        {
            result.git_commits_found = new_commits;
        }

        // 2. Check for file changes (if file watcher enabled).
        if start.elapsed() < budget
            && let Some(changed_files) = self.poll_file_changes()
        {
            result.files_changed = changed_files;
        }

        // 3. Process pending ingestion queue (delegate to existing tick()).
        if start.elapsed() < budget {
            let remaining = budget.saturating_sub(start.elapsed());
            let tick_result = self.tick(remaining);
            if let TickResult::Progress { .. } = tick_result {
                result.nodes_processed = 1;
            }
        }

        // 4. Recompute confidence every 100 ticks and record snapshot.
        if start.elapsed() < budget && self.ticks_since_confidence_update > 100 {
            let report = self.compute_confidence();
            self.last_confidence = Some(report.clone());
            self.ticks_since_confidence_update = 0;
            result.confidence_updated = true;

            // Record confidence snapshot (Item 1).
            let snapshot = ConfidenceSnapshot {
                timestamp: Utc::now(),
                tick_number: self.tick_count.load(Ordering::Relaxed),
                confidence: report.overall,
                node_count: self.causal_graph.node_count() as usize,
                edge_count: self.causal_graph.edge_count() as usize,
                gap_count: report.gaps.len(),
                trigger: ConfidenceTrigger::Periodic,
            };
            self.confidence_history.record(snapshot);
        }

        result.elapsed_ms = start.elapsed().as_millis() as u32;
        result.within_budget = start.elapsed() <= budget;
        self.tick_count.fetch_add(1, Ordering::Relaxed);
        self.ticks_since_confidence_update += 1;

        // Record tick result in history (Item 6).
        self.tick_history.record(result.clone());

        result
    }

    /// Enable git polling for a repository path and branch.
    pub fn enable_git_polling(&mut self, repo_path: PathBuf, branch: String) {
        self.git_poller = Some(GitPoller::new(repo_path, branch));
    }

    /// Enable file watching for source files under a root directory.
    pub fn enable_file_watching(&mut self, root: PathBuf, patterns: Vec<String>) {
        let mut watcher = FileWatcher::new(root, patterns);
        watcher.watch_directory();
        self.file_watcher = Some(watcher);
    }

    /// Poll git for new commits (internal helper for on_tick).
    fn poll_git(&mut self) -> Option<usize> {
        self.git_poller.as_mut().map(|p| p.poll())
    }

    /// Poll file watcher for changed files (internal helper for on_tick).
    fn poll_file_changes(&mut self) -> Option<usize> {
        self.file_watcher.as_mut().map(|w| w.poll_changes().len())
    }

    /// Get the cached confidence report from the last recomputation.
    pub fn cached_confidence(&self) -> Option<&ConfidenceReport> {
        self.last_confidence.as_ref()
    }

    /// Get a reference to the git poller, if enabled.
    pub fn git_poller(&self) -> Option<&GitPoller> {
        self.git_poller.as_ref()
    }

    /// Get a reference to the file watcher, if enabled.
    pub fn file_watcher(&self) -> Option<&FileWatcher> {
        self.file_watcher.as_ref()
    }

    /// Total ticks processed.
    pub fn total_ticks(&self) -> u64 {
        self.tick_count.load(Ordering::Relaxed)
    }

    // ── Confidence history accessors (Item 1) ────────────────────

    /// Get a reference to the confidence history.
    pub fn confidence_history(&self) -> &ConfidenceHistory {
        &self.confidence_history
    }

    /// Get a mutable reference to the confidence history.
    pub fn confidence_history_mut(&mut self) -> &mut ConfidenceHistory {
        &mut self.confidence_history
    }

    // ── Strategy tracker accessors (Item 4) ──────────────────────

    /// Get a reference to the strategy tracker.
    pub fn strategy_tracker(&self) -> &StrategyTracker {
        &self.strategy_tracker
    }

    /// Get a mutable reference to the strategy tracker.
    pub fn strategy_tracker_mut(&mut self) -> &mut StrategyTracker {
        &mut self.strategy_tracker
    }

    // ── Tick history / interval recommendation (Item 6) ──────────

    /// Get a reference to the tick history.
    pub fn tick_history(&self) -> &TickHistory {
        &self.tick_history
    }

    /// Set the current tick interval (for recommendation calculations).
    pub fn set_tick_interval_ms(&mut self, ms: u32) {
        self.current_tick_interval_ms = ms;
    }

    /// Analyze recent tick history and recommend interval adjustment.
    ///
    /// The hardcoded step-function (preserved as the untrained
    /// fallback) maps change-rate to interval as:
    /// - Frequent changes (>10/min): 200ms (fast).
    /// - Moderate changes (1-10/min): 1000ms (default).
    /// - Rare changes (<1/min): 3000ms (slow).
    /// - No changes for 100+ ticks: 5000ms (idle mode).
    ///
    /// NOTE(eml-swap): wired — Finding #7 (TickIntervalModel). When a
    /// trained tick-interval model is installed via
    /// [`Self::set_tick_interval_model`], its `(cpm, idle_ticks,
    /// variance) -> recommended_ms` prediction overrides the step
    /// function. Untrained models leave the step function untouched
    /// so the fallback path is bit-for-bit identical.
    pub fn recommend_tick_interval(&self) -> TickRecommendation {
        let idle = self.tick_history.idle_ticks();
        let cpm = self.tick_history.changes_per_minute();
        let sample_count = self.tick_history.len();

        // Step function — same logic as before, lifted into a small
        // helper so the EML override has a clean fallback to consult.
        let step = self.step_recommend_tick_interval(idle, cpm, sample_count);

        let Some(model) = self.tick_interval_model.as_ref() else {
            return step;
        };
        if !model.is_trained() {
            return step;
        }

        // EML override — variance is the squared deviation of cpm
        // from the moderate-tier midpoint (5/min), normalised by 100.
        let variance = ((cpm - 5.0) * (cpm - 5.0) / 100.0).clamp(0.0, 1.0);
        let predicted_ms = model.recommend_or(cpm, idle as u64, variance, step.recommended_ms);
        TickRecommendation {
            recommended_ms: predicted_ms,
            current_ms: step.current_ms,
            reason: format!(
                "EML tick-interval model (cpm={cpm:.2}, idle={idle}, var={variance:.3}); fallback was {fallback}ms",
                fallback = step.recommended_ms
            ),
            changes_per_minute: cpm,
            // Confidence borrowed from the step-function — the model
            // is advisory and shares the same observability gate.
            recommendation_confidence: step.recommendation_confidence,
        }
    }

    /// Hardcoded step-function recommender — preserved verbatim from
    /// the pre-EML implementation. Lifted into its own method so the
    /// EML override in [`Self::recommend_tick_interval`] can reuse it
    /// as the untrained fallback.
    fn step_recommend_tick_interval(
        &self,
        idle: usize,
        cpm: f64,
        sample_count: usize,
    ) -> TickRecommendation {
        // Not enough data to make a confident recommendation.
        if sample_count < 5 {
            return TickRecommendation {
                recommended_ms: self.current_tick_interval_ms,
                current_ms: self.current_tick_interval_ms,
                reason: "Insufficient data for recommendation".to_string(),
                changes_per_minute: cpm,
                recommendation_confidence: 0.1,
            };
        }

        // Idle mode: no changes for many consecutive ticks.
        if idle >= 100 {
            return TickRecommendation {
                recommended_ms: 5000,
                current_ms: self.current_tick_interval_ms,
                reason: format!("No changes for {idle} consecutive ticks; entering idle mode"),
                changes_per_minute: cpm,
                recommendation_confidence: 0.9,
            };
        }

        // High frequency changes: speed up.
        if cpm > 10.0 {
            return TickRecommendation {
                recommended_ms: 200,
                current_ms: self.current_tick_interval_ms,
                reason: format!("High change rate ({cpm:.1}/min); recommending fast ticks"),
                changes_per_minute: cpm,
                recommendation_confidence: 0.8,
            };
        }

        // Moderate frequency: default speed.
        if cpm >= 1.0 {
            return TickRecommendation {
                recommended_ms: 1000,
                current_ms: self.current_tick_interval_ms,
                reason: format!("Moderate change rate ({cpm:.1}/min); default interval"),
                changes_per_minute: cpm,
                recommendation_confidence: 0.7,
            };
        }

        // Low frequency: slow down.
        TickRecommendation {
            recommended_ms: 3000,
            current_ms: self.current_tick_interval_ms,
            reason: format!("Low change rate ({cpm:.2}/min); recommending slower ticks"),
            changes_per_minute: cpm,
            recommendation_confidence: 0.6,
        }
    }
}

#[async_trait]
impl SystemService for WeaverEngine {
    fn name(&self) -> &str {
        "weaver"
    }

    fn service_type(&self) -> ServiceType {
        ServiceType::Core
    }

    async fn start(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        info!("weaver engine started");
        Ok(())
    }

    async fn stop(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        // Stop all active sessions.
        if let Ok(mut sessions) = self.sessions.write() {
            for (_, session) in sessions.iter_mut() {
                session.active = false;
            }
        }
        info!(
            total_ticks = self.total_ticks(),
            kb_strategies = self.knowledge_base.count(),
            "weaver engine stopped"
        );
        Ok(())
    }

    async fn health_check(&self) -> HealthStatus {
        // Always healthy: both active and idle states are normal.
        HealthStatus::Healthy
    }
}

// ---------------------------------------------------------------------------
// ModelDiff (K3c-G4b)
// ---------------------------------------------------------------------------

/// Differences between two exported models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDiff {
    /// Model A identifier (domain).
    pub model_a: String,
    /// Model B identifier (domain).
    pub model_b: String,
    /// Confidence delta (B - A).
    pub confidence_delta: f64,
    /// Node types only in A.
    pub nodes_only_a: Vec<String>,
    /// Node types only in B.
    pub nodes_only_b: Vec<String>,
    /// Node types in both.
    pub nodes_common: Vec<String>,
    /// Edge types only in A.
    pub edges_only_a: Vec<String>,
    /// Edge types only in B.
    pub edges_only_b: Vec<String>,
    /// Edge types in both.
    pub edges_common: Vec<String>,
    /// Causal nodes added in B vs A.
    pub causal_nodes_added: usize,
    /// Causal nodes removed in B vs A.
    pub causal_nodes_removed: usize,
    /// Causal edges added.
    pub causal_edges_added: usize,
    /// Causal edges removed.
    pub causal_edges_removed: usize,
    /// Summary assessment.
    pub summary: String,
}

/// Compare two exported models and produce a structured diff.
pub fn diff_models(a: &ExportedModel, b: &ExportedModel) -> ModelDiff {
    // Node types by name.
    let a_node_names: HashSet<&str> = a.node_types.iter().map(|n| n.name.as_str()).collect();
    let b_node_names: HashSet<&str> = b.node_types.iter().map(|n| n.name.as_str()).collect();

    let nodes_only_a: Vec<String> = a_node_names
        .difference(&b_node_names)
        .map(|s| s.to_string())
        .collect();
    let nodes_only_b: Vec<String> = b_node_names
        .difference(&a_node_names)
        .map(|s| s.to_string())
        .collect();
    let nodes_common: Vec<String> = a_node_names
        .intersection(&b_node_names)
        .map(|s| s.to_string())
        .collect();

    // Edge types by (from, to, type) composite key.
    let edge_key = |e: &EdgeTypeSpec| format!("{}->{}:{}", e.from_type, e.to_type, e.edge_type);
    let a_edge_keys: HashSet<String> = a.edge_types.iter().map(&edge_key).collect();
    let b_edge_keys: HashSet<String> = b.edge_types.iter().map(edge_key).collect();

    let edges_only_a: Vec<String> = a_edge_keys.difference(&b_edge_keys).cloned().collect();
    let edges_only_b: Vec<String> = b_edge_keys.difference(&a_edge_keys).cloned().collect();
    let edges_common: Vec<String> = a_edge_keys.intersection(&b_edge_keys).cloned().collect();

    // Causal nodes by label.
    let a_causal_labels: HashSet<&str> = a.causal_nodes.iter().map(|n| n.label.as_str()).collect();
    let b_causal_labels: HashSet<&str> = b.causal_nodes.iter().map(|n| n.label.as_str()).collect();

    let causal_nodes_added = b_causal_labels.difference(&a_causal_labels).count();
    let causal_nodes_removed = a_causal_labels.difference(&b_causal_labels).count();

    // Causal edges by (from, to).
    let causal_edge_key =
        |e: &ExportedCausalEdge| format!("{}->{}", e.source_label, e.target_label);
    let a_causal_edge_keys: HashSet<String> = a.causal_edges.iter().map(&causal_edge_key).collect();
    let b_causal_edge_keys: HashSet<String> = b.causal_edges.iter().map(causal_edge_key).collect();

    let causal_edges_added = b_causal_edge_keys.difference(&a_causal_edge_keys).count();
    let causal_edges_removed = a_causal_edge_keys.difference(&b_causal_edge_keys).count();

    let confidence_delta = b.confidence - a.confidence;

    // Build summary.
    let mut parts = Vec::new();
    if confidence_delta.abs() > f64::EPSILON {
        parts.push(format!(
            "confidence {} by {:.3}",
            if confidence_delta > 0.0 {
                "increased"
            } else {
                "decreased"
            },
            confidence_delta.abs()
        ));
    }
    if causal_nodes_added > 0 || causal_nodes_removed > 0 {
        parts.push(format!(
            "{} causal nodes added, {} removed",
            causal_nodes_added, causal_nodes_removed
        ));
    }
    if causal_edges_added > 0 || causal_edges_removed > 0 {
        parts.push(format!(
            "{} causal edges added, {} removed",
            causal_edges_added, causal_edges_removed
        ));
    }
    if !nodes_only_a.is_empty() || !nodes_only_b.is_empty() {
        parts.push(format!(
            "{} node types only in A, {} only in B",
            nodes_only_a.len(),
            nodes_only_b.len()
        ));
    }
    let summary = if parts.is_empty() {
        "models are identical".to_string()
    } else {
        parts.join("; ")
    };

    ModelDiff {
        model_a: a.domain.clone(),
        model_b: b.domain.clone(),
        confidence_delta,
        nodes_only_a,
        nodes_only_b,
        nodes_common,
        edges_only_a,
        edges_only_b,
        edges_common,
        causal_nodes_added,
        causal_nodes_removed,
        causal_edges_added,
        causal_edges_removed,
        summary,
    }
}

// ---------------------------------------------------------------------------
// ModelMerge (K3c-G4c)
// ---------------------------------------------------------------------------

/// Result of merging two models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeResult {
    /// The merged model.
    pub merged: ExportedModel,
    /// Conflicts that were resolved.
    pub conflicts: Vec<MergeConflict>,
    /// Statistics about the merge.
    pub stats: MergeStats,
}

/// A conflict encountered during model merge.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeConflict {
    /// What conflicted.
    pub item: String,
    /// Value from model A.
    pub value_a: String,
    /// Value from model B.
    pub value_b: String,
    /// How it was resolved.
    pub resolution: ConflictResolution,
}

/// How a merge conflict was resolved.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ConflictResolution {
    /// Kept the value from model A.
    KeepA,
    /// Kept the value from model B.
    KeepB,
    /// Merged both values.
    Merged,
    /// Used the higher confidence value.
    HigherConfidence,
}

/// Statistics about a model merge operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeStats {
    /// Total node types in the merged model.
    pub total_node_types: usize,
    /// Total edge types in the merged model.
    pub total_edge_types: usize,
    /// Total causal nodes in the merged model.
    pub total_causal_nodes: usize,
    /// Total causal edges in the merged model.
    pub total_causal_edges: usize,
    /// Number of conflicts resolved.
    pub conflicts_resolved: usize,
    /// Node types from A only.
    pub nodes_from_a: usize,
    /// Node types from B only.
    pub nodes_from_b: usize,
    /// Node types shared between A and B.
    pub nodes_shared: usize,
}

/// Merge two exported models into one.
///
/// Node types are unioned by name (higher-dimension embedding strategy wins
/// on conflict). Edge types are unioned by (from, to, type) key with higher
/// confidence kept. Causal nodes are unioned by label. Causal edges are
/// unioned by (source, target) key with weights averaged on overlap.
pub fn merge_models(a: &ExportedModel, b: &ExportedModel) -> MergeResult {
    let mut conflicts = Vec::new();

    // ── Node types ──────────────────────────────────────────────
    let a_node_map: HashMap<&str, &NodeTypeSpec> =
        a.node_types.iter().map(|n| (n.name.as_str(), n)).collect();
    let b_node_map: HashMap<&str, &NodeTypeSpec> =
        b.node_types.iter().map(|n| (n.name.as_str(), n)).collect();

    let all_node_names: HashSet<&str> = a_node_map
        .keys()
        .chain(b_node_map.keys())
        .copied()
        .collect();

    let mut merged_node_types = Vec::new();
    let mut nodes_from_a = 0usize;
    let mut nodes_from_b = 0usize;
    let mut nodes_shared = 0usize;

    for name in &all_node_names {
        match (a_node_map.get(name), b_node_map.get(name)) {
            (Some(na), None) => {
                merged_node_types.push((*na).clone());
                nodes_from_a += 1;
            }
            (None, Some(nb)) => {
                merged_node_types.push((*nb).clone());
                nodes_from_b += 1;
            }
            (Some(na), Some(nb)) => {
                nodes_shared += 1;
                if na.embedding_strategy != nb.embedding_strategy {
                    let (winner, resolution) = if na.dimensions >= nb.dimensions {
                        ((*na).clone(), ConflictResolution::KeepA)
                    } else {
                        ((*nb).clone(), ConflictResolution::KeepB)
                    };
                    conflicts.push(MergeConflict {
                        item: format!("node_type:{}", name),
                        value_a: na.embedding_strategy.clone(),
                        value_b: nb.embedding_strategy.clone(),
                        resolution,
                    });
                    merged_node_types.push(winner);
                } else {
                    merged_node_types.push((*na).clone());
                }
            }
            (None, None) => unreachable!(),
        }
    }

    // ── Edge types ──────────────────────────────────────────────
    let edge_key = |e: &EdgeTypeSpec| format!("{}->{}:{}", e.from_type, e.to_type, e.edge_type);
    let a_edge_map: HashMap<String, &EdgeTypeSpec> =
        a.edge_types.iter().map(|e| (edge_key(e), e)).collect();
    let b_edge_map: HashMap<String, &EdgeTypeSpec> =
        b.edge_types.iter().map(|e| (edge_key(e), e)).collect();

    let all_edge_keys: HashSet<&str> = a_edge_map
        .keys()
        .chain(b_edge_map.keys())
        .map(|s| s.as_str())
        .collect();

    let mut merged_edge_types = Vec::new();
    for key in &all_edge_keys {
        match (a_edge_map.get(*key), b_edge_map.get(*key)) {
            (Some(ea), None) => merged_edge_types.push((*ea).clone()),
            (None, Some(eb)) => merged_edge_types.push((*eb).clone()),
            (Some(ea), Some(eb)) => {
                if (ea.confidence - eb.confidence).abs() > f64::EPSILON {
                    let (winner, resolution) = if ea.confidence >= eb.confidence {
                        ((*ea).clone(), ConflictResolution::HigherConfidence)
                    } else {
                        ((*eb).clone(), ConflictResolution::HigherConfidence)
                    };
                    conflicts.push(MergeConflict {
                        item: format!("edge_type:{}", key),
                        value_a: format!("{:.4}", ea.confidence),
                        value_b: format!("{:.4}", eb.confidence),
                        resolution,
                    });
                    merged_edge_types.push(winner);
                } else {
                    merged_edge_types.push((*ea).clone());
                }
            }
            (None, None) => unreachable!(),
        }
    }

    // ── Causal nodes ────────────────────────────────────────────
    let a_cn_map: HashMap<&str, &ExportedCausalNode> = a
        .causal_nodes
        .iter()
        .map(|n| (n.label.as_str(), n))
        .collect();
    let b_cn_map: HashMap<&str, &ExportedCausalNode> = b
        .causal_nodes
        .iter()
        .map(|n| (n.label.as_str(), n))
        .collect();

    let all_cn_labels: HashSet<&str> = a_cn_map.keys().chain(b_cn_map.keys()).copied().collect();

    let mut merged_causal_nodes = Vec::new();
    for label in &all_cn_labels {
        match (a_cn_map.get(label), b_cn_map.get(label)) {
            (Some(na), None) => merged_causal_nodes.push((*na).clone()),
            (None, Some(nb)) => merged_causal_nodes.push((*nb).clone()),
            (Some(_na), Some(nb)) => {
                // Both have the node; prefer B (assumed later export).
                merged_causal_nodes.push((*nb).clone());
            }
            (None, None) => unreachable!(),
        }
    }

    // ── Causal edges ────────────────────────────────────────────
    let ce_key = |e: &ExportedCausalEdge| format!("{}->{}", e.source_label, e.target_label);
    let a_ce_map: HashMap<String, &ExportedCausalEdge> =
        a.causal_edges.iter().map(|e| (ce_key(e), e)).collect();
    let b_ce_map: HashMap<String, &ExportedCausalEdge> =
        b.causal_edges.iter().map(|e| (ce_key(e), e)).collect();

    let all_ce_keys: HashSet<&str> = a_ce_map
        .keys()
        .chain(b_ce_map.keys())
        .map(|s| s.as_str())
        .collect();

    let mut merged_causal_edges = Vec::new();
    for key in &all_ce_keys {
        match (a_ce_map.get(*key), b_ce_map.get(*key)) {
            (Some(ea), None) => merged_causal_edges.push((*ea).clone()),
            (None, Some(eb)) => merged_causal_edges.push((*eb).clone()),
            (Some(ea), Some(eb)) => {
                let mut merged_edge = (*ea).clone();
                merged_edge.weight = (ea.weight + eb.weight) / 2.0;
                if ea.edge_type != eb.edge_type {
                    conflicts.push(MergeConflict {
                        item: format!("causal_edge:{}", key),
                        value_a: ea.edge_type.clone(),
                        value_b: eb.edge_type.clone(),
                        resolution: ConflictResolution::Merged,
                    });
                }
                merged_causal_edges.push(merged_edge);
            }
            (None, None) => unreachable!(),
        }
    }

    // ── Merged metadata ─────────────────────────────────────────
    let mut merged_metadata = a.metadata.clone();
    for (k, v) in &b.metadata {
        merged_metadata
            .entry(k.clone())
            .or_insert_with(|| v.clone());
    }

    // ── Merged confidence: weighted average by causal node count ──
    let a_weight = a.causal_nodes.len().max(1) as f64;
    let b_weight = b.causal_nodes.len().max(1) as f64;
    let merged_confidence =
        (a.confidence * a_weight + b.confidence * b_weight) / (a_weight + b_weight);

    let merged = ExportedModel {
        version: "1.0".to_string(),
        domain: format!("{}+{}", a.domain, b.domain),
        exported_at: Utc::now(),
        confidence: merged_confidence,
        node_types: merged_node_types,
        edge_types: merged_edge_types,
        causal_nodes: merged_causal_nodes,
        causal_edges: merged_causal_edges,
        metadata: merged_metadata,
    };

    let stats = MergeStats {
        total_node_types: merged.node_types.len(),
        total_edge_types: merged.edge_types.len(),
        total_causal_nodes: merged.causal_nodes.len(),
        total_causal_edges: merged.causal_edges.len(),
        conflicts_resolved: conflicts.len(),
        nodes_from_a,
        nodes_from_b,
        nodes_shared,
    };

    MergeResult {
        merged,
        conflicts,
        stats,
    }
}

// ---------------------------------------------------------------------------
// Knowledge Base Persistence (K3c-G5b)
// ---------------------------------------------------------------------------

/// Serializable form of the knowledge base for JSON persistence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SerializableKB {
    /// Schema version.
    pub version: u32,
    /// Learned strategy patterns.
    pub patterns: Vec<StrategyPattern>,
    /// Domains that have been modeled.
    pub domains_modeled: Vec<String>,
    /// Total modeling sessions conducted.
    pub total_sessions: u64,
    /// When the KB was last updated.
    pub last_updated: DateTime<Utc>,
}

impl WeaverKnowledgeBase {
    /// Convert to a serializable representation.
    pub fn to_serializable(&self) -> SerializableKB {
        let patterns = self.list_strategies();
        let domains: Vec<String> = patterns
            .iter()
            .map(|p| p.context.clone())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        SerializableKB {
            version: 1,
            patterns,
            domains_modeled: domains,
            total_sessions: self.count(),
            last_updated: Utc::now(),
        }
    }

    /// Reconstruct from a serializable representation.
    pub fn from_serializable(kb: SerializableKB) -> Self {
        let result = Self::new();
        for pattern in kb.patterns {
            result.record_strategy(pattern);
        }
        result
    }

    /// Save the knowledge base to a JSON file.
    pub fn save_to_file(&self, path: &Path) -> Result<(), WeaverError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.to_serializable())?;
        std::fs::write(path, json)?;
        Ok(())
    }

    /// Load the knowledge base from a JSON file.
    pub fn load_from_file(path: &Path) -> Result<Self, WeaverError> {
        let data = std::fs::read_to_string(path)?;
        let kb: SerializableKB = serde_json::from_str(&data)?;
        Ok(Self::from_serializable(kb))
    }

    /// Add a strategy pattern learned from a modeling session.
    ///
    /// If a similar pattern exists (same decision_type and context),
    /// updates the existing pattern's improvement based on new evidence.
    /// Otherwise, adds it as a new pattern.
    pub fn learn_pattern(&self, pattern: StrategyPattern) {
        if let Ok(mut strategies) = self.strategies.write() {
            if let Some(existing) = strategies
                .iter_mut()
                .find(|s| s.decision_type == pattern.decision_type && s.context == pattern.context)
            {
                existing.improvement = (existing.improvement + pattern.improvement) / 2.0;
                existing.timestamp = pattern.timestamp;
                return;
            }
            strategies.push(pattern);
            self.strategy_count.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Find applicable patterns for a domain given its characteristics.
    ///
    /// Scores each pattern by how many of the provided characteristics
    /// appear in the pattern's context. Returns patterns sorted by
    /// relevance (highest match score first).
    pub fn find_patterns(&self, domain_characteristics: &[String]) -> Vec<StrategyPattern> {
        let strategies = match self.strategies.read() {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };

        let mut scored: Vec<(usize, &StrategyPattern)> = strategies
            .iter()
            .filter_map(|s| {
                let score = domain_characteristics
                    .iter()
                    .filter(|c| s.context.contains(c.as_str()))
                    .count();
                if score > 0 { Some((score, s)) } else { None }
            })
            .collect();

        scored.sort_by_key(|b| std::cmp::Reverse(b.0));
        scored.into_iter().map(|(_, p)| p.clone()).collect()
    }

    /// Number of stored patterns.
    pub fn pattern_count(&self) -> usize {
        self.strategies.read().map(|s| s.len()).unwrap_or(0)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hnsw_service::HnswServiceConfig;

    fn make_engine() -> WeaverEngine {
        let graph = Arc::new(CausalGraph::new());
        let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
        WeaverEngine::new_with_mock(graph, hnsw)
    }

    #[test]
    fn start_session_creates_session() {
        let engine = make_engine();
        let sid = engine
            .start_session("test-domain", Some("test context"), None)
            .unwrap();
        assert!(!sid.is_empty());
        let session = engine.get_session("test-domain").unwrap();
        assert_eq!(session.domain, "test-domain");
        assert!(session.active);
        assert_eq!(session.confidence, 0.0);
    }

    #[test]
    fn duplicate_session_start_fails() {
        let engine = make_engine();
        engine.start_session("dup", None, None).unwrap();
        let result = engine.start_session("dup", None, None);
        assert!(result.is_err());
    }

    #[test]
    fn stop_and_resume_session() {
        let engine = make_engine();
        engine.start_session("lifecycle", None, None).unwrap();
        engine.stop_session("lifecycle").unwrap();
        assert!(!engine.get_session("lifecycle").unwrap().active);
        engine.resume_session("lifecycle").unwrap();
        assert!(engine.get_session("lifecycle").unwrap().active);
    }

    #[test]
    fn add_source_records_ingestion() {
        let engine = make_engine();
        engine.start_session("src-test", None, None).unwrap();
        engine.add_source("src-test", "git_log", None).unwrap();
        let session = engine.get_session("src-test").unwrap();
        assert_eq!(session.sources_ingested, vec!["git_log"]);
    }

    #[test]
    fn add_source_nonexistent_domain_fails() {
        let engine = make_engine();
        let result = engine.add_source("nope", "git_log", None);
        assert!(result.is_err());
    }

    #[test]
    fn evaluate_confidence_basic() {
        let engine = make_engine();
        engine.start_session("conf", None, None).unwrap();
        let report = engine.evaluate_confidence("conf").unwrap();
        assert!(report.overall >= 0.0 && report.overall <= 1.0);
        // With no sources, should have gaps.
        assert!(!report.gaps.is_empty());
    }

    #[test]
    fn evaluate_confidence_improves_with_sources() {
        let engine = make_engine();
        engine.start_session("improve", None, None).unwrap();
        let r1 = engine.evaluate_confidence("improve").unwrap();
        engine.add_source("improve", "git_log", None).unwrap();
        engine.add_source("improve", "file_tree", None).unwrap();
        let r2 = engine.evaluate_confidence("improve").unwrap();
        assert!(r2.overall >= r1.overall);
    }

    #[test]
    fn tick_with_no_session_returns_idle() {
        let engine = make_engine();
        let result = engine.tick(Duration::from_secs(1));
        assert!(matches!(result, TickResult::Idle));
    }

    #[test]
    fn tick_processes_active_session() {
        let engine = make_engine();
        engine.start_session("tick-test", None, None).unwrap();
        let result = engine.tick(Duration::from_secs(5));
        assert!(matches!(result, TickResult::Progress { .. }));
        let session = engine.get_session("tick-test").unwrap();
        assert_eq!(session.tick_count, 1);
    }

    #[test]
    fn tick_increments_total_count() {
        let engine = make_engine();
        engine.start_session("ticks", None, None).unwrap();
        engine.tick(Duration::from_secs(5));
        engine.tick(Duration::from_secs(5));
        assert_eq!(engine.total_ticks(), 2);
    }

    #[test]
    fn tick_skips_stopped_session() {
        let engine = make_engine();
        engine.start_session("stopped", None, None).unwrap();
        engine.stop_session("stopped").unwrap();
        let result = engine.tick(Duration::from_secs(5));
        assert!(matches!(result, TickResult::Idle));
    }

    #[test]
    fn export_model_basic() {
        let engine = make_engine();
        engine.start_session("export", None, None).unwrap();
        engine.add_source("export", "git_log", None).unwrap();
        let model = engine.export_model("export", 0.0).unwrap();
        assert_eq!(model.domain, "export");
        assert_eq!(model.version, "1.0");
        assert!(!model.node_types.is_empty());
    }

    #[test]
    fn export_model_filters_by_confidence() {
        let engine = make_engine();
        engine.start_session("filter", None, None).unwrap();
        engine.add_source("filter", "git", None).unwrap();
        engine.add_source("filter", "file", None).unwrap();
        let model_all = engine.export_model("filter", 0.0).unwrap();
        let model_high = engine.export_model("filter", 0.5).unwrap();
        assert!(model_all.edge_types.len() >= model_high.edge_types.len());
    }

    #[test]
    fn import_model_creates_session() {
        let engine = make_engine();
        let model = ExportedModel {
            version: "1.0".into(),
            domain: "imported".into(),
            exported_at: Utc::now(),
            confidence: 0.75,
            node_types: vec![],
            edge_types: vec![],
            causal_nodes: vec![],
            causal_edges: vec![],
            metadata: HashMap::new(),
        };
        engine.import_model("imported", model).unwrap();
        let session = engine.get_session("imported").unwrap();
        assert_eq!(session.confidence, 0.75);
        assert!(session.active);
    }

    #[test]
    fn import_model_version_check() {
        let engine = make_engine();
        let model = ExportedModel {
            version: "2.0".into(),
            domain: "bad".into(),
            exported_at: Utc::now(),
            confidence: 0.5,
            node_types: vec![],
            edge_types: vec![],
            causal_nodes: vec![],
            causal_edges: vec![],
            metadata: HashMap::new(),
        };
        let result = engine.import_model("bad", model);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("incompatible"));
    }

    #[test]
    fn meta_loom_events_recorded_on_session_start() {
        let engine = make_engine();
        engine.start_session("meta", None, None).unwrap();
        let events = engine.meta_loom_events("meta");
        assert!(!events.is_empty());
        assert!(matches!(
            events[0].decision_type,
            MetaDecisionType::ModelVersionBumped { .. }
        ));
    }

    #[test]
    fn meta_loom_events_recorded_on_source_add() {
        let engine = make_engine();
        engine.start_session("meta-src", None, None).unwrap();
        engine.add_source("meta-src", "git_log", None).unwrap();
        let events = engine.meta_loom_events("meta-src");
        assert!(events.len() >= 2); // session start + source add
        assert!(matches!(
            events.last().unwrap().decision_type,
            MetaDecisionType::SourceAdded { .. }
        ));
    }

    #[test]
    fn knowledge_base_record_and_list() {
        let kb = WeaverKnowledgeBase::new();
        kb.record_strategy(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust-project".into(),
            improvement: 0.15,
            timestamp: Utc::now(),
        });
        let strategies = kb.list_strategies();
        assert_eq!(strategies.len(), 1);
        assert_eq!(strategies[0].improvement, 0.15);
    }

    #[test]
    fn knowledge_base_strategies_for_domain() {
        let kb = WeaverKnowledgeBase::new();
        kb.record_strategy(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust".into(),
            improvement: 0.1,
            timestamp: Utc::now(),
        });
        kb.record_strategy(StrategyPattern {
            decision_type: "EdgeType".into(),
            context: "python".into(),
            improvement: 0.2,
            timestamp: Utc::now(),
        });
        assert_eq!(kb.strategies_for("rust").len(), 1);
        assert_eq!(kb.strategies_for("python").len(), 1);
        assert_eq!(kb.strategies_for("go").len(), 0);
    }

    #[test]
    fn knowledge_base_export() {
        let kb = WeaverKnowledgeBase::new();
        kb.record_strategy(StrategyPattern {
            decision_type: "test".into(),
            context: "test".into(),
            improvement: 0.5,
            timestamp: Utc::now(),
        });
        let exported = kb.export();
        assert!(exported.is_array());
    }

    #[test]
    fn handle_command_session_start() {
        let engine = make_engine();
        let resp = engine.handle_command(WeaverCommand::SessionStart {
            domain: "cmd-test".into(),
            git_path: None,
            context: None,
            goal: None,
        });
        assert!(matches!(resp, WeaverResponse::SessionStarted { .. }));
    }

    #[test]
    fn handle_command_confidence() {
        let engine = make_engine();
        engine.start_session("cmd-conf", None, None).unwrap();
        let resp = engine.handle_command(WeaverCommand::Confidence {
            domain: "cmd-conf".into(),
            edge: None,
            verbose: false,
        });
        assert!(matches!(resp, WeaverResponse::ConfidenceReport(_)));
    }

    #[test]
    fn handle_command_source_list() {
        let engine = make_engine();
        engine.start_session("cmd-src", None, None).unwrap();
        engine.add_source("cmd-src", "git_log", None).unwrap();
        let resp = engine.handle_command(WeaverCommand::SourceList {
            domain: "cmd-src".into(),
        });
        match resp {
            WeaverResponse::Sources(s) => assert_eq!(s, vec!["git_log"]),
            other => panic!("expected Sources, got {other:?}"),
        }
    }

    #[test]
    fn handle_command_unknown_domain() {
        let engine = make_engine();
        let resp = engine.handle_command(WeaverCommand::Confidence {
            domain: "missing".into(),
            edge: None,
            verbose: false,
        });
        assert!(matches!(resp, WeaverResponse::Error(_)));
    }

    #[test]
    fn list_sessions() {
        let engine = make_engine();
        engine.start_session("a", None, None).unwrap();
        engine.start_session("b", None, None).unwrap();
        let mut sessions = engine.list_sessions();
        sessions.sort();
        assert_eq!(sessions, vec!["a", "b"]);
    }

    #[tokio::test]
    async fn system_service_impl() {
        let engine = make_engine();
        assert_eq!(engine.name(), "weaver");
        assert_eq!(engine.service_type(), ServiceType::Core);
        engine.start().await.unwrap();
        let health = engine.health_check().await;
        assert_eq!(health, HealthStatus::Healthy);
        engine.stop().await.unwrap();
    }

    #[test]
    fn impulse_emitted_on_session_start() {
        let queue = Arc::new(ImpulseQueue::new());
        let graph = Arc::new(CausalGraph::new());
        let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
        let mut engine = WeaverEngine::new_with_mock(graph, hnsw);
        engine.set_impulse_queue(queue.clone());
        engine.start_session("impulse-test", None, None).unwrap();
        let impulses = queue.drain_ready();
        assert!(!impulses.is_empty());
        assert!(
            impulses
                .iter()
                .any(|i| i.impulse_type == ImpulseType::Custom(0x32))
        );
    }

    #[test]
    fn impulse_emitted_on_source_add() {
        let queue = Arc::new(ImpulseQueue::new());
        let graph = Arc::new(CausalGraph::new());
        let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
        let mut engine = WeaverEngine::new_with_mock(graph, hnsw);
        engine.set_impulse_queue(queue.clone());
        engine.start_session("impulse-src", None, None).unwrap();
        let _ = queue.drain_ready(); // clear session-start impulse
        engine.add_source("impulse-src", "git", None).unwrap();
        let impulses = queue.drain_ready();
        assert!(
            impulses
                .iter()
                .any(|i| i.impulse_type == ImpulseType::Custom(0x33))
        );
    }

    // ── Graph ingestion tests ────────────────────────────────────

    #[test]
    fn ingest_graph_file_git_history() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let graph_path = PathBuf::from(&manifest).join("../../.weftos/graph/git-history.json");
        if !graph_path.exists() {
            // Skip if running outside the project tree.
            return;
        }
        let engine = make_engine();
        let result = engine.ingest_graph_file(&graph_path).unwrap();
        assert!(result.nodes_added > 0, "should ingest at least one node");
        assert_eq!(result.source, "git-history");
        assert!(result.embeddings_created > 0, "should create embeddings");
        // Verify causal graph was populated.
        assert!(engine.causal_graph().node_count() > 0);
    }

    #[test]
    fn ingest_graph_file_module_deps() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let graph_path = PathBuf::from(&manifest).join("../../.weftos/graph/module-deps.json");
        if !graph_path.exists() {
            return;
        }
        let engine = make_engine();
        let result = engine.ingest_graph_file(&graph_path).unwrap();
        assert!(result.nodes_added > 0);
        assert_eq!(result.source, "module-dependencies");
    }

    #[test]
    fn ingest_graph_file_decisions() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let graph_path = PathBuf::from(&manifest).join("../../.weftos/graph/decisions.json");
        if !graph_path.exists() {
            return;
        }
        let engine = make_engine();
        let result = engine.ingest_graph_file(&graph_path).unwrap();
        assert!(result.nodes_added > 0);
        assert_eq!(result.source, "decisions-and-phases");
    }

    #[test]
    fn ingest_graph_creates_edges() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let graph_path = PathBuf::from(&manifest).join("../../.weftos/graph/git-history.json");
        if !graph_path.exists() {
            return;
        }
        let engine = make_engine();
        let result = engine.ingest_graph_file(&graph_path).unwrap();
        assert!(
            result.edges_added > 0,
            "git-history graph should have edges"
        );
        assert!(engine.causal_graph().edge_count() > 0);
    }

    #[test]
    fn ingest_graph_populates_hnsw() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let graph_path = PathBuf::from(&manifest).join("../../.weftos/graph/module-deps.json");
        if !graph_path.exists() {
            return;
        }
        let engine = make_engine();
        let result = engine.ingest_graph_file(&graph_path).unwrap();
        assert!(result.embeddings_created > 0);
        assert!(engine.hnsw().insert_count() > 0);
    }

    #[test]
    fn ingest_nonexistent_file_returns_error() {
        let engine = make_engine();
        let result = engine.ingest_graph_file(Path::new("/nonexistent/graph.json"));
        assert!(result.is_err());
    }

    #[test]
    fn ingest_invalid_json_returns_error() {
        let dir = std::env::temp_dir().join("weaver_test_invalid");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("bad.json");
        std::fs::write(&path, "not valid json {{{").unwrap();
        let engine = make_engine();
        let result = engine.ingest_graph_file(&path);
        assert!(result.is_err());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ingest_empty_graph_returns_zero_counts() {
        let dir = std::env::temp_dir().join("weaver_test_empty");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("empty.json");
        std::fs::write(&path, r#"{"source":"test","nodes":[],"edges":[]}"#).unwrap();
        let engine = make_engine();
        let result = engine.ingest_graph_file(&path).unwrap();
        assert_eq!(result.nodes_added, 0);
        assert_eq!(result.edges_added, 0);
        assert_eq!(result.embeddings_created, 0);
        assert_eq!(result.source, "test");
        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Confidence scoring tests ─────────────────────────────────

    #[test]
    fn compute_confidence_empty_graph() {
        let engine = make_engine();
        let report = engine.compute_confidence();
        assert_eq!(report.overall, 0.0);
        assert!(!report.gaps.is_empty(), "should have gaps with empty graph");
        assert!(
            !report.suggestions.is_empty(),
            "should have suggestions with empty graph"
        );
    }

    #[test]
    fn compute_confidence_with_nodes_only() {
        let engine = make_engine();
        // Add nodes without edges -- should be partially confident.
        for i in 0..10 {
            engine
                .causal_graph()
                .add_node(format!("node-{i}"), serde_json::json!({"test": true}));
        }
        let report = engine.compute_confidence();
        // All orphans: connectivity = 0, but volume > 0.
        assert!(
            report.overall > 0.0,
            "should have some confidence from volume"
        );
        assert!(report.overall < 0.5, "should be low without edges");
    }

    #[test]
    fn compute_confidence_with_connected_graph() {
        let engine = make_engine();
        // Create a small connected graph.
        let n1 = engine
            .causal_graph()
            .add_node("module-a".into(), serde_json::json!({}));
        let n2 = engine
            .causal_graph()
            .add_node("module-b".into(), serde_json::json!({}));
        let n3 = engine
            .causal_graph()
            .add_node("module-c".into(), serde_json::json!({}));
        engine
            .causal_graph()
            .link(n1, n2, CausalEdgeType::Enables, 1.0, 0, 0);
        engine
            .causal_graph()
            .link(n2, n3, CausalEdgeType::Causes, 0.8, 0, 0);

        let report = engine.compute_confidence();
        // At least 2 of 3 nodes have edges, so connectivity should be decent.
        assert!(report.overall > 0.0);
    }

    #[test]
    fn compute_confidence_detects_orphans() {
        let engine = make_engine();
        let n1 = engine
            .causal_graph()
            .add_node("connected-a".into(), serde_json::json!({}));
        let n2 = engine
            .causal_graph()
            .add_node("connected-b".into(), serde_json::json!({}));
        engine
            .causal_graph()
            .add_node("orphan-x".into(), serde_json::json!({}));
        engine
            .causal_graph()
            .link(n1, n2, CausalEdgeType::Follows, 1.0, 0, 0);

        let report = engine.compute_confidence();
        // The suggestion should mention orphan nodes.
        let has_orphan_suggestion = report.suggestions.iter().any(|s| match s {
            ModelingSuggestion::AddSource { reason, .. } => reason.contains("orphan"),
            _ => false,
        });
        assert!(has_orphan_suggestion, "should detect the orphan node");
    }

    #[test]
    fn compute_confidence_improves_with_more_data() {
        let engine = make_engine();
        // Small graph.
        let n1 = engine
            .causal_graph()
            .add_node("a".into(), serde_json::json!({}));
        let n2 = engine
            .causal_graph()
            .add_node("b".into(), serde_json::json!({}));
        engine
            .causal_graph()
            .link(n1, n2, CausalEdgeType::Enables, 1.0, 0, 0);
        let c1 = engine.compute_confidence().overall;

        // Add more connected nodes.
        for i in 0..50 {
            let na = engine
                .causal_graph()
                .add_node(format!("extra-{i}"), serde_json::json!({}));
            engine
                .causal_graph()
                .link(n1, na, CausalEdgeType::Correlates, 0.5, 0, 0);
        }
        let c2 = engine.compute_confidence().overall;
        assert!(
            c2 > c1,
            "confidence should increase with more data ({c2} > {c1})"
        );
    }

    // ── Model export/import roundtrip tests ──────────────────────

    #[test]
    fn export_model_includes_causal_data() {
        let engine = make_engine();
        engine.start_session("exp-causal", None, None).unwrap();
        // Add some nodes and edges.
        let n1 = engine
            .causal_graph()
            .add_node("node-a".into(), serde_json::json!({}));
        let n2 = engine
            .causal_graph()
            .add_node("node-b".into(), serde_json::json!({}));
        engine
            .causal_graph()
            .link(n1, n2, CausalEdgeType::Causes, 0.9, 0, 0);

        let model = engine.export_model("exp-causal", 0.0).unwrap();
        assert!(
            !model.causal_nodes.is_empty(),
            "exported model should have nodes"
        );
        assert!(
            !model.causal_edges.is_empty(),
            "exported model should have edges"
        );
    }

    #[test]
    fn export_model_to_file_roundtrip() {
        let dir = std::env::temp_dir().join("weaver_test_export");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test-model.json");

        let engine = make_engine();
        engine.start_session("roundtrip", None, None).unwrap();
        engine.add_source("roundtrip", "git_log", None).unwrap();

        // Add some graph data.
        let n1 = engine
            .causal_graph()
            .add_node("rt-a".into(), serde_json::json!({}));
        let n2 = engine
            .causal_graph()
            .add_node("rt-b".into(), serde_json::json!({}));
        engine
            .causal_graph()
            .link(n1, n2, CausalEdgeType::Enables, 1.0, 0, 0);

        // Export.
        let exported = engine
            .export_model_to_file("roundtrip", 0.0, &path)
            .unwrap();
        assert!(path.exists(), "export file should exist");

        // Read back and verify JSON is valid.
        let data = std::fs::read_to_string(&path).unwrap();
        let reimported: ExportedModel = serde_json::from_str(&data).unwrap();
        assert_eq!(reimported.domain, "roundtrip");
        assert_eq!(reimported.version, exported.version);
        assert_eq!(reimported.causal_nodes.len(), exported.causal_nodes.len());
        assert_eq!(reimported.causal_edges.len(), exported.causal_edges.len());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn import_model_from_file_works() {
        let dir = std::env::temp_dir().join("weaver_test_import");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("import-model.json");

        // Write a model file.
        let model = ExportedModel {
            version: "1.0".into(),
            domain: "file-import".into(),
            exported_at: Utc::now(),
            confidence: 0.82,
            node_types: vec![],
            edge_types: vec![],
            causal_nodes: vec![ExportedCausalNode {
                label: "test-node".into(),
                metadata: serde_json::json!({}),
            }],
            causal_edges: vec![],
            metadata: HashMap::new(),
        };
        let json = serde_json::to_string_pretty(&model).unwrap();
        std::fs::write(&path, json).unwrap();

        let engine = make_engine();
        engine.import_model_from_file("file-import", &path).unwrap();
        let session = engine.get_session("file-import").unwrap();
        assert_eq!(session.confidence, 0.82);
        assert!(session.active);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── Edge type parsing tests ──────────────────────────────────

    #[test]
    fn parse_edge_type_known_types() {
        assert_eq!(
            WeaverEngine::parse_edge_type("Causes"),
            CausalEdgeType::Causes
        );
        assert_eq!(
            WeaverEngine::parse_edge_type("Enables"),
            CausalEdgeType::Enables
        );
        assert_eq!(
            WeaverEngine::parse_edge_type("Follows"),
            CausalEdgeType::Follows
        );
        assert_eq!(
            WeaverEngine::parse_edge_type("Correlates"),
            CausalEdgeType::Correlates
        );
        assert_eq!(
            WeaverEngine::parse_edge_type("EvidenceFor"),
            CausalEdgeType::EvidenceFor
        );
        assert_eq!(
            WeaverEngine::parse_edge_type("Inhibits"),
            CausalEdgeType::Inhibits
        );
        assert_eq!(
            WeaverEngine::parse_edge_type("Contradicts"),
            CausalEdgeType::Contradicts
        );
        assert_eq!(
            WeaverEngine::parse_edge_type("TriggeredBy"),
            CausalEdgeType::TriggeredBy
        );
    }

    #[test]
    fn parse_edge_type_unknown_defaults_to_correlates() {
        assert_eq!(
            WeaverEngine::parse_edge_type("FooBar"),
            CausalEdgeType::Correlates
        );
        assert_eq!(
            WeaverEngine::parse_edge_type(""),
            CausalEdgeType::Correlates
        );
    }

    // ── WeaverError tests ────────────────────────────────────────

    #[test]
    fn weaver_error_display() {
        let io_err = WeaverError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            "file missing",
        ));
        assert!(io_err.to_string().contains("I/O"));

        let domain_err = WeaverError::Domain("test failure".to_string());
        assert!(domain_err.to_string().contains("test failure"));
    }

    // ── CognitiveTick integration tests ─────────────────────────

    fn make_engine_mut() -> WeaverEngine {
        let graph = Arc::new(CausalGraph::new());
        let hnsw = Arc::new(HnswService::new(HnswServiceConfig::default()));
        WeaverEngine::new_with_mock(graph, hnsw)
    }

    #[test]
    fn on_tick_respects_budget() {
        let mut engine = make_engine_mut();
        engine.start_session("tick-budget", None, None).unwrap();
        let result = engine.on_tick(500); // 500ms budget
        assert!(
            result.within_budget,
            "tick should complete within 500ms budget"
        );
        assert!(result.elapsed_ms <= 500, "elapsed should be within budget");
    }

    #[test]
    fn on_tick_returns_correct_fields() {
        let mut engine = make_engine_mut();
        engine.start_session("tick-fields", None, None).unwrap();
        let result = engine.on_tick(100);
        assert_eq!(result.budget_ms, 100);
        assert_eq!(result.tick_number, 0); // first tick
        // No git poller or file watcher configured, so these should be 0.
        assert_eq!(result.git_commits_found, 0);
        assert_eq!(result.files_changed, 0);
    }

    #[test]
    fn on_tick_increments_tick_count() {
        let mut engine = make_engine_mut();
        engine.start_session("tick-count", None, None).unwrap();
        engine.on_tick(100);
        engine.on_tick(100);
        engine.on_tick(100);
        // on_tick calls tick() internally which also increments, plus on_tick itself.
        // The on_tick method does fetch_add(1) each call.
        assert!(engine.total_ticks() >= 3, "should have at least 3 ticks");
    }

    #[test]
    fn on_tick_confidence_update_after_100_ticks() {
        let mut engine = make_engine_mut();
        engine.start_session("conf-update", None, None).unwrap();
        // Simulate 101 ticks to trigger confidence update.
        engine.ticks_since_confidence_update = 101;
        let result = engine.on_tick(1000);
        assert!(
            result.confidence_updated,
            "confidence should be updated after 100+ ticks"
        );
        assert!(
            engine.cached_confidence().is_some(),
            "cached confidence should be set"
        );
        assert_eq!(
            engine.ticks_since_confidence_update, 1,
            "counter should reset to 1 (incremented after reset)"
        );
    }

    #[test]
    fn on_tick_no_confidence_update_before_100_ticks() {
        let mut engine = make_engine_mut();
        engine.start_session("no-conf-update", None, None).unwrap();
        let result = engine.on_tick(100);
        assert!(
            !result.confidence_updated,
            "should not update confidence on first tick"
        );
    }

    #[test]
    fn on_tick_git_and_file_watcher_disabled_by_default() {
        let engine = make_engine_mut();
        assert!(
            engine.git_poller().is_none(),
            "git poller should be None by default"
        );
        assert!(
            engine.file_watcher().is_none(),
            "file watcher should be None by default"
        );
    }

    // ── GitPoller tests ─────────────────────────────────────────

    #[test]
    fn git_poller_poll_detects_commits_in_real_repo() {
        // Use the actual project repo for this test.
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let repo_path = PathBuf::from(&manifest).join("../..");
        if !repo_path.join(".git").exists() {
            return; // skip if not in a git repo
        }
        let mut poller = GitPoller::new(repo_path, "HEAD".to_string());
        // First poll should detect at least 1 commit.
        let count = poller.poll();
        assert!(count >= 1, "first poll should find at least 1 commit");
        assert!(poller.last_hash().is_some(), "last hash should be set");
    }

    #[test]
    fn git_poller_poll_returns_zero_on_no_changes() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let repo_path = PathBuf::from(&manifest).join("../..");
        if !repo_path.join(".git").exists() {
            return;
        }
        let mut poller = GitPoller::new(repo_path, "HEAD".to_string());
        poller.poll(); // first poll sets the baseline
        let count = poller.poll(); // second poll, no new commits
        assert_eq!(count, 0, "second poll should find 0 new commits");
    }

    #[test]
    fn git_poller_disabled_returns_zero() {
        let mut poller = GitPoller::new(PathBuf::from("/tmp"), "main".to_string());
        poller.set_enabled(false);
        assert_eq!(poller.poll(), 0);
        assert!(!poller.is_enabled());
    }

    #[test]
    fn git_poller_nonexistent_repo_returns_zero() {
        let mut poller = GitPoller::new(
            PathBuf::from("/nonexistent/path/to/repo"),
            "main".to_string(),
        );
        let count = poller.poll();
        assert_eq!(count, 0, "should return 0 for nonexistent repo");
        assert!(poller.last_hash().is_none());
    }

    #[test]
    fn git_poller_branch_accessor() {
        let poller = GitPoller::new(PathBuf::from("/tmp"), "develop".to_string());
        assert_eq!(poller.branch(), "develop");
    }

    // ── FileWatcher tests ───────────────────────────────────────

    #[test]
    fn file_watcher_watch_and_poll_detects_mtime_change() {
        let dir = std::env::temp_dir().join("weaver_fw_test_mtime");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("test.rs");
        std::fs::write(&path, "fn main() {}").unwrap();

        let mut watcher = FileWatcher::new(dir.clone(), vec!["*.rs".to_string()]);
        watcher.watch(path.clone());
        assert_eq!(watcher.watched_count(), 1);

        // First poll: no changes (mtime matches).
        let changed = watcher.poll_changes();
        assert!(changed.is_empty(), "no changes on first poll");

        // Simulate mtime change by sleeping briefly and rewriting.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(&path, "fn main() { println!(\"changed\"); }").unwrap();

        let changed = watcher.poll_changes();
        assert_eq!(changed.len(), 1, "should detect the changed file");
        assert_eq!(changed[0], path);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_watcher_watch_directory_registers_files() {
        let dir = std::env::temp_dir().join("weaver_fw_test_dir");
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(dir.join("lib.rs"), "// lib").unwrap();
        std::fs::write(dir.join("main.rs"), "// main").unwrap();
        std::fs::write(dir.join("readme.md"), "# readme").unwrap();

        let mut watcher = FileWatcher::new(dir.clone(), vec!["*.rs".to_string()]);
        watcher.watch_directory();
        assert_eq!(watcher.watched_count(), 2, "should only watch .rs files");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_watcher_detects_deleted_file() {
        let dir = std::env::temp_dir().join("weaver_fw_test_delete");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("temp.rs");
        std::fs::write(&path, "// temp").unwrap();

        let mut watcher = FileWatcher::new(dir.clone(), vec!["*.rs".to_string()]);
        watcher.watch(path.clone());

        // Delete the file.
        std::fs::remove_file(&path).unwrap();
        let changed = watcher.poll_changes();
        assert_eq!(changed.len(), 1, "should detect deleted file");
        assert_eq!(
            watcher.watched_count(),
            0,
            "deleted file should be unregistered"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn file_watcher_disabled_returns_empty() {
        let mut watcher = FileWatcher::new(PathBuf::from("/tmp"), vec!["*.rs".to_string()]);
        watcher.set_enabled(false);
        assert!(watcher.poll_changes().is_empty());
        assert!(!watcher.is_enabled());
    }

    // ── WeaverEngine git/file integration tests ─────────────────

    #[test]
    fn enable_git_polling_sets_poller() {
        let mut engine = make_engine_mut();
        assert!(engine.git_poller().is_none());
        engine.enable_git_polling(PathBuf::from("/tmp"), "main".to_string());
        assert!(engine.git_poller().is_some());
        assert_eq!(engine.git_poller().unwrap().branch(), "main");
    }

    #[test]
    fn enable_file_watching_sets_watcher() {
        let dir = std::env::temp_dir().join("weaver_fw_test_enable");
        std::fs::create_dir_all(&dir).ok();
        std::fs::write(dir.join("test.rs"), "// test").unwrap();

        let mut engine = make_engine_mut();
        assert!(engine.file_watcher().is_none());
        engine.enable_file_watching(dir.clone(), vec!["*.rs".to_string()]);
        assert!(engine.file_watcher().is_some());
        assert_eq!(engine.file_watcher().unwrap().watched_count(), 1);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn on_tick_with_git_polling_enabled() {
        let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let repo_path = PathBuf::from(&manifest).join("../..");
        if !repo_path.join(".git").exists() {
            return;
        }
        let mut engine = make_engine_mut();
        engine.start_session("git-tick", None, None).unwrap();
        engine.enable_git_polling(repo_path, "HEAD".to_string());
        let result = engine.on_tick(500);
        // First tick should detect at least 1 commit (initial baseline).
        assert!(
            result.git_commits_found >= 1,
            "first on_tick with git polling should find commits"
        );
    }

    #[test]
    fn cognitive_tick_result_default() {
        let result = CognitiveTickResult::default();
        assert_eq!(result.tick_number, 0);
        assert_eq!(result.elapsed_ms, 0);
        assert_eq!(result.budget_ms, 0);
        assert_eq!(result.git_commits_found, 0);
        assert_eq!(result.files_changed, 0);
        assert_eq!(result.nodes_processed, 0);
        assert!(!result.confidence_updated);
        assert!(!result.within_budget);
    }

    #[test]
    fn cognitive_tick_result_serde_roundtrip() {
        let result = CognitiveTickResult {
            tick_number: 42,
            elapsed_ms: 15,
            budget_ms: 50,
            git_commits_found: 3,
            files_changed: 2,
            nodes_processed: 10,
            confidence_updated: true,
            within_budget: true,
        };
        let json = serde_json::to_string(&result).unwrap();
        let restored: CognitiveTickResult = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tick_number, 42);
        assert_eq!(restored.git_commits_found, 3);
        assert!(restored.confidence_updated);
    }

    // ── ConfidenceHistory tests (Item 1) ─────────────────────────

    #[test]
    fn confidence_history_record_and_latest() {
        let mut history = ConfidenceHistory::new(10);
        assert!(history.is_empty());
        assert!(history.latest().is_none());

        history.record(ConfidenceSnapshot {
            timestamp: Utc::now(),
            tick_number: 1,
            confidence: 0.5,
            node_count: 10,
            edge_count: 5,
            gap_count: 2,
            trigger: ConfidenceTrigger::Periodic,
        });
        assert_eq!(history.len(), 1);
        assert_eq!(history.latest().unwrap().confidence, 0.5);
    }

    #[test]
    fn confidence_history_ring_buffer_eviction() {
        let mut history = ConfidenceHistory::new(3);
        for i in 0..5 {
            history.record(ConfidenceSnapshot {
                timestamp: Utc::now(),
                tick_number: i,
                confidence: i as f64 * 0.1,
                node_count: 0,
                edge_count: 0,
                gap_count: 0,
                trigger: ConfidenceTrigger::Periodic,
            });
        }
        assert_eq!(history.len(), 3);
        // Oldest should have been evicted; first remaining is tick 2.
        assert_eq!(history.all().front().unwrap().tick_number, 2);
        assert_eq!(history.latest().unwrap().tick_number, 4);
    }

    #[test]
    fn confidence_history_trend_improving() {
        let mut history = ConfidenceHistory::new(10);
        for i in 0..5 {
            history.record(ConfidenceSnapshot {
                timestamp: Utc::now(),
                tick_number: i,
                confidence: 0.3 + i as f64 * 0.1,
                node_count: 0,
                edge_count: 0,
                gap_count: 0,
                trigger: ConfidenceTrigger::Periodic,
            });
        }
        let trend = history.trend(5);
        assert_eq!(trend.direction, TrendDirection::Improving);
        assert!(trend.delta > 0.01);
        assert_eq!(trend.samples, 5);
    }

    #[test]
    fn confidence_history_trend_declining() {
        let mut history = ConfidenceHistory::new(10);
        for i in 0..4 {
            history.record(ConfidenceSnapshot {
                timestamp: Utc::now(),
                tick_number: i,
                confidence: 0.8 - i as f64 * 0.1,
                node_count: 0,
                edge_count: 0,
                gap_count: 0,
                trigger: ConfidenceTrigger::Periodic,
            });
        }
        let trend = history.trend(4);
        assert_eq!(trend.direction, TrendDirection::Declining);
        assert!(trend.delta < -0.01);
    }

    #[test]
    fn confidence_history_trend_stable() {
        let mut history = ConfidenceHistory::new(10);
        for i in 0..5 {
            history.record(ConfidenceSnapshot {
                timestamp: Utc::now(),
                tick_number: i,
                confidence: 0.75,
                node_count: 0,
                edge_count: 0,
                gap_count: 0,
                trigger: ConfidenceTrigger::Periodic,
            });
        }
        let trend = history.trend(5);
        assert_eq!(trend.direction, TrendDirection::Stable);
        assert!(trend.delta.abs() <= 0.01);
        assert!((trend.avg_confidence - 0.75).abs() < 0.001);
    }

    #[test]
    fn confidence_history_trend_empty() {
        let history = ConfidenceHistory::new(10);
        let trend = history.trend(5);
        assert_eq!(trend.direction, TrendDirection::Stable);
        assert_eq!(trend.samples, 0);
    }

    #[test]
    fn confidence_history_trend_window_clamp() {
        let mut history = ConfidenceHistory::new(10);
        history.record(ConfidenceSnapshot {
            timestamp: Utc::now(),
            tick_number: 0,
            confidence: 0.5,
            node_count: 0,
            edge_count: 0,
            gap_count: 0,
            trigger: ConfidenceTrigger::Manual,
        });
        // Ask for 100 but only 1 exists.
        let trend = history.trend(100);
        assert_eq!(trend.samples, 1);
    }

    // ── StrategyTracker tests (Item 4) ──────────────────────────

    #[test]
    fn strategy_tracker_begin_complete() {
        let mut tracker = StrategyTracker::new(50);
        assert!(tracker.is_empty());
        let handle = tracker.begin_strategy("add_git", "Add git log", 0.4);
        tracker.complete_strategy(handle, 0.55);
        assert_eq!(tracker.len(), 1);
        let outcome = &tracker.outcomes()[0];
        assert_eq!(outcome.strategy, "add_git");
        assert!((outcome.delta - 0.15).abs() < 0.001);
        assert!(outcome.beneficial);
    }

    #[test]
    fn strategy_tracker_most_effective() {
        let mut tracker = StrategyTracker::new(50);
        let h1 = tracker.begin_strategy("a", "desc a", 0.3);
        tracker.complete_strategy(h1, 0.5); // +0.2
        let h2 = tracker.begin_strategy("b", "desc b", 0.5);
        tracker.complete_strategy(h2, 0.9); // +0.4
        let h3 = tracker.begin_strategy("c", "desc c", 0.9);
        tracker.complete_strategy(h3, 0.85); // -0.05

        let top = tracker.most_effective(2);
        assert_eq!(top.len(), 2);
        assert_eq!(top[0].strategy, "b"); // highest delta
        assert_eq!(top[1].strategy, "a");
    }

    #[test]
    fn strategy_tracker_harmful_strategies() {
        let mut tracker = StrategyTracker::new(50);
        let h1 = tracker.begin_strategy("good", "good move", 0.5);
        tracker.complete_strategy(h1, 0.7);
        let h2 = tracker.begin_strategy("bad", "bad move", 0.7);
        tracker.complete_strategy(h2, 0.5);

        let harmful = tracker.harmful_strategies();
        assert_eq!(harmful.len(), 1);
        assert_eq!(harmful[0].strategy, "bad");
        assert!(!harmful[0].beneficial);
    }

    #[test]
    fn strategy_tracker_recommend() {
        let mut tracker = StrategyTracker::new(50);
        assert!(tracker.recommend().is_none());

        let h1 = tracker.begin_strategy("small_win", "desc", 0.5);
        tracker.complete_strategy(h1, 0.55);
        let h2 = tracker.begin_strategy("big_win", "desc", 0.55);
        tracker.complete_strategy(h2, 0.85);

        assert_eq!(tracker.recommend(), Some("big_win".to_string()));
    }

    #[test]
    fn strategy_tracker_recommend_ignores_harmful() {
        let mut tracker = StrategyTracker::new(50);
        let h = tracker.begin_strategy("harmful", "desc", 0.5);
        tracker.complete_strategy(h, 0.3);
        // Only harmful strategies => no recommendation.
        assert!(tracker.recommend().is_none());
    }

    #[test]
    fn strategy_tracker_eviction() {
        let mut tracker = StrategyTracker::new(2);
        let h1 = tracker.begin_strategy("first", "d", 0.1);
        tracker.complete_strategy(h1, 0.2);
        let h2 = tracker.begin_strategy("second", "d", 0.2);
        tracker.complete_strategy(h2, 0.3);
        let h3 = tracker.begin_strategy("third", "d", 0.3);
        tracker.complete_strategy(h3, 0.4);

        assert_eq!(tracker.len(), 2);
        assert_eq!(tracker.outcomes()[0].strategy, "second");
        assert_eq!(tracker.outcomes()[1].strategy, "third");
    }

    // ── TickHistory tests (Item 6) ──────────────────────────────

    #[test]
    fn tick_history_record_and_len() {
        let mut history = TickHistory::new(10);
        assert!(history.is_empty());
        history.record(CognitiveTickResult {
            tick_number: 0,
            elapsed_ms: 10,
            budget_ms: 50,
            ..Default::default()
        });
        assert_eq!(history.len(), 1);
    }

    #[test]
    fn tick_history_eviction() {
        let mut history = TickHistory::new(3);
        for i in 0..5 {
            history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 10,
                budget_ms: 50,
                ..Default::default()
            });
        }
        assert_eq!(history.len(), 3);
        assert_eq!(history.all().front().unwrap().tick_number, 2);
    }

    #[test]
    fn tick_history_changes_per_minute() {
        let mut history = TickHistory::new(100);
        // 10 ticks, each 100ms elapsed, each with 1 git commit.
        for i in 0..10 {
            history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 100,
                budget_ms: 200,
                git_commits_found: 1,
                files_changed: 0,
                ..Default::default()
            });
        }
        // Total: 10 changes in 1000ms = 1 second.
        // Changes per minute = 10 / (1000/60000) = 600.
        let cpm = history.changes_per_minute();
        assert!(cpm > 100.0, "expected high cpm, got {cpm}");
    }

    #[test]
    fn tick_history_changes_per_minute_no_changes() {
        let mut history = TickHistory::new(100);
        for i in 0..10 {
            history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 100,
                budget_ms: 200,
                ..Default::default()
            });
        }
        assert_eq!(history.changes_per_minute(), 0.0);
    }

    #[test]
    fn tick_history_changes_per_minute_insufficient_data() {
        let history = TickHistory::new(10);
        assert_eq!(history.changes_per_minute(), 0.0);

        let mut history2 = TickHistory::new(10);
        history2.record(CognitiveTickResult::default());
        assert_eq!(history2.changes_per_minute(), 0.0);
    }

    #[test]
    fn tick_history_avg_budget_usage() {
        let mut history = TickHistory::new(100);
        // 50% usage each tick.
        for i in 0..5 {
            history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 50,
                budget_ms: 100,
                ..Default::default()
            });
        }
        let usage = history.avg_budget_usage();
        assert!((usage - 0.5).abs() < 0.01);
    }

    #[test]
    fn tick_history_avg_budget_usage_empty() {
        let history = TickHistory::new(10);
        assert_eq!(history.avg_budget_usage(), 0.0);
    }

    #[test]
    fn tick_history_idle_ticks() {
        let mut history = TickHistory::new(100);
        // 3 active ticks followed by 5 idle ticks.
        for i in 0..3 {
            history.record(CognitiveTickResult {
                tick_number: i,
                git_commits_found: 1,
                ..Default::default()
            });
        }
        for i in 3..8 {
            history.record(CognitiveTickResult {
                tick_number: i,
                ..Default::default()
            });
        }
        assert_eq!(history.idle_ticks(), 5);
    }

    #[test]
    fn tick_history_idle_ticks_none_idle() {
        let mut history = TickHistory::new(100);
        history.record(CognitiveTickResult {
            tick_number: 0,
            files_changed: 1,
            ..Default::default()
        });
        assert_eq!(history.idle_ticks(), 0);
    }

    // ── TickRecommendation tests (Item 6) ────────────────────────

    #[test]
    fn tick_recommend_insufficient_data() {
        let engine = make_engine_mut();
        let rec = engine.recommend_tick_interval();
        assert_eq!(rec.recommended_ms, engine.current_tick_interval_ms);
        assert!(rec.reason.contains("Insufficient"));
        assert!(rec.recommendation_confidence < 0.5);
    }

    #[test]
    fn tick_recommend_idle_mode() {
        let mut engine = make_engine_mut();
        // Fill with 110 idle ticks.
        for i in 0..110 {
            engine.tick_history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 10,
                budget_ms: 100,
                ..Default::default()
            });
        }
        let rec = engine.recommend_tick_interval();
        assert_eq!(rec.recommended_ms, 5000);
        assert!(rec.reason.contains("idle"));
    }

    #[test]
    fn tick_recommend_high_frequency() {
        let mut engine = make_engine_mut();
        // 20 ticks, each 100ms, each with 5 git commits.
        for i in 0..20 {
            engine.tick_history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 100,
                budget_ms: 200,
                git_commits_found: 5,
                ..Default::default()
            });
        }
        let rec = engine.recommend_tick_interval();
        assert_eq!(rec.recommended_ms, 200);
        assert!(rec.changes_per_minute > 10.0);
    }

    #[test]
    fn tick_recommend_low_frequency() {
        let mut engine = make_engine_mut();
        // 20 ticks, each 6000ms (6s), 1 change total.
        for i in 0..20 {
            engine.tick_history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 6000,
                budget_ms: 10000,
                git_commits_found: if i == 0 { 1 } else { 0 },
                ..Default::default()
            });
        }
        let rec = engine.recommend_tick_interval();
        assert_eq!(rec.recommended_ms, 3000);
        assert!(rec.changes_per_minute < 1.0);
    }

    #[test]
    fn tick_recommend_moderate_frequency() {
        let mut engine = make_engine_mut();
        // 10 ticks, each 1000ms, each with 1 change => ~60 cpm, but
        // we need a moderate rate. Let's do 1 change per 10s.
        for i in 0..10 {
            engine.tick_history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 10000,
                budget_ms: 15000,
                git_commits_found: if i % 5 == 0 { 1 } else { 0 },
                files_changed: if i % 3 == 0 { 1 } else { 0 },
                ..Default::default()
            });
        }
        let rec = engine.recommend_tick_interval();
        // With ~6 changes in 100s => ~3.6 cpm => moderate.
        assert!(
            rec.recommended_ms == 1000,
            "expected 1000ms for moderate, got {}ms (cpm={:.2})",
            rec.recommended_ms,
            rec.changes_per_minute
        );
    }

    // ── Finding #7: TickIntervalModel wiring ──────────────────────

    #[test]
    fn tick_recommend_untrained_model_reproduces_step_function() {
        // Finding #7: installing an untrained TickIntervalModel must
        // not change the recommendation across all four step-function
        // tiers (idle / fast / moderate / slow / insufficient-data).
        for case in 0..5 {
            let mut baseline = make_engine_mut();
            let mut wired = make_engine_mut();
            wired.set_tick_interval_model(crate::eml_kernel::TickIntervalModel::new());

            // Drive the same tick_history into both engines.
            match case {
                0 => { /* leave empty for insufficient-data tier */ }
                1 => {
                    // Idle.
                    for i in 0..110 {
                        let res = CognitiveTickResult {
                            tick_number: i,
                            elapsed_ms: 10,
                            budget_ms: 100,
                            ..Default::default()
                        };
                        baseline.tick_history.record(res.clone());
                        wired.tick_history.record(res);
                    }
                }
                2 => {
                    // High frequency.
                    for i in 0..20 {
                        let res = CognitiveTickResult {
                            tick_number: i,
                            elapsed_ms: 100,
                            budget_ms: 200,
                            git_commits_found: 5,
                            ..Default::default()
                        };
                        baseline.tick_history.record(res.clone());
                        wired.tick_history.record(res);
                    }
                }
                3 => {
                    // Moderate.
                    for i in 0..10 {
                        let res = CognitiveTickResult {
                            tick_number: i,
                            elapsed_ms: 10000,
                            budget_ms: 15000,
                            git_commits_found: if i % 5 == 0 { 1 } else { 0 },
                            files_changed: if i % 3 == 0 { 1 } else { 0 },
                            ..Default::default()
                        };
                        baseline.tick_history.record(res.clone());
                        wired.tick_history.record(res);
                    }
                }
                _ => {
                    // Slow.
                    for i in 0..20 {
                        let res = CognitiveTickResult {
                            tick_number: i,
                            elapsed_ms: 6000,
                            budget_ms: 10000,
                            git_commits_found: if i == 0 { 1 } else { 0 },
                            ..Default::default()
                        };
                        baseline.tick_history.record(res.clone());
                        wired.tick_history.record(res);
                    }
                }
            }

            let b = baseline.recommend_tick_interval();
            let w = wired.recommend_tick_interval();
            assert_eq!(
                b.recommended_ms, w.recommended_ms,
                "untrained model must not change tier-{case} recommendation"
            );
        }
    }

    #[test]
    fn tick_recommend_trained_model_overrides_step_function() {
        // Finding #7: with a trained TickIntervalModel installed, the
        // recommendation comes from the model, not the step function.
        let model = crate::eml_kernel::TickIntervalModel::new();
        let mut json = serde_json::to_value(&model).unwrap();
        if let Some(inner) = json.get_mut("inner").and_then(|v| v.as_object_mut()) {
            inner.insert("trained".into(), serde_json::Value::Bool(true));
        }
        let forced: crate::eml_kernel::TickIntervalModel = serde_json::from_value(json).unwrap();
        assert!(forced.is_trained());

        let mut engine = make_engine_mut();
        // Drive a clear "fast" tier so the step-function would say 200ms.
        for i in 0..20 {
            engine.tick_history.record(CognitiveTickResult {
                tick_number: i,
                elapsed_ms: 100,
                budget_ms: 200,
                git_commits_found: 5,
                ..Default::default()
            });
        }
        let baseline = engine.recommend_tick_interval();
        assert_eq!(baseline.recommended_ms, 200);

        engine.set_tick_interval_model(forced);
        let with_model = engine.recommend_tick_interval();
        // The reason string explicitly identifies the EML branch.
        assert!(with_model.reason.contains("EML tick-interval model"));
        // The trained model should produce a different value (proves
        // the dispatch fired).
        assert_ne!(with_model.recommended_ms, baseline.recommended_ms);
        // Clamped to [100, 60_000].
        assert!((100..=60_000).contains(&with_model.recommended_ms));
    }

    // ── Integration: on_tick records tick_history ─────────────────

    #[test]
    fn on_tick_records_tick_history() {
        let mut engine = make_engine_mut();
        engine.start_session("hist", None, None).unwrap();
        engine.on_tick(100);
        engine.on_tick(100);
        assert_eq!(engine.tick_history().len(), 2);
    }

    // ── Integration: on_tick records confidence snapshot ──────────

    #[test]
    fn on_tick_records_confidence_snapshot_on_periodic() {
        let mut engine = make_engine_mut();
        engine.start_session("snap", None, None).unwrap();
        engine.ticks_since_confidence_update = 101;
        engine.on_tick(1000);
        assert!(
            !engine.confidence_history().is_empty(),
            "should have recorded a confidence snapshot"
        );
        let snap = engine.confidence_history().latest().unwrap();
        assert!(matches!(snap.trigger, ConfidenceTrigger::Periodic));
    }

    // ── Integration: ingest_graph_file_tracked ───────────────────

    #[test]
    fn ingest_graph_file_tracked_records_strategy_and_snapshot() {
        let dir = std::env::temp_dir().join("weaver_test_tracked");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("small.json");
        std::fs::write(
            &path,
            r#"{"source":"test","nodes":[{"id":"n1","title":"Node 1"}],"edges":[]}"#,
        )
        .unwrap();

        let mut engine = make_engine_mut();
        let result = engine.ingest_graph_file_tracked(&path).unwrap();
        assert_eq!(result.nodes_added, 1);

        // Strategy tracker should have one outcome.
        assert_eq!(engine.strategy_tracker().len(), 1);
        assert_eq!(
            engine.strategy_tracker().outcomes()[0].strategy,
            "ingest:small"
        );

        // Confidence history should have at least one PostIngestion snapshot.
        assert!(!engine.confidence_history().is_empty());
        let has_post_ingestion = engine
            .confidence_history()
            .all()
            .iter()
            .any(|s| matches!(s.trigger, ConfidenceTrigger::PostIngestion));
        assert!(has_post_ingestion);

        std::fs::remove_dir_all(&dir).ok();
    }

    // ── ConfidenceSnapshot / StrategyOutcome serde tests ─────────

    #[test]
    fn confidence_snapshot_serde_roundtrip() {
        let snap = ConfidenceSnapshot {
            timestamp: Utc::now(),
            tick_number: 42,
            confidence: 0.78,
            node_count: 100,
            edge_count: 200,
            gap_count: 3,
            trigger: ConfidenceTrigger::PostIngestion,
        };
        let json = serde_json::to_string(&snap).unwrap();
        let restored: ConfidenceSnapshot = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.tick_number, 42);
        assert!((restored.confidence - 0.78).abs() < 0.001);
    }

    #[test]
    fn strategy_outcome_serde_roundtrip() {
        let outcome = StrategyOutcome {
            strategy: "add_git".to_string(),
            description: "desc".to_string(),
            confidence_before: 0.4,
            confidence_after: 0.6,
            delta: 0.2,
            timestamp: Utc::now(),
            beneficial: true,
        };
        let json = serde_json::to_string(&outcome).unwrap();
        let restored: StrategyOutcome = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.strategy, "add_git");
        assert!(restored.beneficial);
    }

    #[test]
    fn tick_recommendation_serde_roundtrip() {
        let rec = TickRecommendation {
            recommended_ms: 200,
            current_ms: 1000,
            reason: "fast".to_string(),
            changes_per_minute: 50.0,
            recommendation_confidence: 0.8,
        };
        let json = serde_json::to_string(&rec).unwrap();
        let restored: TickRecommendation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.recommended_ms, 200);
    }

    // ── diff_models tests ───────────────────────────────────────

    fn make_model(domain: &str, confidence: f64) -> ExportedModel {
        ExportedModel {
            version: "1.0".into(),
            domain: domain.into(),
            exported_at: Utc::now(),
            confidence,
            node_types: vec![],
            edge_types: vec![],
            causal_nodes: vec![],
            causal_edges: vec![],
            metadata: HashMap::new(),
        }
    }

    #[test]
    fn diff_models_identical_produces_empty_diff() {
        let a = make_model("alpha", 0.8);
        let b = a.clone();
        let diff = diff_models(&a, &b);
        assert!(diff.nodes_only_a.is_empty());
        assert!(diff.nodes_only_b.is_empty());
        assert!(diff.edges_only_a.is_empty());
        assert!(diff.edges_only_b.is_empty());
        assert_eq!(diff.causal_nodes_added, 0);
        assert_eq!(diff.causal_nodes_removed, 0);
        assert_eq!(diff.causal_edges_added, 0);
        assert_eq!(diff.causal_edges_removed, 0);
        assert_eq!(diff.summary, "models are identical");
    }

    #[test]
    fn diff_models_different_node_types_detected() {
        let mut a = make_model("alpha", 0.5);
        a.node_types.push(NodeTypeSpec {
            name: "module".into(),
            embedding_strategy: "hash".into(),
            dimensions: 64,
        });
        a.node_types.push(NodeTypeSpec {
            name: "shared".into(),
            embedding_strategy: "hash".into(),
            dimensions: 64,
        });

        let mut b = make_model("beta", 0.5);
        b.node_types.push(NodeTypeSpec {
            name: "commit".into(),
            embedding_strategy: "hash".into(),
            dimensions: 64,
        });
        b.node_types.push(NodeTypeSpec {
            name: "shared".into(),
            embedding_strategy: "hash".into(),
            dimensions: 64,
        });

        let diff = diff_models(&a, &b);
        assert_eq!(diff.nodes_only_a, vec!["module"]);
        assert_eq!(diff.nodes_only_b, vec!["commit"]);
        assert_eq!(diff.nodes_common, vec!["shared"]);
    }

    #[test]
    fn diff_models_causal_additions_removals_counted() {
        let mut a = make_model("alpha", 0.5);
        a.causal_nodes.push(ExportedCausalNode {
            label: "A".into(),
            metadata: serde_json::json!({}),
        });
        a.causal_nodes.push(ExportedCausalNode {
            label: "shared".into(),
            metadata: serde_json::json!({}),
        });

        let mut b = make_model("beta", 0.5);
        b.causal_nodes.push(ExportedCausalNode {
            label: "B".into(),
            metadata: serde_json::json!({}),
        });
        b.causal_nodes.push(ExportedCausalNode {
            label: "shared".into(),
            metadata: serde_json::json!({}),
        });

        let diff = diff_models(&a, &b);
        assert_eq!(diff.causal_nodes_added, 1); // B
        assert_eq!(diff.causal_nodes_removed, 1); // A
    }

    #[test]
    fn diff_models_causal_edge_changes() {
        let mut a = make_model("alpha", 0.5);
        a.causal_edges.push(ExportedCausalEdge {
            source_label: "X".into(),
            target_label: "Y".into(),
            edge_type: "Causes".into(),
            weight: 1.0,
        });

        let mut b = make_model("beta", 0.5);
        b.causal_edges.push(ExportedCausalEdge {
            source_label: "Y".into(),
            target_label: "Z".into(),
            edge_type: "Enables".into(),
            weight: 0.5,
        });

        let diff = diff_models(&a, &b);
        assert_eq!(diff.causal_edges_added, 1);
        assert_eq!(diff.causal_edges_removed, 1);
    }

    #[test]
    fn diff_models_confidence_delta_in_summary() {
        let a = make_model("alpha", 0.5);
        let b = make_model("beta", 0.8);
        let diff = diff_models(&a, &b);
        assert!((diff.confidence_delta - 0.3).abs() < 1e-10);
        assert!(diff.summary.contains("increased"));
    }

    #[test]
    fn diff_models_summary_shows_decrease() {
        let a = make_model("alpha", 0.9);
        let b = make_model("beta", 0.6);
        let diff = diff_models(&a, &b);
        assert!(diff.summary.contains("decreased"));
    }

    #[test]
    fn diff_models_edge_type_differences() {
        let mut a = make_model("a", 0.5);
        a.edge_types.push(EdgeTypeSpec {
            from_type: "mod".into(),
            to_type: "mod".into(),
            edge_type: "uses".into(),
            confidence: 0.8,
        });

        let mut b = make_model("b", 0.5);
        b.edge_types.push(EdgeTypeSpec {
            from_type: "mod".into(),
            to_type: "test".into(),
            edge_type: "tests".into(),
            confidence: 0.7,
        });

        let diff = diff_models(&a, &b);
        assert_eq!(diff.edges_only_a.len(), 1);
        assert_eq!(diff.edges_only_b.len(), 1);
        assert!(diff.edges_common.is_empty());
    }

    // ── merge_models tests ──────────────────────────────────────

    #[test]
    fn merge_models_disjoint_produces_union() {
        let mut a = make_model("alpha", 0.6);
        a.node_types.push(NodeTypeSpec {
            name: "mod_a".into(),
            embedding_strategy: "hash".into(),
            dimensions: 64,
        });
        a.causal_nodes.push(ExportedCausalNode {
            label: "A1".into(),
            metadata: serde_json::json!({}),
        });

        let mut b = make_model("beta", 0.8);
        b.node_types.push(NodeTypeSpec {
            name: "mod_b".into(),
            embedding_strategy: "hash".into(),
            dimensions: 64,
        });
        b.causal_nodes.push(ExportedCausalNode {
            label: "B1".into(),
            metadata: serde_json::json!({}),
        });

        let result = merge_models(&a, &b);
        assert_eq!(result.stats.total_node_types, 2);
        assert_eq!(result.stats.total_causal_nodes, 2);
        assert_eq!(result.stats.nodes_from_a, 1);
        assert_eq!(result.stats.nodes_from_b, 1);
        assert_eq!(result.stats.nodes_shared, 0);
        assert_eq!(result.conflicts.len(), 0);
    }

    #[test]
    fn merge_models_overlapping_nodes_higher_confidence() {
        let mut a = make_model("alpha", 0.6);
        a.node_types.push(NodeTypeSpec {
            name: "shared".into(),
            embedding_strategy: "hash_v1".into(),
            dimensions: 64,
        });

        let mut b = make_model("beta", 0.8);
        b.node_types.push(NodeTypeSpec {
            name: "shared".into(),
            embedding_strategy: "hash_v2".into(),
            dimensions: 128,
        });

        let result = merge_models(&a, &b);
        assert_eq!(result.stats.nodes_shared, 1);
        assert_eq!(result.conflicts.len(), 1);
        // B has higher dimensions so KeepB.
        assert_eq!(result.conflicts[0].resolution, ConflictResolution::KeepB);
        assert_eq!(result.merged.node_types[0].embedding_strategy, "hash_v2");
    }

    #[test]
    fn merge_models_causal_edges_merged_by_id() {
        let mut a = make_model("alpha", 0.5);
        a.causal_edges.push(ExportedCausalEdge {
            source_label: "X".into(),
            target_label: "Y".into(),
            edge_type: "Causes".into(),
            weight: 1.0,
        });

        let mut b = make_model("beta", 0.5);
        b.causal_edges.push(ExportedCausalEdge {
            source_label: "X".into(),
            target_label: "Y".into(),
            edge_type: "Causes".into(),
            weight: 0.5,
        });

        let result = merge_models(&a, &b);
        assert_eq!(result.stats.total_causal_edges, 1);
        // Weight should be averaged: (1.0 + 0.5) / 2.0 = 0.75
        assert!((result.merged.causal_edges[0].weight - 0.75).abs() < 1e-5);
    }

    #[test]
    fn merge_models_conflict_resolution_recorded() {
        let mut a = make_model("alpha", 0.5);
        a.edge_types.push(EdgeTypeSpec {
            from_type: "m".into(),
            to_type: "m".into(),
            edge_type: "uses".into(),
            confidence: 0.3,
        });

        let mut b = make_model("beta", 0.5);
        b.edge_types.push(EdgeTypeSpec {
            from_type: "m".into(),
            to_type: "m".into(),
            edge_type: "uses".into(),
            confidence: 0.9,
        });

        let result = merge_models(&a, &b);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(
            result.conflicts[0].resolution,
            ConflictResolution::HigherConfidence
        );
        // The higher confidence edge (0.9) should win.
        assert!((result.merged.edge_types[0].confidence - 0.9).abs() < 1e-10);
    }

    #[test]
    fn merge_models_confidence_is_weighted_average() {
        let mut a = make_model("alpha", 0.4);
        a.causal_nodes.push(ExportedCausalNode {
            label: "A1".into(),
            metadata: serde_json::json!({}),
        });
        // A has 1 node

        let mut b = make_model("beta", 0.8);
        b.causal_nodes.push(ExportedCausalNode {
            label: "B1".into(),
            metadata: serde_json::json!({}),
        });
        b.causal_nodes.push(ExportedCausalNode {
            label: "B2".into(),
            metadata: serde_json::json!({}),
        });
        b.causal_nodes.push(ExportedCausalNode {
            label: "B3".into(),
            metadata: serde_json::json!({}),
        });
        // B has 3 nodes

        let result = merge_models(&a, &b);
        // Weighted: (0.4*1 + 0.8*3) / (1+3) = 2.8/4 = 0.7
        assert!((result.merged.confidence - 0.7).abs() < 1e-10);
    }

    #[test]
    fn merge_models_domain_combined() {
        let a = make_model("alpha", 0.5);
        let b = make_model("beta", 0.5);
        let result = merge_models(&a, &b);
        assert_eq!(result.merged.domain, "alpha+beta");
    }

    #[test]
    fn merge_models_causal_edge_type_conflict() {
        let mut a = make_model("a", 0.5);
        a.causal_edges.push(ExportedCausalEdge {
            source_label: "X".into(),
            target_label: "Y".into(),
            edge_type: "Causes".into(),
            weight: 1.0,
        });

        let mut b = make_model("b", 0.5);
        b.causal_edges.push(ExportedCausalEdge {
            source_label: "X".into(),
            target_label: "Y".into(),
            edge_type: "Enables".into(),
            weight: 0.5,
        });

        let result = merge_models(&a, &b);
        assert!(result.conflicts.iter().any(|c| {
            c.item.starts_with("causal_edge:") && c.resolution == ConflictResolution::Merged
        }));
    }

    // ── knowledge_base persistence tests ────────────────────────

    #[test]
    fn knowledge_base_save_load_roundtrip() {
        let dir = std::env::temp_dir().join("weaver_kb_test_roundtrip");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("kb.json");

        let kb = WeaverKnowledgeBase::new();
        kb.record_strategy(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust-project".into(),
            improvement: 0.15,
            timestamp: Utc::now(),
        });
        kb.record_strategy(StrategyPattern {
            decision_type: "EdgeType".into(),
            context: "python-project".into(),
            improvement: 0.25,
            timestamp: Utc::now(),
        });

        kb.save_to_file(&path).unwrap();
        let loaded = WeaverKnowledgeBase::load_from_file(&path).unwrap();
        assert_eq!(loaded.pattern_count(), 2);

        let strategies = loaded.list_strategies();
        assert!(
            strategies
                .iter()
                .any(|s| s.decision_type == "SourceAdded" && s.context == "rust-project")
        );
        assert!(
            strategies
                .iter()
                .any(|s| s.decision_type == "EdgeType" && s.context == "python-project")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn knowledge_base_learn_pattern_adds_new() {
        let kb = WeaverKnowledgeBase::new();
        kb.learn_pattern(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust".into(),
            improvement: 0.1,
            timestamp: Utc::now(),
        });
        assert_eq!(kb.pattern_count(), 1);

        kb.learn_pattern(StrategyPattern {
            decision_type: "EdgeType".into(),
            context: "python".into(),
            improvement: 0.2,
            timestamp: Utc::now(),
        });
        assert_eq!(kb.pattern_count(), 2);
    }

    #[test]
    fn knowledge_base_learn_pattern_updates_existing() {
        let kb = WeaverKnowledgeBase::new();
        kb.learn_pattern(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust".into(),
            improvement: 0.1,
            timestamp: Utc::now(),
        });

        // Same decision_type + context should update, not add.
        kb.learn_pattern(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust".into(),
            improvement: 0.3,
            timestamp: Utc::now(),
        });

        assert_eq!(kb.pattern_count(), 1);
        let strategies = kb.list_strategies();
        // Average of 0.1 and 0.3 = 0.2
        assert!((strategies[0].improvement - 0.2).abs() < 1e-10);
    }

    #[test]
    fn knowledge_base_find_patterns_returns_matching() {
        let kb = WeaverKnowledgeBase::new();
        kb.learn_pattern(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust-backend".into(),
            improvement: 0.1,
            timestamp: Utc::now(),
        });
        kb.learn_pattern(StrategyPattern {
            decision_type: "EdgeType".into(),
            context: "python-ml".into(),
            improvement: 0.2,
            timestamp: Utc::now(),
        });
        kb.learn_pattern(StrategyPattern {
            decision_type: "Tick".into(),
            context: "go-service".into(),
            improvement: 0.15,
            timestamp: Utc::now(),
        });

        let matches = kb.find_patterns(&["rust".to_string()]);
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].context, "rust-backend");
    }

    /// `find_patterns` ranks by how many context keywords a pattern's context
    /// contains. It is a keyword-count API with no embedder involved, so this
    /// test is about keyword hit counts and says nothing about semantic recall.
    #[test]
    fn knowledge_base_find_patterns_ranks_by_keyword_hit_count() {
        let kb = WeaverKnowledgeBase::new();
        kb.learn_pattern(StrategyPattern {
            decision_type: "A".into(),
            context: "rust".into(),
            improvement: 0.1,
            timestamp: Utc::now(),
        });
        kb.learn_pattern(StrategyPattern {
            decision_type: "B".into(),
            context: "rust-backend-api".into(),
            improvement: 0.2,
            timestamp: Utc::now(),
        });
        kb.learn_pattern(StrategyPattern {
            decision_type: "C".into(),
            context: "python".into(),
            improvement: 0.3,
            timestamp: Utc::now(),
        });

        let matches =
            kb.find_patterns(&["rust".to_string(), "backend".to_string(), "api".to_string()]);
        // B should rank first (matches rust, backend, api = 3 hits).
        // A should rank second (matches rust = 1 hit).
        // C should not appear.
        assert_eq!(matches.len(), 2);
        assert_eq!(matches[0].decision_type, "B");
        assert_eq!(matches[1].decision_type, "A");
    }

    #[test]
    fn knowledge_base_empty_handles_gracefully() {
        let kb = WeaverKnowledgeBase::new();
        assert_eq!(kb.pattern_count(), 0);
        assert!(kb.find_patterns(&["rust".to_string()]).is_empty());
        assert!(kb.list_strategies().is_empty());

        // save/load empty KB.
        let dir = std::env::temp_dir().join("weaver_kb_empty_test");
        std::fs::create_dir_all(&dir).ok();
        let path = dir.join("empty-kb.json");
        kb.save_to_file(&path).unwrap();
        let loaded = WeaverKnowledgeBase::load_from_file(&path).unwrap();
        assert_eq!(loaded.pattern_count(), 0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn knowledge_base_serializable_kb_fields() {
        let kb = WeaverKnowledgeBase::new();
        kb.record_strategy(StrategyPattern {
            decision_type: "SourceAdded".into(),
            context: "rust".into(),
            improvement: 0.1,
            timestamp: Utc::now(),
        });
        let ser = kb.to_serializable();
        assert_eq!(ser.version, 1);
        assert_eq!(ser.patterns.len(), 1);
        assert!(ser.domains_modeled.contains(&"rust".to_string()));
    }

    #[test]
    fn knowledge_base_load_nonexistent_file_errors() {
        let result = WeaverKnowledgeBase::load_from_file(Path::new("/nonexistent/path/kb.json"));
        assert!(result.is_err());
    }

    #[test]
    fn knowledge_base_find_patterns_no_match_returns_empty() {
        let kb = WeaverKnowledgeBase::new();
        kb.learn_pattern(StrategyPattern {
            decision_type: "X".into(),
            context: "rust".into(),
            improvement: 0.1,
            timestamp: Utc::now(),
        });
        let matches = kb.find_patterns(&["java".to_string()]);
        assert!(matches.is_empty());
    }
}
