//! Pipeline orchestrator: detect -> extract -> build -> cluster -> analyze.
//!
//! Ported from Python `graphify/pipeline.py`.

use crate::GraphifyError;
use crate::analyze;
use crate::build::MergeStats;
use crate::cluster;
use crate::entity::EntityId;
use crate::hyperedge::{self, HyperedgeDiscoveryConfig};
use crate::model::{
    DetectionResult, Entity, ExtractionResult, ExtractionStats, Hyperedge, KnowledgeGraph,
};
use crate::relationship::Relationship;
use crate::summary;
use std::collections::HashMap;
use std::path::Path;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Pipeline configuration.
#[derive(Debug, Clone)]
pub struct PipelineConfig {
    /// Whether to run community detection.
    pub cluster: bool,
    /// Whether to run analysis (god nodes, surprises, questions).
    pub analyze: bool,
    /// Whether to run WEFT-377 hyperedge discovery after graph build.
    pub discover_hyperedges: bool,
    /// Hyperedge discovery knobs (used when `discover_hyperedges` is true).
    pub hyperedge_discovery: HyperedgeDiscoveryConfig,
    /// Export formats to generate.
    pub exports: Vec<String>,
    /// Maximum number of god nodes to report.
    pub god_nodes_top_n: usize,
    /// Maximum surprising connections.
    pub surprises_top_n: usize,
    /// Maximum questions.
    pub questions_top_n: usize,
    /// Cache directory (optional).
    pub cache_dir: Option<String>,
    /// Domain hint.
    pub domain: Option<String>,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            cluster: true,
            analyze: true,
            discover_hyperedges: true,
            hyperedge_discovery: HyperedgeDiscoveryConfig::default(),
            exports: vec!["json".to_owned()],
            god_nodes_top_n: 10,
            surprises_top_n: 5,
            questions_top_n: 7,
            cache_dir: None,
            domain: None,
        }
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

/// Results of the full analysis pipeline.
#[derive(Debug, Clone)]
pub struct AnalysisResult {
    pub god_nodes: Vec<analyze::GodNode>,
    pub surprising_connections: Vec<analyze::SurprisingConnection>,
    pub questions: Vec<analyze::SuggestedQuestion>,
    pub communities: HashMap<usize, Vec<EntityId>>,
    pub community_labels: HashMap<usize, String>,
    pub cohesion_scores: HashMap<usize, f64>,
    pub community_summaries: HashMap<usize, summary::CommunitySummary>,
}

/// Full pipeline result.
#[derive(Debug)]
pub struct PipelineResult {
    pub graph: KnowledgeGraph,
    pub analysis: Option<AnalysisResult>,
    pub stats: ExtractionStats,
    pub detection: DetectionResult,
    /// Populated only for incremental runs.
    pub merge_stats: Option<MergeStats>,
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Chain event kind for pipeline completion.
pub const EVENT_KIND_GRAPHIFY_PIPELINE: &str = "graphify.pipeline";

/// The full graphify pipeline.
pub struct Pipeline {
    config: PipelineConfig,
}

impl Pipeline {
    pub fn new(config: PipelineConfig) -> Self {
        Self { config }
    }

    /// Run the full pipeline on pre-extracted results.
    ///
    /// This is the Phase 2 entry point: takes already-extracted entities/relationships
    /// and runs build -> cluster -> analyze. Phase 1 will add the detect -> extract
    /// steps with tree-sitter and LLM extraction.
    pub fn run_from_extractions(
        &self,
        extractions: Vec<ExtractionResult>,
        detection: DetectionResult,
    ) -> Result<PipelineResult, GraphifyError> {
        // Aggregate entities, relationships, hyperedges from all extractions
        let mut all_entities: Vec<Entity> = Vec::new();
        let mut all_relationships: Vec<Relationship> = Vec::new();
        let mut all_hyperedges: Vec<Hyperedge> = Vec::new();
        let mut stats = ExtractionStats::default();

        for result in &extractions {
            all_entities.extend(result.entities.clone());
            all_relationships.extend(result.relationships.clone());
            all_hyperedges.extend(result.hyperedges.clone());
            stats.files_processed += 1;
            stats.entities_extracted += result.entities.len();
            stats.relationships_extracted += result.relationships.len();
            stats.input_tokens += result.input_tokens;
            stats.output_tokens += result.output_tokens;
            if !result.errors.is_empty() {
                // Partial failure: count but don't abort
                tracing::warn!(
                    file = %result.source_file,
                    errors = ?result.errors,
                    "Extraction had partial failures"
                );
            }
        }

        // Build the graph
        let mut graph = KnowledgeGraph::from_parts(all_entities, all_relationships, all_hyperedges);

        // WEFT-377: discover multi-node hyperedges (shared context, co-occurrence, schema)
        if self.config.discover_hyperedges {
            let added = hyperedge::apply_discovered_hyperedges(
                &mut graph,
                &self.config.hyperedge_discovery,
            );
            tracing::debug!(
                hyperedges_discovered = added,
                hyperedges_total = graph.hyperedges.len(),
                "Hyperedge discovery complete"
            );
        }

        // Cluster + analyze
        let analysis = if self.config.cluster || self.config.analyze {
            let communities = if self.config.cluster {
                cluster::cluster(&graph)
            } else {
                HashMap::new()
            };

            let community_labels = cluster::auto_label_all(&graph, &communities);
            let cohesion_scores = cluster::score_all(&graph, &communities);
            let community_summaries =
                summary::generate_community_summaries(&graph, &communities, &community_labels);

            let (god_nodes, surprising_connections, questions) = if self.config.analyze {
                let gn = analyze::god_nodes(&graph, self.config.god_nodes_top_n);
                let sc = analyze::surprising_connections(
                    &graph,
                    &communities,
                    self.config.surprises_top_n,
                );
                let qs = analyze::suggest_questions(
                    &graph,
                    &communities,
                    &community_labels,
                    self.config.questions_top_n,
                );
                (gn, sc, qs)
            } else {
                (vec![], vec![], vec![])
            };

            Some(AnalysisResult {
                god_nodes,
                surprising_connections,
                questions,
                communities,
                community_labels,
                cohesion_scores,
                community_summaries,
            })
        } else {
            None
        };

        // Chain event marker -- daemon subscriber forwards to ExoChain.
        tracing::info!(
            target: "chain_event",
            source = "graphify",
            kind = EVENT_KIND_GRAPHIFY_PIPELINE,
            entity_count = stats.entities_extracted,
            relationship_count = stats.relationships_extracted,
            files_processed = stats.files_processed,
            hyperedge_count = graph.hyperedges.len(),
            has_analysis = analysis.is_some(),
            "chain"
        );

        Ok(PipelineResult {
            graph,
            analysis,
            stats,
            detection,
            merge_stats: None,
        })
    }

    /// Run incrementally: merge new extractions into an existing graph, then
    /// re-cluster and re-analyze.
    ///
    /// `existing_graph` is the previously-built graph (loaded from JSON).
    /// `new_extractions` are the extraction results for new/changed files only.
    /// `removed_files` are file paths that existed in the last run but have
    /// been deleted from disk.
    pub fn run_incremental(
        &self,
        mut existing_graph: KnowledgeGraph,
        new_extractions: Vec<ExtractionResult>,
        removed_files: &[String],
        detection: DetectionResult,
    ) -> Result<PipelineResult, GraphifyError> {
        // Aggregate stats from new extractions.
        let mut stats = ExtractionStats::default();
        for ext in &new_extractions {
            stats.files_processed += 1;
            stats.entities_extracted += ext.entities.len();
            stats.relationships_extracted += ext.relationships.len();
            stats.input_tokens += ext.input_tokens;
            stats.output_tokens += ext.output_tokens;
        }

        // Merge into existing graph.
        let merge_stats = crate::build::merge(&mut existing_graph, &new_extractions, removed_files);

        tracing::info!(
            entities_added = merge_stats.entities_added,
            entities_updated = merge_stats.entities_updated,
            entities_removed = merge_stats.entities_removed,
            relationships_added = merge_stats.relationships_added,
            relationships_removed = merge_stats.relationships_removed,
            "Incremental merge complete"
        );

        // WEFT-377: re-discover hyperedges on the merged graph.
        // Extraction hyperedges already merged by build::merge; discovery
        // re-runs from scratch and merges (existing extraction edges win on
        // entity-set collision via merge_hyperedges).
        if self.config.discover_hyperedges {
            // Drop previously discovered hyperedges so re-run is clean; keep
            // extraction-origin ones (no detector metadata / kind tags).
            existing_graph.hyperedges.retain(|h| {
                h.metadata
                    .get("detector")
                    .and_then(|v| v.as_str())
                    .map(|d| d != "weft-377")
                    .unwrap_or(true)
            });
            let added = hyperedge::apply_discovered_hyperedges(
                &mut existing_graph,
                &self.config.hyperedge_discovery,
            );
            tracing::debug!(
                hyperedges_discovered = added,
                hyperedges_total = existing_graph.hyperedges.len(),
                "Incremental hyperedge discovery complete"
            );
        }

        // Re-cluster + re-analyze on the merged graph.
        let analysis = if self.config.cluster || self.config.analyze {
            let communities = if self.config.cluster {
                cluster::cluster(&existing_graph)
            } else {
                HashMap::new()
            };

            let community_labels = cluster::auto_label_all(&existing_graph, &communities);
            let cohesion_scores = cluster::score_all(&existing_graph, &communities);
            let community_summaries = summary::generate_community_summaries(
                &existing_graph,
                &communities,
                &community_labels,
            );

            let (god_nodes, surprising_connections, questions) = if self.config.analyze {
                let gn = analyze::god_nodes(&existing_graph, self.config.god_nodes_top_n);
                let sc = analyze::surprising_connections(
                    &existing_graph,
                    &communities,
                    self.config.surprises_top_n,
                );
                let qs = analyze::suggest_questions(
                    &existing_graph,
                    &communities,
                    &community_labels,
                    self.config.questions_top_n,
                );
                (gn, sc, qs)
            } else {
                (vec![], vec![], vec![])
            };

            Some(AnalysisResult {
                god_nodes,
                surprising_connections,
                questions,
                communities,
                community_labels,
                cohesion_scores,
                community_summaries,
            })
        } else {
            None
        };

        // Update stats with totals from the merged graph.
        stats.entities_extracted = existing_graph.entity_count();
        stats.relationships_extracted = existing_graph.relationship_count();

        Ok(PipelineResult {
            graph: existing_graph,
            analysis,
            stats,
            detection,
            merge_stats: Some(merge_stats),
        })
    }

    /// Run the full pipeline on file paths (stub for Phase 1 integration).
    pub fn run(&self, _paths: &[&Path]) -> Result<PipelineResult, GraphifyError> {
        // Phase 1 will implement: detect files -> extract entities -> build graph
        // For now, return an empty result
        Err(GraphifyError::Pipeline(
            "File-based pipeline not yet implemented. Use run_from_extractions() instead."
                .to_owned(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::relationship::{Confidence, RelationType};

    fn extraction(entities: Vec<Entity>, rels: Vec<Relationship>) -> ExtractionResult {
        ExtractionResult {
            source_file: "test.py".to_owned(),
            entities,
            relationships: rels,
            hyperedges: vec![],
            input_tokens: 100,
            output_tokens: 50,
            errors: vec![],
        }
    }

    fn entity(name: &str, source_file: &str) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Function, name, source_file),
            entity_type: EntityType::Function,
            label: name.to_owned(),
            source_file: Some(source_file.to_owned()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        }
    }

    fn rel(src_name: &str, src_file: &str, tgt_name: &str, tgt_file: &str) -> Relationship {
        Relationship {
            source: EntityId::new(&DomainTag::Code, &EntityType::Function, src_name, src_file),
            target: EntityId::new(&DomainTag::Code, &EntityType::Function, tgt_name, tgt_file),
            relation_type: RelationType::Calls,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    #[test]
    fn pipeline_runs_from_extractions() {
        let pipeline = Pipeline::new(PipelineConfig::default());
        let ext = extraction(
            vec![entity("a", "a.py"), entity("b", "b.py")],
            vec![rel("a", "a.py", "b", "b.py")],
        );
        let detection = DetectionResult {
            total_files: 2,
            total_words: 500,
            warning: None,
        };
        let result = pipeline.run_from_extractions(vec![ext], detection).unwrap();
        assert_eq!(result.graph.node_count(), 2);
        assert_eq!(result.graph.edge_count(), 1);
        assert!(result.analysis.is_some());
        let analysis = result.analysis.unwrap();
        assert!(!analysis.communities.is_empty());
    }

    #[test]
    fn pipeline_stats_track_counts() {
        let pipeline = Pipeline::new(PipelineConfig::default());
        let ext1 = extraction(
            vec![entity("a", "a.py"), entity("b", "a.py")],
            vec![rel("a", "a.py", "b", "a.py")],
        );
        let ext2 = extraction(vec![entity("c", "c.py")], vec![]);
        let detection = DetectionResult::default();
        let result = pipeline
            .run_from_extractions(vec![ext1, ext2], detection)
            .unwrap();
        assert_eq!(result.stats.files_processed, 2);
        assert_eq!(result.stats.entities_extracted, 3);
        assert_eq!(result.stats.relationships_extracted, 1);
        assert_eq!(result.stats.input_tokens, 200);
    }

    #[test]
    fn pipeline_handles_partial_failures() {
        let pipeline = Pipeline::new(PipelineConfig::default());
        let mut ext = extraction(vec![entity("a", "a.py")], vec![]);
        ext.errors = vec!["parse error".to_owned()];
        let detection = DetectionResult::default();
        let result = pipeline.run_from_extractions(vec![ext], detection);
        assert!(result.is_ok());
    }

    #[test]
    fn pipeline_empty_extractions_ok() {
        let pipeline = Pipeline::new(PipelineConfig::default());
        let detection = DetectionResult::default();
        let result = pipeline.run_from_extractions(vec![], detection).unwrap();
        assert_eq!(result.graph.node_count(), 0);
    }

    /// WEFT-377: pipeline invokes discover_hyperedges on a canonical multi-node graph.
    #[test]
    fn pipeline_discovers_hyperedges() {
        // Three co-located functions + a dispatcher that calls all three.
        let f1 = entity("alpha", "mod.rs");
        let f2 = entity("beta", "mod.rs");
        let f3 = entity("gamma", "mod.rs");
        let dispatch = entity("dispatch", "dispatch.rs");
        let ext = extraction(
            vec![f1, f2, f3, dispatch],
            vec![
                rel("dispatch", "dispatch.rs", "alpha", "mod.rs"),
                rel("dispatch", "dispatch.rs", "beta", "mod.rs"),
                rel("dispatch", "dispatch.rs", "gamma", "mod.rs"),
            ],
        );
        let pipeline = Pipeline::new(PipelineConfig::default());
        let result = pipeline
            .run_from_extractions(vec![ext], DetectionResult::default())
            .unwrap();
        assert!(
            !result.graph.hyperedges.is_empty(),
            "expected pipeline to discover hyperedges"
        );
        // Shared context for mod.rs (3 functions) and/or co-occurrence star.
        assert!(
            result
                .graph
                .hyperedges
                .iter()
                .any(|h| h.entity_ids.len() >= 3)
        );
        assert!(
            result.graph.hyperedges.iter().any(|h| {
                h.metadata.get("detector").and_then(|v| v.as_str()) == Some("weft-377")
            })
        );
    }

    #[test]
    fn pipeline_can_disable_hyperedge_discovery() {
        let f1 = entity("alpha", "mod.rs");
        let f2 = entity("beta", "mod.rs");
        let f3 = entity("gamma", "mod.rs");
        let ext = extraction(vec![f1, f2, f3], vec![]);
        let config = PipelineConfig {
            discover_hyperedges: false,
            ..PipelineConfig::default()
        };
        let pipeline = Pipeline::new(config);
        let result = pipeline
            .run_from_extractions(vec![ext], DetectionResult::default())
            .unwrap();
        assert!(result.graph.hyperedges.is_empty());
    }
}
