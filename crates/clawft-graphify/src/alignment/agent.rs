//! EA-Agent orchestration: propose merges, override, apply.

use super::score::{
    AlignmentConfig, AlignmentEvidence, AlignmentMethod, EmbeddingLookup, EntityAlignment,
    ScoreBreakdown, build_evidence, classify_method, entity_embedding, fuse_scores,
    label_similarity, structural_similarity,
};
use crate::entity::EntityId;
use crate::model::{Entity, KnowledgeGraph};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Proposal types
// ---------------------------------------------------------------------------

/// Lifecycle status of a merge proposal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProposalStatus {
    /// Awaiting human or agent decision.
    Pending,
    /// Explicitly accepted (manual override or agent).
    Accepted,
    /// Explicitly rejected.
    Rejected,
    /// Confidence ≥ [`AlignmentConfig::auto_accept_threshold`].
    AutoAccepted,
}

/// Which side to keep when applying a merge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreferredSide {
    /// Keep the entity from graph A (default when degrees tie).
    A,
    /// Keep the entity from graph B.
    B,
}

/// EA-Agent merge proposal with confidence and rationale.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MergeProposal {
    pub alignment: EntityAlignment,
    pub status: ProposalStatus,
    /// Confidence in `[0, 1]` (may differ from fused score after refine).
    pub confidence: f64,
    pub rationale: String,
    pub preferred_keep: PreferredSide,
}

/// Result of applying accepted proposals into a target graph.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ApplyResult {
    pub merges_applied: usize,
    pub entities_imported: usize,
    pub edges_imported: usize,
    pub edges_redirected: usize,
    /// Surviving A-side id → original B-side id for each applied merge.
    pub mapping: Vec<(EntityId, EntityId)>,
}

// ---------------------------------------------------------------------------
// Alignment refiner (LLM-agent scaffold)
// ---------------------------------------------------------------------------

/// Decision returned by an optional agent refine step.
#[derive(Debug, Clone)]
pub struct RefineDecision {
    pub confidence: f64,
    pub rationale: String,
    pub method: AlignmentMethod,
    /// When true, force-accept; when false, leave status policy to thresholds.
    pub force_accept: Option<bool>,
}

/// Scaffold for LLM-agent dispatch (EA-Agent paper multi-step reasoner).
///
/// Full chat-agent delegation is blocked on agent-core (see WEFT-355 deps).
/// Callers may implement this trait once a model path is available.
pub trait AlignmentRefiner {
    fn refine(&self, evidence: &AlignmentEvidence, scores: &ScoreBreakdown) -> RefineDecision;
}

/// Default heuristic refiner — no network; rewrites rationale from scores.
#[derive(Debug, Default, Clone, Copy)]
pub struct HeuristicRefiner;

impl AlignmentRefiner for HeuristicRefiner {
    fn refine(&self, evidence: &AlignmentEvidence, scores: &ScoreBreakdown) -> RefineDecision {
        let method = classify_method(scores);
        let rationale = format!(
            "heuristic: label={:.3} embed={:.3} struct={:.3} fused={:.3}; \
             types={}↔{}; shared_neighbors={}; relations={}",
            scores.label,
            scores.embedding,
            scores.structural,
            scores.combined,
            evidence.type_a,
            evidence.type_b,
            evidence.shared_neighbors.len(),
            if evidence.relation_summary.is_empty() {
                "∅"
            } else {
                evidence.relation_summary.as_str()
            }
        );
        RefineDecision {
            confidence: scores.combined,
            rationale,
            method,
            force_accept: None,
        }
    }
}

// ---------------------------------------------------------------------------
// EaAgent
// ---------------------------------------------------------------------------

/// Multi-repo entity alignment agent (KG-015 scaffold).
#[derive(Debug, Clone)]
pub struct EaAgent {
    pub config: AlignmentConfig,
}

impl Default for EaAgent {
    fn default() -> Self {
        Self::new(AlignmentConfig::default())
    }
}

impl EaAgent {
    pub fn new(config: AlignmentConfig) -> Self {
        Self { config }
    }

    /// Propose merges between `graph_a` and `graph_b`.
    ///
    /// Uses the [`HeuristicRefiner`] and optional external embeddings.
    pub fn propose(
        &self,
        graph_a: &KnowledgeGraph,
        graph_b: &KnowledgeGraph,
        embeddings: Option<&dyn EmbeddingLookup>,
    ) -> Vec<MergeProposal> {
        self.propose_with_refiner(graph_a, graph_b, embeddings, &HeuristicRefiner)
    }

    /// Like [`Self::propose`] but with a custom [`AlignmentRefiner`].
    pub fn propose_with_refiner(
        &self,
        graph_a: &KnowledgeGraph,
        graph_b: &KnowledgeGraph,
        embeddings: Option<&dyn EmbeddingLookup>,
        refiner: &dyn AlignmentRefiner,
    ) -> Vec<MergeProposal> {
        let pairs = self.collect_scored_pairs(graph_a, graph_b, embeddings);
        let mut proposals = Vec::with_capacity(pairs.len());

        for (alignment, deg_a, deg_b) in pairs {
            let decision = refiner.refine(&alignment.evidence, &alignment.scores);
            let mut alignment = alignment;
            alignment.method = decision.method;
            alignment.similarity = decision.confidence;

            let preferred_keep = if deg_b > deg_a {
                PreferredSide::B
            } else {
                PreferredSide::A
            };

            let status = match decision.force_accept {
                Some(true) => ProposalStatus::Accepted,
                Some(false) => ProposalStatus::Rejected,
                None if decision.confidence >= self.config.auto_accept_threshold => {
                    ProposalStatus::AutoAccepted
                }
                None => ProposalStatus::Pending,
            };

            proposals.push(MergeProposal {
                alignment,
                status,
                confidence: decision.confidence,
                rationale: decision.rationale,
                preferred_keep,
            });
        }

        proposals.sort_by(|a, b| {
            b.confidence
                .partial_cmp(&a.confidence)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        proposals
    }

    /// Manual override of a proposal by index.
    pub fn override_status(
        proposals: &mut [MergeProposal],
        index: usize,
        status: ProposalStatus,
    ) -> bool {
        if let Some(p) = proposals.get_mut(index) {
            p.status = status;
            true
        } else {
            false
        }
    }

    /// Apply accepted / auto-accepted proposals into `graph_a` (canonical store).
    ///
    /// - Aligned B entities map onto the A-side survivor (A remains canonical;
    ///   [`PreferredSide`] is advisory metadata for callers/UI).
    /// - Unaligned B entities and remapped B edges are imported.
    pub fn apply_accepted(
        &self,
        graph_a: &mut KnowledgeGraph,
        graph_b: &KnowledgeGraph,
        proposals: &[MergeProposal],
    ) -> ApplyResult {
        let mut result = ApplyResult::default();
        let mut b_to_a: HashMap<EntityId, EntityId> = HashMap::new();

        for p in proposals {
            if !matches!(
                p.status,
                ProposalStatus::Accepted | ProposalStatus::AutoAccepted
            ) {
                continue;
            }
            let survivor = p.alignment.entity_a.clone();
            let from_b = p.alignment.entity_b.clone();
            if graph_a.entity(&survivor).is_none() {
                // Defensive: A-side entity missing — skip.
                continue;
            }
            b_to_a.insert(from_b.clone(), survivor.clone());
            result.mapping.push((survivor, from_b));
            result.merges_applied += 1;
        }

        // Import remaining B entities (not merged away).
        for eb in graph_b.entities() {
            if b_to_a.contains_key(&eb.id) {
                continue;
            }
            if graph_a.entity(&eb.id).is_none() {
                graph_a.add_entity(eb.clone());
                result.entities_imported += 1;
            }
        }

        // Import B edges with id remapping through the alignment map.
        for (_, _, rel) in graph_b.edges() {
            let mut new_rel = rel.clone();
            if let Some(mapped) = b_to_a.get(&new_rel.source) {
                new_rel.source = mapped.clone();
                result.edges_redirected += 1;
            }
            if let Some(mapped) = b_to_a.get(&new_rel.target) {
                new_rel.target = mapped.clone();
                result.edges_redirected += 1;
            }
            if new_rel.source == new_rel.target {
                continue;
            }
            if graph_a.entity(&new_rel.source).is_some()
                && graph_a.entity(&new_rel.target).is_some()
            {
                graph_a.add_relationship(new_rel);
                result.edges_imported += 1;
            }
        }

        result
    }

    // --- internal scoring --------------------------------------------------

    fn collect_scored_pairs(
        &self,
        graph_a: &KnowledgeGraph,
        graph_b: &KnowledgeGraph,
        embeddings: Option<&dyn EmbeddingLookup>,
    ) -> Vec<(EntityAlignment, usize, usize)> {
        let b_entities: Vec<&Entity> = graph_b.entities().collect();
        let mut candidates: Vec<(EntityAlignment, usize, usize)> = Vec::new();

        for ea in graph_a.entities() {
            let emb_a = entity_embedding(graph_a, ea, embeddings, self.config.local_embedding_dims);

            let mut shortlist: Vec<(&Entity, f64, f64)> = Vec::new();
            for eb in &b_entities {
                if self.config.require_same_type && ea.entity_type != eb.entity_type {
                    continue;
                }
                let label_sim = label_similarity(&ea.label, &eb.label);
                let emb_b =
                    entity_embedding(graph_b, eb, embeddings, self.config.local_embedding_dims);
                let emb_sim = super::score::cosine_similarity(&emb_a, &emb_b);

                if label_sim >= self.config.label_candidate_threshold
                    || emb_sim >= self.config.embedding_candidate_threshold
                {
                    shortlist.push((eb, label_sim, emb_sim));
                }
            }

            shortlist.sort_by(|x, y| {
                let sx = 0.6 * x.1 + 0.4 * x.2;
                let sy = 0.6 * y.1 + 0.4 * y.2;
                sy.partial_cmp(&sx).unwrap_or(std::cmp::Ordering::Equal)
            });
            shortlist.truncate(self.config.max_candidates);

            for (eb, label_sim, emb_sim) in shortlist {
                let (struct_sim, shared) = structural_similarity(graph_a, &ea.id, graph_b, &eb.id);
                let scores = fuse_scores(label_sim, emb_sim, struct_sim, &self.config);
                // Drop very weak fused scores early.
                if scores.combined < 0.35 {
                    continue;
                }
                let evidence = build_evidence(ea, eb, shared, graph_a, graph_b);
                let method = classify_method(&scores);
                candidates.push((
                    EntityAlignment {
                        entity_a: ea.id.clone(),
                        entity_b: eb.id.clone(),
                        similarity: scores.combined,
                        scores,
                        method,
                        evidence,
                    },
                    graph_a.degree(&ea.id),
                    graph_b.degree(&eb.id),
                ));
            }
        }

        // Greedy mutual best: each entity pairs at most once.
        candidates.sort_by(|a, b| {
            b.0.similarity
                .partial_cmp(&a.0.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut used_a: HashSet<EntityId> = HashSet::new();
        let mut used_b: HashSet<EntityId> = HashSet::new();
        let mut out = Vec::new();
        for c in candidates {
            if used_a.contains(&c.0.entity_a) || used_b.contains(&c.0.entity_b) {
                continue;
            }
            used_a.insert(c.0.entity_a.clone());
            used_b.insert(c.0.entity_b.clone());
            out.push(c);
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Convenience free functions (backward-compatible surface)
// ---------------------------------------------------------------------------

/// Find cross-graph alignments with default EA-Agent config.
///
/// Returns scored [`EntityAlignment`] rows (no proposal lifecycle).
pub fn align_entities(
    graph_a: &KnowledgeGraph,
    graph_b: &KnowledgeGraph,
    threshold: f64,
) -> Vec<EntityAlignment> {
    align_entities_with_config(graph_a, graph_b, threshold, &AlignmentConfig::default())
}

/// Align with an explicit config. Embeddings use the local char-trigram fallback.
pub fn align_entities_with_config(
    graph_a: &KnowledgeGraph,
    graph_b: &KnowledgeGraph,
    threshold: f64,
    config: &AlignmentConfig,
) -> Vec<EntityAlignment> {
    let agent = EaAgent::new(config.clone());
    agent
        .collect_scored_pairs(graph_a, graph_b, None)
        .into_iter()
        .map(|(a, _, _)| a)
        .filter(|a| a.similarity >= threshold)
        .collect()
}

/// EA-Agent style merge proposals (pending / auto-accepted).
pub fn propose_merges(
    graph_a: &KnowledgeGraph,
    graph_b: &KnowledgeGraph,
    threshold: f64,
) -> Vec<MergeProposal> {
    let agent = EaAgent::default();
    let mut proposals = agent.propose(graph_a, graph_b, None);
    proposals.retain(|p| p.confidence >= threshold);
    proposals
}

// ---------------------------------------------------------------------------
// Tests — multi-repo fixtures (should / should-not align)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::relationship::{Confidence, RelationType, Relationship};

    fn entity(name: &str, source: &str, ty: EntityType) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &ty, name, source),
            entity_type: ty,
            label: name.to_string(),
            source_file: Some(source.into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({ "repo": source }),
            legacy_id: None,
            iri: None,
        }
    }

    fn rel(src: &Entity, tgt: &Entity) -> Relationship {
        Relationship {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation_type: RelationType::Imports,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    /// Fixture graph A: small "auth-service" repo.
    fn fixture_repo_a() -> (KnowledgeGraph, Entity, Entity, Entity) {
        let auth = entity("auth_service", "repo-a/auth.rs", EntityType::Module);
        let db = entity("db_pool", "repo-a/db.rs", EntityType::Module);
        let cfg = entity("config_loader", "repo-a/config.rs", EntityType::Module);
        let mut kg = KnowledgeGraph::new();
        kg.add_entity(auth.clone());
        kg.add_entity(db.clone());
        kg.add_entity(cfg.clone());
        kg.add_relationship(rel(&auth, &db));
        kg.add_relationship(rel(&auth, &cfg));
        (kg, auth, db, cfg)
    }

    /// Fixture graph B: same concepts, different naming / paths (should align).
    fn fixture_repo_b_should_align() -> (KnowledgeGraph, Entity, Entity, Entity) {
        let auth = entity("AuthService", "repo-b/auth/mod.rs", EntityType::Module);
        let db = entity("DatabasePool", "repo-b/db/pool.rs", EntityType::Module);
        let cfg = entity("ConfigLoader", "repo-b/cfg.rs", EntityType::Module);
        let mut kg = KnowledgeGraph::new();
        kg.add_entity(auth.clone());
        kg.add_entity(db.clone());
        kg.add_entity(cfg.clone());
        kg.add_relationship(rel(&auth, &db));
        kg.add_relationship(rel(&auth, &cfg));
        (kg, auth, db, cfg)
    }

    /// Fixture graph C: unrelated payment domain (should not align to A).
    fn fixture_repo_c_should_not_align() -> KnowledgeGraph {
        let pay = entity("payment_gateway", "repo-c/pay.rs", EntityType::Module);
        let inv = entity("invoice_renderer", "repo-c/invoice.rs", EntityType::Module);
        let tax = entity("tax_engine", "repo-c/tax.rs", EntityType::Module);
        let mut kg = KnowledgeGraph::new();
        kg.add_entity(pay.clone());
        kg.add_entity(inv.clone());
        kg.add_entity(tax.clone());
        kg.add_relationship(rel(&pay, &inv));
        kg.add_relationship(rel(&pay, &tax));
        kg
    }

    #[test]
    fn multi_repo_should_align() {
        let (kg_a, auth_a, db_a, _) = fixture_repo_a();
        let (kg_b, auth_b, db_b, _) = fixture_repo_b_should_align();

        let proposals = propose_merges(&kg_a, &kg_b, 0.45);
        assert!(
            proposals.len() >= 2,
            "expected ≥2 merge proposals across similar repos, got {}",
            proposals.len()
        );

        let auth_hit = proposals
            .iter()
            .any(|p| p.alignment.entity_a == auth_a.id && p.alignment.entity_b == auth_b.id);
        let db_hit = proposals
            .iter()
            .any(|p| p.alignment.entity_a == db_a.id && p.alignment.entity_b == db_b.id);
        assert!(
            auth_hit,
            "auth_service ↔ AuthService should align: {proposals:?}"
        );
        assert!(db_hit, "db_pool ↔ DatabasePool should align");

        for p in &proposals {
            assert!(
                p.confidence >= 0.45,
                "proposal confidence too low: {}",
                p.confidence
            );
            assert!(!p.rationale.is_empty());
        }
    }

    #[test]
    fn multi_repo_should_not_align() {
        let (kg_a, _, _, _) = fixture_repo_a();
        let kg_c = fixture_repo_c_should_not_align();

        // High threshold: unrelated domains must not produce confident merges.
        let proposals = propose_merges(&kg_a, &kg_c, 0.70);
        assert!(
            proposals.is_empty(),
            "auth repo must not align to payment repo at 0.70: {:?}",
            proposals
                .iter()
                .map(|p| (
                    p.alignment.evidence.label_a.clone(),
                    p.alignment.evidence.label_b.clone(),
                    p.confidence
                ))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn align_entities_threshold_filters() {
        let (kg_a, _, _, _) = fixture_repo_a();
        let (kg_b, _, _, _) = fixture_repo_b_should_align();
        let high = align_entities(&kg_a, &kg_b, 0.99);
        let low = align_entities(&kg_a, &kg_b, 0.40);
        assert!(low.len() >= high.len());
        assert!(!low.is_empty());
    }

    #[test]
    fn manual_override_and_apply() {
        let (mut kg_a, auth_a, _, _) = fixture_repo_a();
        let (kg_b, auth_b, _, _) = fixture_repo_b_should_align();
        let agent = EaAgent::default();
        let mut proposals = agent.propose(&kg_a, &kg_b, None);
        assert!(!proposals.is_empty());

        // Accept the first proposal only; reject the rest.
        for i in 0..proposals.len() {
            let status = if i == 0 {
                ProposalStatus::Accepted
            } else {
                ProposalStatus::Rejected
            };
            assert!(EaAgent::override_status(&mut proposals, i, status));
        }

        let before = kg_a.entity_count();
        let result = agent.apply_accepted(&mut kg_a, &kg_b, &proposals);
        assert_eq!(result.merges_applied, 1);
        assert!(result.entities_imported >= 1);
        assert!(kg_a.entity_count() >= before);
        // Mapping should reference auth pair if that was proposal 0, else any accepted.
        assert_eq!(result.mapping.len(), 1);
        let _ = (auth_a, auth_b);
    }

    #[test]
    fn empty_graphs_produce_no_proposals() {
        let a = KnowledgeGraph::new();
        let b = KnowledgeGraph::new();
        assert!(propose_merges(&a, &b, 0.5).is_empty());
    }

    #[test]
    fn type_mismatch_skipped_when_required() {
        let mut kg_a = KnowledgeGraph::new();
        kg_a.add_entity(entity("auth", "a.rs", EntityType::Module));
        let mut kg_b = KnowledgeGraph::new();
        kg_b.add_entity(entity("auth", "b.rs", EntityType::Function));

        let config = AlignmentConfig {
            require_same_type: true,
            ..AlignmentConfig::default()
        };
        let hits = align_entities_with_config(&kg_a, &kg_b, 0.5, &config);
        assert!(hits.is_empty());
    }

    #[test]
    fn external_embeddings_boost() {
        use super::super::score::{MapEmbeddingLookup, local_label_embedding};

        let mut kg_a = KnowledgeGraph::new();
        let ea = entity("alpha_mod", "a.rs", EntityType::Module);
        kg_a.add_entity(ea.clone());

        let mut kg_b = KnowledgeGraph::new();
        // Dissimilar labels but identical forced embeddings → embedding path.
        let eb = entity("zzz_other", "b.rs", EntityType::Module);
        kg_b.add_entity(eb.clone());

        let shared = local_label_embedding("shared_concept_vector", 64);
        let mut map = MapEmbeddingLookup::default();
        map.map.insert(ea.id.clone(), shared.clone());
        map.map.insert(eb.id.clone(), shared);

        let config = AlignmentConfig {
            label_candidate_threshold: 0.99, // labels will not pass
            embedding_candidate_threshold: 0.50,
            label_weight: 0.1,
            embedding_weight: 0.8,
            structural_weight: 0.1,
            require_same_type: true,
            ..AlignmentConfig::default()
        };
        let agent = EaAgent::new(config);
        let proposals = agent.propose(&kg_a, &kg_b, Some(&map));
        assert_eq!(proposals.len(), 1);
        assert!(
            proposals[0].alignment.scores.embedding > 0.95,
            "expected near-perfect embedding match"
        );
        assert_eq!(
            proposals[0].alignment.method,
            AlignmentMethod::EmbeddingMatch
        );
    }

    #[test]
    fn custom_refiner_can_reject() {
        struct RejectAll;
        impl AlignmentRefiner for RejectAll {
            fn refine(
                &self,
                _evidence: &AlignmentEvidence,
                scores: &ScoreBreakdown,
            ) -> RefineDecision {
                RefineDecision {
                    confidence: scores.combined,
                    rationale: "reject".into(),
                    method: AlignmentMethod::AgentRefined,
                    force_accept: Some(false),
                }
            }
        }

        let (kg_a, _, _, _) = fixture_repo_a();
        let (kg_b, _, _, _) = fixture_repo_b_should_align();
        let agent = EaAgent::default();
        let proposals = agent.propose_with_refiner(&kg_a, &kg_b, None, &RejectAll);
        assert!(!proposals.is_empty());
        assert!(
            proposals
                .iter()
                .all(|p| p.status == ProposalStatus::Rejected)
        );
        assert!(
            proposals
                .iter()
                .all(|p| p.alignment.method == AlignmentMethod::AgentRefined)
        );
    }
}
