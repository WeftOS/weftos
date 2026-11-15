//! Vault hyperedge pipeline: SUGGEST → ratify → CRDT apply (WEFT-378).
//!
//! ```text
//! vault nodes ──suggest_hyperedges──► CrdtHyperedgeStore (Suggested)
//!                                            │
//!                          ratify / reject   │  (human or agent gate)
//!                                            ▼
//!                                     Ratified / Rejected
//!                                            │
//!                          apply_to_graph    │
//!                                            ▼
//!                              KnowledgeGraph.hyperedges (+ Document entities)
//! ```
//!
//! Multi-replica: exchange [`CrdtDelta`](super::crdt::CrdtDelta) and
//! [`merge`](VaultHyperedgePipeline::merge). Ratify UI consumers use
//! [`RatifyView`] (serializable pending list) — panel wiring is cross-stream
//! (W-UI/15).

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use super::analyze::VaultNode;
use super::crdt::{CrdtDelta, CrdtError, CrdtHyperedgeStore, MergeStats};
use super::hyperedge::{
    HyperedgeProposal, HyperedgeSuggestConfig, ProposalLifecycle, suggest_hyperedges,
};
use crate::entity::{DomainTag, EntityId, EntityType, FileType};
use crate::model::{Entity, Hyperedge, KnowledgeGraph};

/// Domain discriminator for vault-sourced entities (0x22).
pub const VAULT_DOMAIN: DomainTag = DomainTag::Custom(0x22);

// ---------------------------------------------------------------------------
// Apply / ratify view
// ---------------------------------------------------------------------------

/// Result of materializing ratified hyperedges into a knowledge graph.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ApplyStats {
    /// Document entities added (paths not already present).
    pub entities_added: usize,
    /// Hyperedges appended to the graph.
    pub hyperedges_applied: usize,
    /// Ratified proposals skipped (already present by id in metadata).
    pub hyperedges_skipped: usize,
}

/// Serializable view for a user-facing ratify UI (W-UI/15).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RatifyView {
    pub actor: String,
    pub clock: u64,
    pub pending: Vec<HyperedgeProposal>,
    pub ratified_count: usize,
    pub rejected_count: usize,
    pub suggested_count: usize,
}

// ---------------------------------------------------------------------------
// Pipeline
// ---------------------------------------------------------------------------

/// Orchestrates vault hyperedge SUGGEST → ratify → CRDT apply.
#[derive(Debug, Clone)]
pub struct VaultHyperedgePipeline {
    store: CrdtHyperedgeStore,
    config: HyperedgeSuggestConfig,
}

impl VaultHyperedgePipeline {
    /// New pipeline for replica `actor`.
    pub fn new(actor: impl Into<String>) -> Self {
        Self {
            store: CrdtHyperedgeStore::new(actor),
            config: HyperedgeSuggestConfig::default(),
        }
    }

    pub fn with_config(actor: impl Into<String>, config: HyperedgeSuggestConfig) -> Self {
        Self {
            store: CrdtHyperedgeStore::new(actor),
            config,
        }
    }

    pub fn store(&self) -> &CrdtHyperedgeStore {
        &self.store
    }

    pub fn config(&self) -> &HyperedgeSuggestConfig {
        &self.config
    }

    pub fn config_mut(&mut self) -> &mut HyperedgeSuggestConfig {
        &mut self.config
    }

    // ----- SUGGEST ---------------------------------------------------------

    /// Run SUGGEST against vault nodes; write proposals into the CRDT store.
    ///
    /// Returns the proposals written (status = Suggested). Existing
    /// ratified/rejected ids are not overwritten.
    pub fn suggest(&mut self, nodes: &HashMap<String, VaultNode>) -> Vec<HyperedgeProposal> {
        let proposals = suggest_hyperedges(nodes, &self.config);
        self.store.put_suggestions(proposals.clone());
        proposals
    }

    // ----- Ratify gate -----------------------------------------------------

    /// Pending proposals (Suggested), highest score first.
    pub fn pending(&self) -> Vec<&HyperedgeProposal> {
        self.store.pending()
    }

    /// Ratify a proposal by id.
    pub fn ratify(&mut self, id: &str) -> Result<&HyperedgeProposal, CrdtError> {
        self.store.set_status(id, ProposalLifecycle::Ratified)
    }

    /// Reject a proposal by id.
    pub fn reject(&mut self, id: &str) -> Result<&HyperedgeProposal, CrdtError> {
        self.store.set_status(id, ProposalLifecycle::Rejected)
    }

    /// Auto-ratify all pending proposals with `confidence >= threshold`.
    pub fn ratify_above(&mut self, threshold: f64) -> usize {
        let ids: Vec<String> = self
            .store
            .pending()
            .into_iter()
            .filter(|p| p.confidence >= threshold)
            .map(|p| p.id.clone())
            .collect();
        let mut n = 0;
        for id in ids {
            if self.ratify(&id).is_ok() {
                n += 1;
            }
        }
        n
    }

    /// Snapshot for a ratify UI / agent panel.
    pub fn ratify_view(&self) -> RatifyView {
        let mut suggested = 0usize;
        let mut ratified = 0usize;
        let mut rejected = 0usize;
        for e in self.store.entries() {
            match e.proposal.status {
                ProposalLifecycle::Suggested => suggested += 1,
                ProposalLifecycle::Ratified => ratified += 1,
                ProposalLifecycle::Rejected => rejected += 1,
            }
        }
        RatifyView {
            actor: self.store.actor().to_string(),
            clock: self.store.clock(),
            pending: self.store.pending().into_iter().cloned().collect(),
            ratified_count: ratified,
            rejected_count: rejected,
            suggested_count: suggested,
        }
    }

    // ----- CRDT merge ------------------------------------------------------

    pub fn merge(&mut self, delta: &CrdtDelta) -> MergeStats {
        self.store.merge(delta)
    }

    pub fn delta_since(&self, since: u64) -> CrdtDelta {
        self.store.delta_since(since)
    }

    pub fn snapshot(&self) -> CrdtDelta {
        self.store.snapshot()
    }

    // ----- Apply to KnowledgeGraph ----------------------------------------

    /// Materialize all **Ratified** proposals into `kg` as [`Hyperedge`]s.
    ///
    /// Ensures a [`EntityType::Document`] entity exists per member path
    /// (domain tag `0x22`). Skips hyperedges whose metadata `proposal_id`
    /// is already present.
    pub fn apply_to_graph(&self, kg: &mut KnowledgeGraph) -> ApplyStats {
        let mut stats = ApplyStats::default();
        let existing_ids: std::collections::HashSet<String> = kg
            .hyperedges
            .iter()
            .filter_map(|h| {
                h.metadata
                    .get("proposal_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .collect();

        for proposal in self.store.ratified() {
            if existing_ids.contains(&proposal.id) {
                stats.hyperedges_skipped += 1;
                continue;
            }

            let mut entity_ids = Vec::with_capacity(proposal.members.len());
            for path in &proposal.members {
                let eid = vault_entity_id(path);
                if kg.entity(&eid).is_none() {
                    let label = std::path::Path::new(path)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(path)
                        .to_string();
                    kg.add_entity(Entity {
                        id: eid.clone(),
                        entity_type: EntityType::Document,
                        label,
                        iri: None,
                        source_file: Some(path.clone()),
                        source_location: None,
                        file_type: FileType::Document,
                        metadata: serde_json::json!({
                            "domain": "vault",
                            "path": path,
                        }),
                        legacy_id: Some(path.clone()),
                    });
                    stats.entities_added += 1;
                }
                entity_ids.push(eid);
            }

            kg.hyperedges.push(Hyperedge {
                label: proposal.label.clone(),
                entity_ids,
                metadata: serde_json::json!({
                    "proposal_id": proposal.id,
                    "kind": proposal.kind.as_str(),
                    "status": "ratified",
                    "confidence": proposal.confidence,
                    "score": proposal.score,
                    "reason": proposal.reason,
                    "shared_tags": proposal.shared_tags,
                    "domain": "vault",
                    "source": "vault_hyperedge_pipeline",
                }),
            });
            stats.hyperedges_applied += 1;
        }
        stats
    }
}

impl Default for VaultHyperedgePipeline {
    fn default() -> Self {
        Self::new("local")
    }
}

/// Deterministic entity id for a vault document path.
pub fn vault_entity_id(path: &str) -> EntityId {
    EntityId::new(&VAULT_DOMAIN, &EntityType::Document, path, path)
}

// ---------------------------------------------------------------------------
// Convenience: full round-trip helper (tests + CLI glue)
// ---------------------------------------------------------------------------

/// SUGGEST from nodes → auto-ratify above threshold → apply to a fresh graph.
///
/// Useful for batch offline runs and round-trip tests. Interactive UIs should
/// call [`VaultHyperedgePipeline::suggest`] / [`ratify`] / [`apply_to_graph`]
/// separately so humans can review pending proposals.
pub fn suggest_ratify_apply(
    nodes: &HashMap<String, VaultNode>,
    actor: &str,
    config: HyperedgeSuggestConfig,
    auto_ratify_threshold: f64,
) -> (VaultHyperedgePipeline, KnowledgeGraph, ApplyStats) {
    let mut pipeline = VaultHyperedgePipeline::with_config(actor, config);
    pipeline.suggest(nodes);
    pipeline.ratify_above(auto_ratify_threshold);
    let mut kg = KnowledgeGraph::new();
    let stats = pipeline.apply_to_graph(&mut kg);
    (pipeline, kg, stats)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::hyperedge::HyperedgeKind;
    use std::path::PathBuf;

    fn node(path: &str, tags: &[&str], out: &[&str], inc: &[&str]) -> VaultNode {
        VaultNode {
            path: PathBuf::from(path),
            title: Some(path.to_string()),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            outgoing: out.iter().map(|s| s.to_string()).collect(),
            incoming: inc.iter().map(|s| s.to_string()).collect(),
            word_count: 40,
        }
    }

    fn sample_vault() -> HashMap<String, VaultNode> {
        let mut nodes = HashMap::new();
        nodes.insert(
            "docs/auth.md".into(),
            node(
                "docs/auth.md",
                &["security", "api"],
                &["docs/oauth.md"],
                &[],
            ),
        );
        nodes.insert(
            "docs/oauth.md".into(),
            node(
                "docs/oauth.md",
                &["security", "api"],
                &[],
                &["docs/auth.md"],
            ),
        );
        nodes.insert(
            "docs/jwt.md".into(),
            node("docs/jwt.md", &["security"], &[], &[]),
        );
        nodes.insert(
            "notes/unrelated.md".into(),
            node("notes/unrelated.md", &["gardening"], &[], &[]),
        );
        nodes
    }

    #[test]
    fn round_trip_suggest_ratify_apply() {
        let nodes = sample_vault();
        let mut pipe = VaultHyperedgePipeline::with_config(
            "tester",
            HyperedgeSuggestConfig {
                min_members: 3,
                min_score: 2.0,
                max_proposals: 20,
                ..Default::default()
            },
        );

        let suggested = pipe.suggest(&nodes);
        assert!(
            !suggested.is_empty(),
            "expected at least one hyperedge suggestion"
        );
        assert!(
            suggested
                .iter()
                .all(|p| p.status == ProposalLifecycle::Suggested)
        );

        let view = pipe.ratify_view();
        assert_eq!(view.pending.len(), suggested.len().min(view.pending.len()));
        assert!(view.suggested_count > 0);
        assert_eq!(view.ratified_count, 0);

        // Ratify the top pending proposal only.
        let top_id = pipe.pending()[0].id.clone();
        pipe.ratify(&top_id).unwrap();
        assert_eq!(pipe.store().ratified().len(), 1);

        let mut kg = KnowledgeGraph::new();
        let stats = pipe.apply_to_graph(&mut kg);
        assert_eq!(stats.hyperedges_applied, 1);
        assert!(stats.entities_added >= 3);
        assert_eq!(kg.hyperedges.len(), 1);
        assert!(
            kg.hyperedges[0]
                .metadata
                .get("proposal_id")
                .and_then(|v| v.as_str())
                == Some(top_id.as_str())
        );
        assert_eq!(
            kg.hyperedges[0].entity_ids.len(),
            pipe.store().get(&top_id).unwrap().proposal.members.len()
        );

        // Idempotent re-apply.
        let stats2 = pipe.apply_to_graph(&mut kg);
        assert_eq!(stats2.hyperedges_applied, 0);
        assert_eq!(stats2.hyperedges_skipped, 1);
        assert_eq!(kg.hyperedges.len(), 1);
    }

    #[test]
    fn reject_excludes_from_apply() {
        let nodes = sample_vault();
        let mut pipe = VaultHyperedgePipeline::new("tester");
        pipe.config_mut().min_members = 3;
        pipe.config_mut().min_score = 1.0;
        pipe.suggest(&nodes);
        let ids: Vec<String> = pipe.pending().iter().map(|p| p.id.clone()).collect();
        assert!(!ids.is_empty());
        for id in &ids {
            pipe.reject(id).unwrap();
        }
        let mut kg = KnowledgeGraph::new();
        let stats = pipe.apply_to_graph(&mut kg);
        assert_eq!(stats.hyperedges_applied, 0);
        assert!(kg.hyperedges.is_empty());
    }

    #[test]
    fn multi_replica_merge_then_apply() {
        let nodes = sample_vault();
        let mut alice = VaultHyperedgePipeline::with_config(
            "alice",
            HyperedgeSuggestConfig {
                min_members: 3,
                min_score: 1.0,
                enable_orphan_bridge: false,
                enable_link_motif: false,
                enable_path_cluster: true,
                enable_shared_tags: true,
                ..Default::default()
            },
        );
        let mut bob = VaultHyperedgePipeline::with_config(
            "bob",
            HyperedgeSuggestConfig {
                min_members: 3,
                min_score: 1.0,
                enable_orphan_bridge: false,
                enable_link_motif: false,
                enable_path_cluster: true,
                enable_shared_tags: true,
                ..Default::default()
            },
        );

        alice.suggest(&nodes);
        // Bob starts empty, pulls alice's suggestions.
        bob.merge(&alice.snapshot());
        assert!(!bob.pending().is_empty());

        let id = bob.pending()[0].id.clone();
        bob.ratify(&id).unwrap();

        // Alice pulls bob's ratification.
        alice.merge(&bob.delta_since(0));
        assert!(
            alice
                .store()
                .get(&id)
                .map(|e| e.proposal.status == ProposalLifecycle::Ratified)
                .unwrap_or(false)
        );

        let mut kg = KnowledgeGraph::new();
        let stats = alice.apply_to_graph(&mut kg);
        assert_eq!(stats.hyperedges_applied, 1);
        assert!(kg.entity_count() >= 3);
    }

    #[test]
    fn helper_suggest_ratify_apply() {
        let nodes = sample_vault();
        let cfg = HyperedgeSuggestConfig {
            min_members: 3,
            min_score: 1.0,
            ..Default::default()
        };
        let (pipe, kg, stats) = suggest_ratify_apply(&nodes, "batch", cfg, 0.0);
        assert!(stats.hyperedges_applied > 0);
        assert!(!kg.hyperedges.is_empty());
        assert!(
            pipe.store()
                .ratified()
                .iter()
                .all(|p| p.status == ProposalLifecycle::Ratified)
        );
    }

    #[test]
    fn ratify_view_serializes() {
        let nodes = sample_vault();
        let mut pipe = VaultHyperedgePipeline::new("ui");
        pipe.config_mut().min_score = 1.0;
        pipe.suggest(&nodes);
        let view = pipe.ratify_view();
        let json = serde_json::to_string(&view).unwrap();
        assert!(json.contains("pending"));
        let back: RatifyView = serde_json::from_str(&json).unwrap();
        assert_eq!(back.actor, "ui");
    }

    #[test]
    fn shared_tags_kind_present() {
        let nodes = sample_vault();
        let mut pipe = VaultHyperedgePipeline::new("t");
        pipe.config_mut().min_members = 3;
        pipe.config_mut().min_score = 1.0;
        pipe.config_mut().enable_path_cluster = false;
        pipe.config_mut().enable_link_motif = false;
        pipe.config_mut().enable_orphan_bridge = false;
        let s = pipe.suggest(&nodes);
        assert!(s.iter().any(|p| p.kind == HyperedgeKind::SharedTags));
    }
}
