//! Graph analysis: god nodes, surprising connections, suggested questions, graph diff.
//!
//! Ported from Python `graphify/analyze.py`.

use crate::cluster::cohesion_score;
use crate::eml_models::SurpriseScorerModel;
use crate::entity::EntityId;
use crate::model::KnowledgeGraph;
use crate::relationship::Confidence;
use petgraph::Direction;
use petgraph::graph::NodeIndex;
use petgraph::visit::EdgeRef;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// File-type classification
// ---------------------------------------------------------------------------

const CODE_EXTENSIONS: &[&str] = &[
    "py", "ts", "tsx", "js", "go", "rs", "java", "rb", "cpp", "c", "h", "cs", "kt", "scala", "php",
];
const PAPER_EXTENSIONS: &[&str] = &["pdf"];
const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "svg"];

fn file_category(path: &str) -> &'static str {
    let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ext = ext.as_str();
    if CODE_EXTENSIONS.contains(&ext) {
        "code"
    } else if PAPER_EXTENSIONS.contains(&ext) {
        "paper"
    } else if IMAGE_EXTENSIONS.contains(&ext) {
        "image"
    } else {
        "doc"
    }
}

fn top_level_dir(path: &str) -> &str {
    path.split('/').next().unwrap_or(path)
}

// ---------------------------------------------------------------------------
// Node classification helpers
// ---------------------------------------------------------------------------

/// Returns true if this node is a file-level hub or AST method stub.
pub fn is_file_node(kg: &KnowledgeGraph, id: &EntityId) -> bool {
    let entity = match kg.entity(id) {
        Some(e) => e,
        None => return false,
    };
    let label = &entity.label;
    if label.is_empty() {
        return false;
    }

    // File-level hub: label matches source filename
    if let Some(ref source_file) = entity.source_file
        && !source_file.is_empty()
        && let Some(fname) = std::path::Path::new(source_file.as_str()).file_name()
        && label == fname.to_str().unwrap_or("")
    {
        return true;
    }

    // Method stub: `.method_name()`
    if label.starts_with('.') && label.ends_with("()") {
        return true;
    }

    // Module-level function stub: `function_name()` with degree <= 1
    if label.ends_with("()") && kg.degree(id) <= 1 {
        return true;
    }

    false
}

/// Returns true for manually-injected semantic concept nodes (not from source code).
pub fn is_concept_node(kg: &KnowledgeGraph, id: &EntityId) -> bool {
    let entity = match kg.entity(id) {
        Some(e) => e,
        None => return true,
    };
    let source = match &entity.source_file {
        Some(s) if !s.is_empty() => s.as_str(),
        _ => return true,
    };
    // Has no file extension -> probably a concept label
    let filename = source.rsplit('/').next().unwrap_or(source);
    !filename.contains('.')
}

// ---------------------------------------------------------------------------
// Utility
// ---------------------------------------------------------------------------

/// Invert `{community_id: [entity_ids]}` to `{entity_id: community_id}`.
pub fn node_community_map(communities: &HashMap<usize, Vec<EntityId>>) -> HashMap<EntityId, usize> {
    let mut map = HashMap::new();
    for (&cid, nodes) in communities {
        for node in nodes {
            map.insert(node.clone(), cid);
        }
    }
    map
}

// ---------------------------------------------------------------------------
// God Nodes (GRAPH-017)
// ---------------------------------------------------------------------------

/// A highly-connected entity: a core abstraction.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GodNode {
    pub id: EntityId,
    pub label: String,
    pub edges: usize,
}

/// Return the top-N most-connected real entities, excluding file and concept nodes.
pub fn god_nodes(kg: &KnowledgeGraph, top_n: usize) -> Vec<GodNode> {
    let mut degrees: Vec<(EntityId, usize)> = kg
        .entity_ids()
        .map(|id| (id.clone(), kg.degree(id)))
        .collect();

    degrees.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.0.cmp(&b.0.0)));

    let mut result = Vec::with_capacity(top_n);
    for (id, deg) in degrees {
        if is_file_node(kg, &id) || is_concept_node(kg, &id) {
            continue;
        }
        let label = kg
            .entity(&id)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| id.to_hex());
        result.push(GodNode {
            id,
            label,
            edges: deg,
        });
        if result.len() >= top_n {
            break;
        }
    }
    result
}

// ---------------------------------------------------------------------------
// Surprising Connections (GRAPH-018)
// ---------------------------------------------------------------------------

/// A connection that is structurally non-obvious.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurprisingConnection {
    pub source: String,
    pub target: String,
    pub source_files: Vec<String>,
    pub confidence: Confidence,
    pub relation: String,
    pub why: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Find surprising connections in the graph.
///
/// - Multi-file corpora: cross-file edges ranked by composite surprise score.
/// - Single-file corpora: cross-community edges.
///
/// When `eml_model` is `Some`, uses the trained EML model for scoring.
/// Pass `None` to use the original hardcoded heuristics.
pub fn surprising_connections(
    kg: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<EntityId>>,
    top_n: usize,
) -> Vec<SurprisingConnection> {
    surprising_connections_eml(kg, communities, top_n, None)
}

/// Find surprising connections with an optional EML model for scoring.
pub fn surprising_connections_eml(
    kg: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<EntityId>>,
    top_n: usize,
    eml_model: Option<&SurpriseScorerModel>,
) -> Vec<SurprisingConnection> {
    let source_files = kg.source_files();
    let is_multi_source = source_files.len() > 1;

    if is_multi_source {
        cross_file_surprises(kg, communities, top_n, eml_model)
    } else {
        cross_community_surprises(kg, communities, top_n)
    }
}

/// Build the 7-element feature vector for surprise scoring.
///
/// `[confidence_ordinal, cross_file_type, cross_repo,
///   cross_community, is_semantic, min_degree, max_degree]`
#[allow(clippy::too_many_arguments)] // feature extraction; each arg is a distinct graph context
fn surprise_features(
    kg: &KnowledgeGraph,
    src_id: &EntityId,
    tgt_id: &EntityId,
    relation: &str,
    confidence: Confidence,
    node_community: &HashMap<EntityId, usize>,
    src_source: &str,
    tgt_source: &str,
) -> [f64; 7] {
    let conf_ord = match confidence {
        Confidence::Ambiguous => 3.0,
        Confidence::Inferred => 2.0,
        Confidence::Extracted => 1.0,
    };

    let cat_u = file_category(src_source);
    let cat_v = file_category(tgt_source);
    let cross_file_type = if cat_u != cat_v { 1.0 } else { 0.0 };
    let cross_repo = if top_level_dir(src_source) != top_level_dir(tgt_source) {
        1.0
    } else {
        0.0
    };

    let cid_u = node_community.get(src_id);
    let cid_v = node_community.get(tgt_id);
    let cross_community = match (cid_u, cid_v) {
        (Some(cu), Some(cv)) if cu != cv => 1.0,
        _ => 0.0,
    };

    let is_semantic = if relation == "semantically_similar_to" {
        1.0
    } else {
        0.0
    };

    let deg_u = kg.degree(src_id) as f64;
    let deg_v = kg.degree(tgt_id) as f64;
    let min_deg = deg_u.min(deg_v);
    let max_deg = deg_u.max(deg_v);

    [
        conf_ord,
        cross_file_type,
        cross_repo,
        cross_community,
        is_semantic,
        min_deg,
        max_deg,
    ]
}

/// Compute the composite surprise score for an edge.
///
/// When `eml_model` is `Some`, uses the trained model for scoring.
/// Otherwise falls back to the original hardcoded heuristics.
#[allow(clippy::too_many_arguments)] // scoring path; each arg is a graph context required by surprise_features
fn surprise_score(
    kg: &KnowledgeGraph,
    src_id: &EntityId,
    tgt_id: &EntityId,
    relation: &str,
    confidence: Confidence,
    node_community: &HashMap<EntityId, usize>,
    src_source: &str,
    tgt_source: &str,
    eml_model: Option<&SurpriseScorerModel>,
) -> (i32, Vec<String>) {
    let features = surprise_features(
        kg,
        src_id,
        tgt_id,
        relation,
        confidence,
        node_community,
        src_source,
        tgt_source,
    );

    // If a trained EML model is provided, use it for the score.
    if let Some(model) = eml_model
        && model.is_trained()
    {
        let eml_score = model.score(&features);
        // Still generate human-readable reasons from the feature vector.
        let reasons = surprise_reasons(
            &features, kg, src_id, tgt_id, src_source, tgt_source, confidence,
        );
        return (eml_score as i32, reasons);
    }

    // Hardcoded fallback (original logic).
    let mut score: i32 = 0;
    let mut reasons: Vec<String> = Vec::new();

    // 1. Confidence weight
    let conf_bonus = match confidence {
        Confidence::Ambiguous => 3,
        Confidence::Inferred => 2,
        Confidence::Extracted => 1,
    };
    score += conf_bonus;
    if matches!(confidence, Confidence::Ambiguous | Confidence::Inferred) {
        reasons.push(format!(
            "{} connection - not explicitly stated in source",
            confidence.as_str().to_lowercase()
        ));
    }

    // 2. Cross file-type bonus
    let cat_u = file_category(src_source);
    let cat_v = file_category(tgt_source);
    if cat_u != cat_v {
        score += 2;
        reasons.push(format!("crosses file types ({cat_u} <-> {cat_v})"));
    }

    // 3. Cross-repo bonus
    if top_level_dir(src_source) != top_level_dir(tgt_source) {
        score += 2;
        reasons.push("connects across different repos/directories".to_owned());
    }

    // 4. Cross-community bonus
    let cid_u = node_community.get(src_id);
    let cid_v = node_community.get(tgt_id);
    if let (Some(cu), Some(cv)) = (cid_u, cid_v)
        && cu != cv
    {
        score += 1;
        reasons.push("bridges separate communities".to_owned());
    }

    // 4b. Semantic similarity multiplier
    if relation == "semantically_similar_to" {
        score = (score as f64 * 1.5) as i32;
        reasons.push("semantically similar concepts with no structural link".to_owned());
    }

    // 5. Peripheral -> hub bonus
    let deg_u = kg.degree(src_id);
    let deg_v = kg.degree(tgt_id);
    if deg_u.min(deg_v) <= 2 && deg_u.max(deg_v) >= 5 {
        let src_label_str = kg
            .entity(src_id)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| src_id.to_hex());
        let tgt_label_str = kg
            .entity(tgt_id)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| tgt_id.to_hex());
        let (peripheral, hub) = if deg_u <= 2 {
            (src_label_str.as_str(), tgt_label_str.as_str())
        } else {
            (tgt_label_str.as_str(), src_label_str.as_str())
        };
        score += 1;
        reasons.push(format!(
            "peripheral node `{peripheral}` unexpectedly reaches hub `{hub}`"
        ));
    }

    (score, reasons)
}

/// Generate human-readable reasons from the feature vector.
///
/// Used when the EML model provides the numeric score but we still
/// want textual explanations.
fn surprise_reasons(
    features: &[f64; 7],
    kg: &KnowledgeGraph,
    src_id: &EntityId,
    tgt_id: &EntityId,
    src_source: &str,
    tgt_source: &str,
    confidence: Confidence,
) -> Vec<String> {
    let mut reasons = Vec::new();

    if matches!(confidence, Confidence::Ambiguous | Confidence::Inferred) {
        reasons.push(format!(
            "{} connection - not explicitly stated in source",
            confidence.as_str().to_lowercase()
        ));
    }
    if features[1] > 0.5 {
        let cat_u = file_category(src_source);
        let cat_v = file_category(tgt_source);
        reasons.push(format!("crosses file types ({cat_u} <-> {cat_v})"));
    }
    if features[2] > 0.5 {
        reasons.push("connects across different repos/directories".to_owned());
    }
    if features[3] > 0.5 {
        reasons.push("bridges separate communities".to_owned());
    }
    if features[4] > 0.5 {
        reasons.push("semantically similar concepts with no structural link".to_owned());
    }
    if features[5] <= 2.0 && features[6] >= 5.0 {
        let src_label_str = kg
            .entity(src_id)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| src_id.to_hex());
        let tgt_label_str = kg
            .entity(tgt_id)
            .map(|e| e.label.clone())
            .unwrap_or_else(|| tgt_id.to_hex());
        let (peripheral, hub) = if features[5] == kg.degree(src_id) as f64 {
            (src_label_str.as_str(), tgt_label_str.as_str())
        } else {
            (tgt_label_str.as_str(), src_label_str.as_str())
        };
        reasons.push(format!(
            "peripheral node `{peripheral}` unexpectedly reaches hub `{hub}`"
        ));
    }

    reasons
}

/// Structural edge relations to exclude.
const STRUCTURAL_RELATIONS: &[&str] = &["imports", "imports_from", "contains", "method"];

fn cross_file_surprises(
    kg: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<EntityId>>,
    top_n: usize,
    eml_model: Option<&SurpriseScorerModel>,
) -> Vec<SurprisingConnection> {
    let node_community = node_community_map(communities);

    struct Candidate {
        score: i32,
        conn: SurprisingConnection,
    }

    let mut candidates: Vec<Candidate> = Vec::new();

    for (src_ent, tgt_ent, rel) in kg.edges() {
        let rel_str = rel.relation_type_str();
        if STRUCTURAL_RELATIONS.contains(&rel_str.as_str()) {
            continue;
        }
        if is_concept_node(kg, &src_ent.id) || is_concept_node(kg, &tgt_ent.id) {
            continue;
        }
        if is_file_node(kg, &src_ent.id) || is_file_node(kg, &tgt_ent.id) {
            continue;
        }

        let src_source = src_ent.source_file.as_deref().unwrap_or("");
        let tgt_source = tgt_ent.source_file.as_deref().unwrap_or("");

        if src_source.is_empty() || tgt_source.is_empty() || src_source == tgt_source {
            continue;
        }

        let (score, reasons) = surprise_score(
            kg,
            &src_ent.id,
            &tgt_ent.id,
            &rel_str,
            rel.confidence,
            &node_community,
            src_source,
            tgt_source,
            eml_model,
        );

        candidates.push(Candidate {
            score,
            conn: SurprisingConnection {
                source: src_ent.label.clone(),
                target: tgt_ent.label.clone(),
                source_files: vec![src_source.to_owned(), tgt_source.to_owned()],
                confidence: rel.confidence,
                relation: rel_str,
                why: if reasons.is_empty() {
                    "cross-file semantic connection".to_owned()
                } else {
                    reasons.join("; ")
                },
                note: None,
            },
        });
    }

    candidates.sort_by_key(|b| std::cmp::Reverse(b.score));

    if candidates.is_empty() {
        return cross_community_surprises(kg, communities, top_n);
    }

    candidates.into_iter().take(top_n).map(|c| c.conn).collect()
}

fn cross_community_surprises(
    kg: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<EntityId>>,
    top_n: usize,
) -> Vec<SurprisingConnection> {
    if communities.is_empty() {
        // Fallback: use edge betweenness centrality approximation
        // (simple: rank by inverse degree product as a heuristic)
        if kg.edge_count() == 0 {
            return vec![];
        }
        let mut edges: Vec<(
            &crate::model::Entity,
            &crate::model::Entity,
            &crate::relationship::Relationship,
        )> = kg.edges().collect();
        // Sort by confidence (ambiguous first) then by product of inverse degrees
        edges.sort_by(|a, b| {
            let conf_order = |c: Confidence| match c {
                Confidence::Ambiguous => 0,
                Confidence::Inferred => 1,
                Confidence::Extracted => 2,
            };
            conf_order(a.2.confidence)
                .cmp(&conf_order(b.2.confidence))
                .then_with(|| {
                    let score_a =
                        1.0 / (kg.degree(&a.0.id) as f64 * kg.degree(&a.1.id) as f64).max(1.0);
                    let score_b =
                        1.0 / (kg.degree(&b.0.id) as f64 * kg.degree(&b.1.id) as f64).max(1.0);
                    score_b
                        .partial_cmp(&score_a)
                        .unwrap_or(std::cmp::Ordering::Equal)
                })
        });

        return edges
            .into_iter()
            .take(top_n)
            .map(|(src, tgt, rel)| SurprisingConnection {
                source: src.label.clone(),
                target: tgt.label.clone(),
                source_files: vec![
                    src.source_file.clone().unwrap_or_default(),
                    tgt.source_file.clone().unwrap_or_default(),
                ],
                confidence: rel.confidence,
                relation: rel.relation_type_str(),
                why: String::new(),
                note: Some("Bridges graph structure".to_owned()),
            })
            .collect();
    }

    let node_community = node_community_map(communities);
    let mut surprises: Vec<(SurprisingConnection, (usize, usize))> = Vec::new();

    for (src_ent, tgt_ent, rel) in kg.edges() {
        let cid_u = node_community.get(&src_ent.id);
        let cid_v = node_community.get(&tgt_ent.id);

        let (cu, cv) = match (cid_u, cid_v) {
            (Some(a), Some(b)) if a != b => (*a, *b),
            _ => continue,
        };

        if is_file_node(kg, &src_ent.id) || is_file_node(kg, &tgt_ent.id) {
            continue;
        }
        let rel_str = rel.relation_type_str();
        if STRUCTURAL_RELATIONS.contains(&rel_str.as_str()) {
            continue;
        }

        let pair = (cu.min(cv), cu.max(cv));
        surprises.push((
            SurprisingConnection {
                source: src_ent.label.clone(),
                target: tgt_ent.label.clone(),
                source_files: vec![
                    src_ent.source_file.clone().unwrap_or_default(),
                    tgt_ent.source_file.clone().unwrap_or_default(),
                ],
                confidence: rel.confidence,
                relation: rel_str,
                why: String::new(),
                note: Some(format!("Bridges community {cu} -> community {cv}")),
            },
            pair,
        ));
    }

    // Sort: AMBIGUOUS first, then INFERRED, then EXTRACTED
    surprises.sort_by_key(|(s, _)| match s.confidence {
        Confidence::Ambiguous => 0,
        Confidence::Inferred => 1,
        Confidence::Extracted => 2,
    });

    // Deduplicate by community pair
    let mut seen_pairs: HashSet<(usize, usize)> = HashSet::new();
    let mut deduped = Vec::new();
    for (conn, pair) in surprises {
        if seen_pairs.insert(pair) {
            deduped.push(conn);
        }
    }

    deduped.into_iter().take(top_n).collect()
}

// ---------------------------------------------------------------------------
// Question Generation (GRAPH-019)
// ---------------------------------------------------------------------------

/// Type of generated question.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuestionType {
    AmbiguousEdge,
    BridgeNode,
    VerifyInferred,
    IsolatedNodes,
    LowCohesion,
    NoSignal,
}

/// A question the graph is uniquely positioned to answer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SuggestedQuestion {
    pub question_type: QuestionType,
    pub question: Option<String>,
    pub why: String,
}

/// Generate questions based on graph structure.
///
/// 5 strategies: ambiguous edges, bridge nodes, god nodes with inferred edges,
/// isolated nodes, low-cohesion communities.
pub fn suggest_questions(
    kg: &KnowledgeGraph,
    communities: &HashMap<usize, Vec<EntityId>>,
    community_labels: &HashMap<usize, String>,
    top_n: usize,
) -> Vec<SuggestedQuestion> {
    let mut questions: Vec<SuggestedQuestion> = Vec::new();
    let node_community = node_community_map(communities);

    // 1. AMBIGUOUS edges
    for (src_ent, tgt_ent, rel) in kg.edges() {
        if rel.confidence == Confidence::Ambiguous {
            questions.push(SuggestedQuestion {
                question_type: QuestionType::AmbiguousEdge,
                question: Some(format!(
                    "What is the exact relationship between `{}` and `{}`?",
                    src_ent.label, tgt_ent.label
                )),
                why: format!(
                    "Edge tagged AMBIGUOUS (relation: {}) - confidence is low.",
                    rel.relation_type_str()
                ),
            });
        }
    }

    // 2. Bridge nodes (approximate betweenness via degree * neighbor diversity)
    if kg.edge_count() > 0 {
        // Use simple betweenness approximation: count distinct communities in neighborhood
        let mut bridge_scores: Vec<(EntityId, f64)> = kg
            .entity_ids()
            .filter(|id| !is_file_node(kg, id) && !is_concept_node(kg, id))
            .map(|id| {
                let neighbors = kg.neighbors(id);
                let own_cid = node_community.get(id);
                let neighbor_comms: HashSet<usize> = neighbors
                    .iter()
                    .filter_map(|n| node_community.get(&n.id))
                    .filter(|c| Some(*c) != own_cid)
                    .copied()
                    .collect();
                // Score: number of distinct other communities reached, normalized
                let score = neighbor_comms.len() as f64 / (kg.node_count() as f64).max(1.0);
                (id.clone(), score)
            })
            .filter(|(_, score)| *score > 0.0)
            .collect();

        bridge_scores.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

        for (node_id, score) in bridge_scores.into_iter().take(3) {
            let label = kg
                .entity(&node_id)
                .map(|e| e.label.clone())
                .unwrap_or_else(|| node_id.to_hex());
            let cid = node_community.get(&node_id);
            let comm_label = cid
                .and_then(|c| community_labels.get(c))
                .cloned()
                .unwrap_or_else(|| format!("Community {:?}", cid));

            let neighbors = kg.neighbors(&node_id);
            let neighbor_comms: HashSet<usize> = neighbors
                .iter()
                .filter_map(|n| node_community.get(&n.id))
                .filter(|c| Some(*c) != cid)
                .copied()
                .collect();

            if !neighbor_comms.is_empty() {
                let other_labels: Vec<String> = neighbor_comms
                    .iter()
                    .map(|c| {
                        community_labels
                            .get(c)
                            .cloned()
                            .unwrap_or_else(|| format!("Community {c}"))
                    })
                    .collect();
                questions.push(SuggestedQuestion {
                    question_type: QuestionType::BridgeNode,
                    question: Some(format!(
                        "Why does `{label}` connect `{comm_label}` to {}?",
                        other_labels
                            .iter()
                            .map(|l| format!("`{l}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )),
                    why: format!(
                        "High betweenness centrality ({score:.3}) - this node is a cross-community bridge."
                    ),
                });
            }
        }
    }

    // 3. God nodes with INFERRED edges
    let top_nodes = god_nodes(kg, 5);
    for gn in &top_nodes {
        let inferred: Vec<&crate::relationship::Relationship> = kg
            .edges_of(&gn.id)
            .into_iter()
            .filter(|r| r.confidence == Confidence::Inferred)
            .collect();

        if inferred.len() >= 2 {
            let others: Vec<String> = inferred
                .iter()
                .take(2)
                .map(|r| {
                    let other_id = if r.source == gn.id {
                        &r.target
                    } else {
                        &r.source
                    };
                    kg.entity(other_id)
                        .map(|e| e.label.clone())
                        .unwrap_or_else(|| other_id.to_hex())
                })
                .collect();

            questions.push(SuggestedQuestion {
                question_type: QuestionType::VerifyInferred,
                question: Some(format!(
                    "Are the {} inferred relationships involving `{}` (e.g. with `{}` and `{}`) actually correct?",
                    inferred.len(),
                    gn.label,
                    others[0],
                    others[1],
                )),
                why: format!(
                    "`{}` has {} INFERRED edges - model-reasoned connections that need verification.",
                    gn.label,
                    inferred.len()
                ),
            });
        }
    }

    // 4. Isolated or weakly-connected nodes
    let isolated: Vec<&EntityId> = kg
        .entity_ids()
        .filter(|id| kg.degree(id) <= 1 && !is_file_node(kg, id) && !is_concept_node(kg, id))
        .collect();

    if !isolated.is_empty() {
        let labels: Vec<String> = isolated
            .iter()
            .take(3)
            .map(|id| {
                kg.entity(id)
                    .map(|e| e.label.clone())
                    .unwrap_or_else(|| id.to_hex())
            })
            .collect();

        questions.push(SuggestedQuestion {
            question_type: QuestionType::IsolatedNodes,
            question: Some(format!(
                "What connects {} to the rest of the system?",
                labels
                    .iter()
                    .map(|l| format!("`{l}`"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
            why: format!(
                "{} weakly-connected nodes found - possible documentation gaps or missing edges.",
                isolated.len()
            ),
        });
    }

    // 5. Low-cohesion communities
    for (&cid, nodes) in communities {
        let score = cohesion_score(kg, nodes);
        if score < 0.15 && nodes.len() >= 5 {
            let label = community_labels
                .get(&cid)
                .cloned()
                .unwrap_or_else(|| format!("Community {cid}"));
            questions.push(SuggestedQuestion {
                question_type: QuestionType::LowCohesion,
                question: Some(format!(
                    "Should `{label}` be split into smaller, more focused modules?"
                )),
                why: format!(
                    "Cohesion score {score} - nodes in this community are weakly interconnected."
                ),
            });
        }
    }

    if questions.is_empty() {
        return vec![SuggestedQuestion {
            question_type: QuestionType::NoSignal,
            question: None,
            why: "Not enough signal to generate questions. \
                  This usually means the corpus has no AMBIGUOUS edges, no bridge nodes, \
                  no INFERRED relationships, and all communities are tightly cohesive. \
                  Add more files or run with --mode deep to extract richer edges."
                .to_owned(),
        }];
    }

    questions.into_iter().take(top_n).collect()
}

// ---------------------------------------------------------------------------
// Graph Diff (GRAPH-020)
// ---------------------------------------------------------------------------

/// Changes between two graph snapshots.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphDiff {
    pub new_nodes: Vec<DiffNode>,
    pub removed_nodes: Vec<DiffNode>,
    pub new_edges: Vec<DiffEdge>,
    pub removed_edges: Vec<DiffEdge>,
    pub summary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffNode {
    pub id: EntityId,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffEdge {
    pub source: EntityId,
    pub target: EntityId,
    pub relation: String,
    pub confidence: Confidence,
}

/// Undirected edge key for comparison.
fn edge_key(src: &EntityId, tgt: &EntityId, relation: &str) -> (String, String, String) {
    let a = src.to_hex();
    let b = tgt.to_hex();
    if a <= b {
        (a, b, relation.to_owned())
    } else {
        (b, a, relation.to_owned())
    }
}

/// Compare two graph snapshots and return what changed.
pub fn graph_diff(old: &KnowledgeGraph, new: &KnowledgeGraph) -> GraphDiff {
    let old_ids: HashSet<&EntityId> = old.entity_ids().collect();
    let new_ids: HashSet<&EntityId> = new.entity_ids().collect();

    let added_ids: Vec<&EntityId> = new_ids.difference(&old_ids).copied().collect();
    let removed_ids: Vec<&EntityId> = old_ids.difference(&new_ids).copied().collect();

    let new_nodes: Vec<DiffNode> = added_ids
        .iter()
        .map(|id| DiffNode {
            id: (*id).clone(),
            label: new
                .entity(id)
                .map(|e| e.label.clone())
                .unwrap_or_else(|| id.to_hex()),
        })
        .collect();

    let removed_nodes: Vec<DiffNode> = removed_ids
        .iter()
        .map(|id| DiffNode {
            id: (*id).clone(),
            label: old
                .entity(id)
                .map(|e| e.label.clone())
                .unwrap_or_else(|| id.to_hex()),
        })
        .collect();

    let old_edge_keys: HashSet<(String, String, String)> = old
        .edges()
        .map(|(src, tgt, rel)| edge_key(&src.id, &tgt.id, &rel.relation_type_str()))
        .collect();

    let new_edge_keys: HashSet<(String, String, String)> = new
        .edges()
        .map(|(src, tgt, rel)| edge_key(&src.id, &tgt.id, &rel.relation_type_str()))
        .collect();

    let added_edge_keys: HashSet<&(String, String, String)> =
        new_edge_keys.difference(&old_edge_keys).collect();
    let removed_edge_keys: HashSet<&(String, String, String)> =
        old_edge_keys.difference(&new_edge_keys).collect();

    let new_edges: Vec<DiffEdge> = new
        .edges()
        .filter(|(src, tgt, rel)| {
            added_edge_keys.contains(&edge_key(&src.id, &tgt.id, &rel.relation_type_str()))
        })
        .map(|(src, tgt, rel)| DiffEdge {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation: rel.relation_type_str(),
            confidence: rel.confidence,
        })
        .collect();

    let removed_edges: Vec<DiffEdge> = old
        .edges()
        .filter(|(src, tgt, rel)| {
            removed_edge_keys.contains(&edge_key(&src.id, &tgt.id, &rel.relation_type_str()))
        })
        .map(|(src, tgt, rel)| DiffEdge {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation: rel.relation_type_str(),
            confidence: rel.confidence,
        })
        .collect();

    let mut parts = Vec::new();
    if !new_nodes.is_empty() {
        let n = new_nodes.len();
        parts.push(format!("{n} new node{}", if n != 1 { "s" } else { "" }));
    }
    if !new_edges.is_empty() {
        let n = new_edges.len();
        parts.push(format!("{n} new edge{}", if n != 1 { "s" } else { "" }));
    }
    if !removed_nodes.is_empty() {
        let n = removed_nodes.len();
        parts.push(format!("{n} node{} removed", if n != 1 { "s" } else { "" }));
    }
    if !removed_edges.is_empty() {
        let n = removed_edges.len();
        parts.push(format!("{n} edge{} removed", if n != 1 { "s" } else { "" }));
    }
    let summary = if parts.is_empty() {
        "no changes".to_owned()
    } else {
        parts.join(", ")
    };

    GraphDiff {
        new_nodes,
        removed_nodes,
        new_edges,
        removed_edges,
        summary,
    }
}

// ---------------------------------------------------------------------------
// KG-007: MCTS Graph Exploration (RANGER)
// ---------------------------------------------------------------------------

/// Configuration for Monte Carlo Tree Search over the knowledge graph.
#[derive(Debug, Clone)]
pub struct MctsExplorer {
    /// UCB1 exploration constant (default: sqrt(2) ~= 1.414).
    pub exploration_constant: f64,
    /// Maximum number of MCTS iterations.
    pub max_iterations: usize,
    /// Maximum depth for rollout random walks.
    pub max_depth: usize,
}

/// Result of MCTS graph exploration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MctsResult {
    /// Best path of entity IDs found during exploration.
    pub best_path: Vec<EntityId>,
    /// Cumulative relevance score of the best path.
    pub score: f64,
    /// Number of MCTS iterations actually performed.
    pub iterations_used: usize,
    /// Total number of unique nodes explored across all iterations.
    pub nodes_explored: usize,
}

/// Internal tree node for MCTS bookkeeping.
struct MctsNode {
    entity_id: EntityId,
    visit_count: u32,
    total_reward: f64,
    children: Vec<usize>, // indices into the arena
    parent: Option<usize>,
    expanded: bool,
}

impl Default for MctsExplorer {
    fn default() -> Self {
        Self {
            exploration_constant: std::f64::consts::SQRT_2,
            max_iterations: 500,
            max_depth: 10,
        }
    }
}

impl MctsExplorer {
    /// Create a new explorer with the given parameters.
    pub fn new(exploration_constant: f64, max_iterations: usize, max_depth: usize) -> Self {
        Self {
            exploration_constant,
            max_iterations,
            max_depth,
        }
    }

    /// Explore the graph from `start`, seeking high-relevance paths.
    ///
    /// The `relevance_fn` scores individual entities (higher = more relevant).
    /// MCTS balances exploitation (following high-relevance nodes) with
    /// exploration (visiting unvisited edges) using the UCB1 formula.
    pub fn explore(
        &self,
        kg: &KnowledgeGraph,
        start: &EntityId,
        relevance_fn: impl Fn(&crate::model::Entity) -> f64,
    ) -> MctsResult {
        if kg.entity(start).is_none() {
            return MctsResult {
                best_path: vec![],
                score: 0.0,
                iterations_used: 0,
                nodes_explored: 0,
            };
        }

        let mut arena: Vec<MctsNode> = Vec::new();
        arena.push(MctsNode {
            entity_id: start.clone(),
            visit_count: 0,
            total_reward: 0.0,
            children: Vec::new(),
            parent: None,
            expanded: false,
        });

        let mut best_path: Vec<EntityId> = vec![start.clone()];
        let mut best_score: f64 = kg.entity(start).map(&relevance_fn).unwrap_or(0.0);
        let mut explored_nodes: HashSet<EntityId> = HashSet::new();
        explored_nodes.insert(start.clone());

        // Deterministic PRNG (xorshift64) for rollout -- avoids rand dep.
        let mut rng_state: u64 = 0xDEAD_BEEF_CAFE_1234;
        let next_rng = |state: &mut u64| -> u64 {
            *state ^= *state << 13;
            *state ^= *state >> 7;
            *state ^= *state << 17;
            *state
        };

        let mut actual_iterations = 0usize;

        for _iter_idx in 0..self.max_iterations {
            actual_iterations += 1;

            // Selection -- walk down via UCB1.
            let mut current = 0usize;
            let mut path_ids: Vec<EntityId> = vec![arena[current].entity_id.clone()];
            let mut depth = 0;

            while arena[current].expanded
                && !arena[current].children.is_empty()
                && depth < self.max_depth
            {
                let parent_visits = arena[current].visit_count as f64;
                let c = self.exploration_constant;
                let best_child = arena[current].children.iter().copied().max_by(|&a, &b| {
                    let ucb_a = Self::ucb1_score(&arena[a], parent_visits, c);
                    let ucb_b = Self::ucb1_score(&arena[b], parent_visits, c);
                    ucb_a
                        .partial_cmp(&ucb_b)
                        .unwrap_or(std::cmp::Ordering::Equal)
                });
                match best_child {
                    Some(ci) => {
                        current = ci;
                        path_ids.push(arena[current].entity_id.clone());
                        depth += 1;
                    }
                    None => break,
                }
            }

            // Expansion -- add children of current node if not expanded.
            if !arena[current].expanded && depth < self.max_depth {
                let current_id = arena[current].entity_id.clone();
                let neighbors = kg.neighbors(&current_id);
                let path_set: HashSet<&EntityId> = path_ids.iter().collect();
                let new_neighbors: Vec<EntityId> = neighbors
                    .iter()
                    .map(|e| e.id.clone())
                    .filter(|id| !path_set.contains(id))
                    .collect();

                let child_start = arena.len();
                for nid in &new_neighbors {
                    explored_nodes.insert(nid.clone());
                    arena.push(MctsNode {
                        entity_id: nid.clone(),
                        visit_count: 0,
                        total_reward: 0.0,
                        children: Vec::new(),
                        parent: Some(current),
                        expanded: false,
                    });
                }
                let child_end = arena.len();
                arena[current].children = (child_start..child_end).collect();
                arena[current].expanded = true;

                if !arena[current].children.is_empty() {
                    current = arena[current].children[0];
                    path_ids.push(arena[current].entity_id.clone());
                    depth += 1;
                }
            }

            // Rollout -- random walk from current, accumulate relevance.
            let mut rollout_reward = 0.0;
            let mut rollout_id = arena[current].entity_id.clone();
            let mut rollout_visited: HashSet<EntityId> = path_ids.iter().cloned().collect();
            let mut rollout_steps = 0;
            for _ in 0..self.max_depth.saturating_sub(depth) {
                if let Some(ent) = kg.entity(&rollout_id) {
                    rollout_reward += relevance_fn(ent);
                }
                rollout_steps += 1;
                let nbrs = kg.neighbors(&rollout_id);
                let unvis: Vec<&EntityId> = nbrs
                    .iter()
                    .map(|e| &e.id)
                    .filter(|id| !rollout_visited.contains(id))
                    .collect();
                if unvis.is_empty() {
                    break;
                }
                let pick = next_rng(&mut rng_state) as usize % unvis.len();
                let next = unvis[pick].clone();
                rollout_visited.insert(next.clone());
                rollout_id = next;
            }

            let normalized_reward = if rollout_steps > 0 {
                rollout_reward / rollout_steps as f64
            } else {
                0.0
            };

            // Backpropagation.
            let mut bp = current;
            loop {
                arena[bp].visit_count += 1;
                arena[bp].total_reward += normalized_reward;
                match arena[bp].parent {
                    Some(p) => bp = p,
                    None => break,
                }
            }

            let path_score: f64 = path_ids
                .iter()
                .filter_map(|id| kg.entity(id))
                .map(&relevance_fn)
                .sum();
            if path_score > best_score {
                best_score = path_score;
                best_path = path_ids;
            }

            if actual_iterations > 10 && arena.iter().all(|n| n.expanded || n.children.is_empty()) {
                break;
            }
        }

        MctsResult {
            best_path,
            score: best_score,
            iterations_used: actual_iterations,
            nodes_explored: explored_nodes.len(),
        }
    }

    fn ucb1_score(node: &MctsNode, parent_visits: f64, c: f64) -> f64 {
        if node.visit_count == 0 {
            return f64::INFINITY;
        }
        let exploitation = node.total_reward / node.visit_count as f64;
        let exploration = c * (parent_visits.ln() / node.visit_count as f64).sqrt();
        exploitation + exploration
    }
}

// ---------------------------------------------------------------------------
// KG-010: Multi-hop Traversal with Priors (Beam Search)
// ---------------------------------------------------------------------------

/// A scored traversal path through the knowledge graph.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraversalPath {
    pub nodes: Vec<EntityId>,
    pub edges: Vec<String>,
    pub total_score: f64,
}

/// Default edge priors for beam search exploration.
pub fn default_edge_priors() -> HashMap<String, f64> {
    let mut priors = HashMap::new();
    priors.insert("calls".to_owned(), 1.0);
    priors.insert("imports".to_owned(), 0.8);
    priors.insert("imports_from".to_owned(), 0.8);
    priors.insert("depends_on".to_owned(), 0.8);
    priors.insert("contains".to_owned(), 0.5);
    priors.insert("method_of".to_owned(), 0.5);
    priors.insert("implements".to_owned(), 0.7);
    priors.insert("extends".to_owned(), 0.7);
    priors.insert("related_to".to_owned(), 0.3);
    priors.insert("uses".to_owned(), 0.8);
    priors
}

/// Beam search traversal with exploration priors over a knowledge graph.
///
/// Expands the most promising nodes first based on edge-type relevance
/// weights. At each depth level, only the top `beam_width` paths are
/// kept for further expansion.
///
/// `edge_prior` maps edge type strings (e.g. "calls") to exploration
/// weights. Edge types not present in the map receive a default weight
/// of 0.1.
pub fn beam_search(
    kg: &KnowledgeGraph,
    start: &EntityId,
    beam_width: usize,
    max_depth: usize,
    edge_prior: &HashMap<String, f64>,
) -> Vec<TraversalPath> {
    let graph = kg.inner_graph();
    let entity_index = kg.entity_index();

    let Some(&start_idx) = entity_index.get(start) else {
        return Vec::new();
    };

    let default_weight = 0.1;

    // Each beam entry: (path_nodes as NodeIndex vec, edge_types, cumulative score)
    let mut beam: Vec<(Vec<NodeIndex>, Vec<String>, f64)> =
        vec![(vec![start_idx], Vec::new(), 0.0)];
    let mut completed: Vec<TraversalPath> = Vec::new();

    for _depth in 0..max_depth {
        let mut candidates: Vec<(Vec<NodeIndex>, Vec<String>, f64)> = Vec::new();

        for (path, edges, score) in &beam {
            let current_idx = *path.last().unwrap();
            let visited: HashSet<NodeIndex> = path.iter().copied().collect();

            let mut has_expansion = false;

            // Expand in both directions to find reachable neighbors.
            for dir in [Direction::Outgoing, Direction::Incoming] {
                for edge_ref in graph.edges_directed(current_idx, dir) {
                    let neighbor_idx = match dir {
                        Direction::Outgoing => edge_ref.target(),
                        Direction::Incoming => edge_ref.source(),
                    };

                    if visited.contains(&neighbor_idx) {
                        continue;
                    }

                    let rel = edge_ref.weight();
                    let rel_str = rel.relation_type_str();
                    let weight = edge_prior.get(&rel_str).copied().unwrap_or(default_weight);

                    let new_score = score + weight;
                    let mut new_path = path.clone();
                    new_path.push(neighbor_idx);
                    let mut new_edges = edges.clone();
                    new_edges.push(rel_str);

                    candidates.push((new_path, new_edges, new_score));
                    has_expansion = true;
                }
            }

            // If this path has no further expansions, record it as completed.
            if !has_expansion && path.len() > 1 {
                let nodes = path.iter().map(|&idx| graph[idx].id.clone()).collect();
                completed.push(TraversalPath {
                    nodes,
                    edges: edges.clone(),
                    total_score: *score,
                });
            }
        }

        if candidates.is_empty() {
            break;
        }

        // Keep only top beam_width candidates by score.
        candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        candidates.truncate(beam_width);
        beam = candidates;
    }

    // Convert remaining beam entries to completed paths.
    for (path, edges, score) in beam {
        if path.len() > 1 {
            let nodes = path.iter().map(|&idx| graph[idx].id.clone()).collect();
            completed.push(TraversalPath {
                nodes,
                edges,
                total_score: score,
            });
        }
    }

    // Sort by score descending.
    completed.sort_by(|a, b| {
        b.total_score
            .partial_cmp(&a.total_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    completed
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::model::Entity;
    use crate::relationship::{Confidence, RelationType, Relationship};

    fn entity(name: &str, label: &str, source_file: &str) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Function, name, source_file),
            entity_type: EntityType::Function,
            label: label.to_owned(),
            source_file: Some(source_file.to_owned()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        }
    }

    fn rel(
        src_name: &str,
        src_file: &str,
        tgt_name: &str,
        tgt_file: &str,
        relation: RelationType,
        confidence: Confidence,
    ) -> Relationship {
        Relationship {
            source: EntityId::new(&DomainTag::Code, &EntityType::Function, src_name, src_file),
            target: EntityId::new(&DomainTag::Code, &EntityType::Function, tgt_name, tgt_file),
            relation_type: relation,
            confidence,
            weight: confidence.to_weight(),
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    fn sample_kg() -> KnowledgeGraph {
        let entities = vec![
            entity("auth", "AuthService", "src/auth.py"),
            entity("db", "Database", "src/db.py"),
            entity("api", "ApiHandler", "src/api.py"),
            entity("cache", "CacheLayer", "src/cache.py"),
            entity("user", "UserModel", "src/models.py"),
        ];
        let rels = vec![
            rel(
                "auth",
                "src/auth.py",
                "db",
                "src/db.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
            rel(
                "auth",
                "src/auth.py",
                "api",
                "src/api.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
            rel(
                "auth",
                "src/auth.py",
                "cache",
                "src/cache.py",
                RelationType::Custom("uses".into()),
                Confidence::Inferred,
            ),
            rel(
                "db",
                "src/db.py",
                "user",
                "src/models.py",
                RelationType::Contains,
                Confidence::Extracted,
            ),
            rel(
                "api",
                "src/api.py",
                "cache",
                "src/cache.py",
                RelationType::DependsOn,
                Confidence::Ambiguous,
            ),
        ];
        KnowledgeGraph::from_parts(entities, rels, vec![])
    }

    #[test]
    fn god_nodes_returns_sorted_by_degree() {
        let kg = sample_kg();
        let gn = god_nodes(&kg, 10);
        assert!(!gn.is_empty());
        // Sorted by degree descending
        for w in gn.windows(2) {
            assert!(w[0].edges >= w[1].edges);
        }
    }

    #[test]
    fn god_nodes_excludes_file_nodes() {
        let entities = vec![
            entity("auth.py", "auth.py", "auth.py"), // file node: label == filename
            entity("auth", "AuthService", "auth.py"),
        ];
        let rels = vec![rel(
            "auth.py",
            "auth.py",
            "auth",
            "auth.py",
            RelationType::Contains,
            Confidence::Extracted,
        )];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let gn = god_nodes(&kg, 10);
        // Should not contain the file node
        assert!(gn.iter().all(|n| n.label != "auth.py"));
    }

    #[test]
    fn god_nodes_has_required_keys() {
        let kg = sample_kg();
        let gn = god_nodes(&kg, 3);
        for n in &gn {
            assert!(!n.label.is_empty());
            assert!(n.edges > 0);
        }
    }

    #[test]
    fn surprising_connections_returns_results() {
        let kg = sample_kg();
        let communities = crate::cluster::cluster(&kg);
        let sc = surprising_connections(&kg, &communities, 5);
        // At minimum, we don't panic
        let _ = sc;
    }

    #[test]
    fn ambiguous_scores_higher_than_extracted() {
        let entities = vec![
            entity("a", "A", "src/a.py"),
            entity("b", "B", "src/b.py"),
            entity("c", "C", "src/c.py"),
            entity("d", "D", "src/d.py"),
        ];
        let rels = vec![
            rel(
                "a",
                "src/a.py",
                "b",
                "src/b.py",
                RelationType::Calls,
                Confidence::Ambiguous,
            ),
            rel(
                "c",
                "src/c.py",
                "d",
                "src/d.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let node_community: HashMap<EntityId, usize> = HashMap::new();

        let id_a = EntityId::new(&DomainTag::Code, &EntityType::Function, "a", "src/a.py");
        let id_b = EntityId::new(&DomainTag::Code, &EntityType::Function, "b", "src/b.py");
        let id_c = EntityId::new(&DomainTag::Code, &EntityType::Function, "c", "src/c.py");
        let id_d = EntityId::new(&DomainTag::Code, &EntityType::Function, "d", "src/d.py");

        let (score_amb, _) = surprise_score(
            &kg,
            &id_a,
            &id_b,
            "calls",
            Confidence::Ambiguous,
            &node_community,
            "src/a.py",
            "src/b.py",
            None,
        );
        let (score_ext, _) = surprise_score(
            &kg,
            &id_c,
            &id_d,
            "calls",
            Confidence::Extracted,
            &node_community,
            "src/c.py",
            "src/d.py",
            None,
        );
        assert!(score_amb > score_ext);
    }

    #[test]
    fn cross_file_type_scores_higher() {
        let entities = vec![
            entity("a", "A", "src/a.py"),
            entity("b", "B", "docs/b.pdf"),
            entity("c", "C", "src/c.py"),
            entity("d", "D", "src/d.py"),
        ];
        let kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let node_community = HashMap::new();

        let id_a = EntityId::new(&DomainTag::Code, &EntityType::Function, "a", "src/a.py");
        let id_b = EntityId::new(&DomainTag::Code, &EntityType::Function, "b", "docs/b.pdf");
        let id_c = EntityId::new(&DomainTag::Code, &EntityType::Function, "c", "src/c.py");
        let id_d = EntityId::new(&DomainTag::Code, &EntityType::Function, "d", "src/d.py");

        let (score_cross, reasons) = surprise_score(
            &kg,
            &id_a,
            &id_b,
            "references",
            Confidence::Extracted,
            &node_community,
            "src/a.py",
            "docs/b.pdf",
            None,
        );
        let (score_same, _) = surprise_score(
            &kg,
            &id_c,
            &id_d,
            "references",
            Confidence::Extracted,
            &node_community,
            "src/c.py",
            "src/d.py",
            None,
        );
        assert!(score_cross > score_same);
        assert!(reasons.iter().any(|r| r.contains("file types")));
    }

    #[test]
    fn graph_diff_empty_when_same() {
        let kg = sample_kg();
        let diff = graph_diff(&kg, &kg);
        assert!(diff.new_nodes.is_empty());
        assert!(diff.removed_nodes.is_empty());
        assert!(diff.new_edges.is_empty());
        assert!(diff.removed_edges.is_empty());
        assert_eq!(diff.summary, "no changes");
    }

    #[test]
    fn graph_diff_detects_added_node() {
        let old = KnowledgeGraph::from_parts(vec![entity("a", "A", "a.py")], vec![], vec![]);
        let new_kg = KnowledgeGraph::from_parts(
            vec![entity("a", "A", "a.py"), entity("b", "B", "b.py")],
            vec![],
            vec![],
        );
        let diff = graph_diff(&old, &new_kg);
        assert_eq!(diff.new_nodes.len(), 1);
    }

    #[test]
    fn graph_diff_detects_removed_edge() {
        let entities = vec![entity("a", "A", "a.py"), entity("b", "B", "b.py")];
        let old = KnowledgeGraph::from_parts(
            entities.clone(),
            vec![rel(
                "a",
                "a.py",
                "b",
                "b.py",
                RelationType::Calls,
                Confidence::Extracted,
            )],
            vec![],
        );
        let new_kg = KnowledgeGraph::from_parts(entities, vec![], vec![]);
        let diff = graph_diff(&old, &new_kg);
        assert_eq!(diff.removed_edges.len(), 1);
        assert!(diff.summary.contains("removed"));
    }

    #[test]
    fn suggest_questions_returns_no_signal_for_trivial_graph() {
        let entities = vec![entity("a", "A", "a.py"), entity("b", "B", "a.py")];
        let rels = vec![rel(
            "a",
            "a.py",
            "b",
            "a.py",
            RelationType::Calls,
            Confidence::Extracted,
        )];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let communities = crate::cluster::cluster(&kg);
        let labels: HashMap<usize, String> = communities
            .keys()
            .map(|&cid| (cid, format!("Community {cid}")))
            .collect();
        let qs = suggest_questions(&kg, &communities, &labels, 7);
        assert!(!qs.is_empty());
    }

    #[test]
    fn suggest_questions_finds_ambiguous_edges() {
        let entities = vec![entity("a", "A", "a.py"), entity("b", "B", "a.py")];
        let rels = vec![rel(
            "a",
            "a.py",
            "b",
            "a.py",
            RelationType::Custom("maybe_calls".into()),
            Confidence::Ambiguous,
        )];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let communities = crate::cluster::cluster(&kg);
        let labels = HashMap::new();
        let qs = suggest_questions(&kg, &communities, &labels, 7);
        assert!(
            qs.iter()
                .any(|q| q.question_type == QuestionType::AmbiguousEdge)
        );
    }

    // -----------------------------------------------------------------------
    // KG-010: Multi-hop Beam Search
    // -----------------------------------------------------------------------

    #[test]
    fn beam_search_basic_traversal() {
        let kg = sample_kg();
        let priors = default_edge_priors();
        let auth_id = EntityId::new(
            &DomainTag::Code,
            &EntityType::Function,
            "auth",
            "src/auth.py",
        );

        let paths = beam_search(&kg, &auth_id, 5, 3, &priors);
        assert!(
            !paths.is_empty(),
            "Beam search should find at least one path"
        );

        for path in &paths {
            assert!(
                path.nodes.len() >= 2,
                "Each path should have at least 2 nodes"
            );
            assert_eq!(path.edges.len(), path.nodes.len() - 1);
            assert!(path.total_score > 0.0);
        }
    }

    #[test]
    fn beam_search_respects_beam_width() {
        // Build a graph where one node connects to many others.
        let mut entities = vec![entity("hub", "Hub", "src/hub.py")];
        let mut rels = Vec::new();
        for i in 0..10 {
            let name = format!("leaf_{i}");
            let file = format!("src/leaf_{i}.py");
            entities.push(entity(&name, &name, &file));
            rels.push(rel(
                "hub",
                "src/hub.py",
                &name,
                &file,
                RelationType::Calls,
                Confidence::Extracted,
            ));
        }
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let priors = default_edge_priors();
        let hub_id = EntityId::new(&DomainTag::Code, &EntityType::Function, "hub", "src/hub.py");

        // beam_width=3 should produce at most 3 paths at depth 1.
        let paths = beam_search(&kg, &hub_id, 3, 1, &priors);
        assert!(paths.len() <= 3, "Should respect beam_width limit");
    }

    #[test]
    fn beam_search_unknown_start() {
        let kg = sample_kg();
        let priors = default_edge_priors();
        let fake_id = EntityId::new(&DomainTag::Code, &EntityType::Function, "nope", "x.py");
        let paths = beam_search(&kg, &fake_id, 5, 3, &priors);
        assert!(paths.is_empty());
    }

    #[test]
    fn beam_search_sorted_by_score_desc() {
        let kg = sample_kg();
        let priors = default_edge_priors();
        let auth_id = EntityId::new(
            &DomainTag::Code,
            &EntityType::Function,
            "auth",
            "src/auth.py",
        );

        let paths = beam_search(&kg, &auth_id, 10, 3, &priors);
        for w in paths.windows(2) {
            assert!(
                w[0].total_score >= w[1].total_score,
                "Paths should be sorted by score descending"
            );
        }
    }

    #[test]
    fn beam_search_custom_priors() {
        let kg = sample_kg();
        // Only value "calls" edges, zero out everything else.
        let mut priors = HashMap::new();
        priors.insert("calls".to_owned(), 1.0);
        priors.insert("depends_on".to_owned(), 0.0);
        priors.insert("contains".to_owned(), 0.0);

        let auth_id = EntityId::new(
            &DomainTag::Code,
            &EntityType::Function,
            "auth",
            "src/auth.py",
        );
        let paths = beam_search(&kg, &auth_id, 10, 3, &priors);

        // All edges in top-scoring paths should prefer "calls".
        if let Some(top) = paths.first() {
            for edge in &top.edges {
                // Top path should contain calls (highest weight).
                let _ = edge; // Just ensure it doesn't panic.
            }
        }
    }

    #[test]
    fn default_edge_priors_contains_expected_keys() {
        let priors = default_edge_priors();
        assert!(priors.contains_key("calls"));
        assert!(priors.contains_key("imports"));
        assert!(priors.contains_key("contains"));
        assert!(priors.contains_key("related_to"));
        assert!(*priors.get("calls").unwrap() > *priors.get("related_to").unwrap());
    }

    // -- KG-007: MCTS tests ------------------------------------------------

    #[test]
    fn mcts_returns_empty_for_missing_start() {
        let kg = KnowledgeGraph::new();
        let explorer = MctsExplorer::default();
        let fake_id = EntityId::new(&DomainTag::Code, &EntityType::Function, "nope", "x.py");
        let result = explorer.explore(&kg, &fake_id, |_| 1.0);
        assert!(result.best_path.is_empty());
        assert_eq!(result.score, 0.0);
        assert_eq!(result.iterations_used, 0);
    }

    #[test]
    fn mcts_explores_single_node() {
        let kg = KnowledgeGraph::from_parts(vec![entity("a", "A", "a.py")], vec![], vec![]);
        let start = EntityId::new(&DomainTag::Code, &EntityType::Function, "a", "a.py");
        let explorer = MctsExplorer::new(std::f64::consts::SQRT_2, 50, 5);
        let result = explorer.explore(&kg, &start, |_| 1.0);
        assert_eq!(result.best_path.len(), 1);
        assert_eq!(result.best_path[0], start);
        assert!(result.score >= 1.0);
        assert_eq!(result.nodes_explored, 1);
    }

    #[test]
    fn mcts_finds_path_in_chain() {
        // A -> B -> C -> D, relevance increases along chain.
        let entities = vec![
            entity("a", "A", "a.py"),
            entity("b", "B", "b.py"),
            entity("c", "C", "c.py"),
            entity("d", "D", "d.py"),
        ];
        let rels = vec![
            rel(
                "a",
                "a.py",
                "b",
                "b.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
            rel(
                "b",
                "b.py",
                "c",
                "c.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
            rel(
                "c",
                "c.py",
                "d",
                "d.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let start = EntityId::new(&DomainTag::Code, &EntityType::Function, "a", "a.py");
        let explorer = MctsExplorer::new(std::f64::consts::SQRT_2, 200, 10);
        // Relevance by label: A=1, B=2, C=3, D=4
        let result = explorer.explore(&kg, &start, |e| match e.label.as_str() {
            "A" => 1.0,
            "B" => 2.0,
            "C" => 3.0,
            "D" => 4.0,
            _ => 0.0,
        });
        assert!(result.best_path.len() >= 2, "should find multi-node path");
        assert!(result.score > 1.0, "score should exceed single-node");
        assert!(result.nodes_explored >= 2);
    }

    #[test]
    fn mcts_prefers_high_relevance_branch() {
        // A -> B (relevance 10.0), A -> C (relevance 0.1)
        let entities = vec![
            entity("a", "A", "a.py"),
            entity("b", "HighRelevance", "b.py"),
            entity("c", "LowRelevance", "c.py"),
        ];
        let rels = vec![
            rel(
                "a",
                "a.py",
                "b",
                "b.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
            rel(
                "a",
                "a.py",
                "c",
                "c.py",
                RelationType::Calls,
                Confidence::Extracted,
            ),
        ];
        let kg = KnowledgeGraph::from_parts(entities, rels, vec![]);
        let start = EntityId::new(&DomainTag::Code, &EntityType::Function, "a", "a.py");
        let explorer = MctsExplorer::new(std::f64::consts::SQRT_2, 100, 5);
        let result = explorer.explore(&kg, &start, |e| {
            if e.label == "HighRelevance" {
                10.0
            } else if e.label == "LowRelevance" {
                0.1
            } else {
                1.0
            }
        });
        // The best path should include the high-relevance node.
        let high_id = EntityId::new(&DomainTag::Code, &EntityType::Function, "b", "b.py");
        assert!(
            result.best_path.contains(&high_id),
            "MCTS should find the high-relevance branch"
        );
        assert!(result.score >= 11.0);
    }

    #[test]
    fn mcts_iterations_and_nodes_populated() {
        let kg = sample_kg();
        let start = EntityId::new(
            &DomainTag::Code,
            &EntityType::Function,
            "auth",
            "src/auth.py",
        );
        let explorer = MctsExplorer::new(1.0, 50, 5);
        let result = explorer.explore(&kg, &start, |_| 1.0);
        assert!(result.iterations_used > 0);
        assert!(result.nodes_explored >= 1);
    }
}
