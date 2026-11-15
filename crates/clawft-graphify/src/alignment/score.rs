//! Similarity signals for multi-repo entity alignment.
//!
//! Three orthogonal scores feed the EA-Agent fusion step:
//! - **Name / label** — normalized edit distance on snake_case labels
//! - **Embedding** — cosine similarity (external map or local char-trigram bag)
//! - **Structural** — Jaccard overlap of neighbor labels

use crate::entity::EntityId;
use crate::model::{Entity, KnowledgeGraph};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Config / score types
// ---------------------------------------------------------------------------

/// Method that dominated an alignment decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AlignmentMethod {
    /// Aligned primarily by label similarity.
    LabelMatch,
    /// Aligned primarily by embedding cosine similarity.
    EmbeddingMatch,
    /// Aligned primarily by structural (neighbor) similarity.
    StructuralMatch,
    /// Multiple signals contributed above their individual floors.
    Combined,
    /// An [`super::AlignmentRefiner`] adjusted the decision.
    AgentRefined,
}

/// Per-signal scores for one entity pair.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ScoreBreakdown {
    /// Normalized label similarity in `[0, 1]`.
    pub label: f64,
    /// Embedding cosine similarity in `[0, 1]` (mapped from `[-1, 1]`).
    pub embedding: f64,
    /// Neighbor-label Jaccard in `[0, 1]`.
    pub structural: f64,
    /// Weighted fusion in `[0, 1]`.
    pub combined: f64,
}

/// Compact evidence selected for a candidate pair (EA-Agent "triple filter").
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AlignmentEvidence {
    pub label_a: String,
    pub label_b: String,
    pub type_a: String,
    pub type_b: String,
    /// Neighbor labels present in both graphs for this pair.
    pub shared_neighbors: Vec<String>,
    /// Compact relation summary (sorted unique relation type names).
    pub relation_summary: String,
}

/// A single scored alignment between two knowledge graphs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityAlignment {
    pub entity_a: EntityId,
    pub entity_b: EntityId,
    /// Combined similarity / confidence in `[0, 1]`.
    pub similarity: f64,
    pub scores: ScoreBreakdown,
    pub method: AlignmentMethod,
    pub evidence: AlignmentEvidence,
}

/// Tuning parameters for entity alignment.
#[derive(Debug, Clone)]
pub struct AlignmentConfig {
    /// Max label/embedding candidates per entity (default: 10).
    pub max_candidates: usize,
    /// Minimum label similarity for the candidate shortlist (default: 0.45).
    pub label_candidate_threshold: f64,
    /// Minimum embedding similarity for the candidate shortlist (default: 0.55).
    pub embedding_candidate_threshold: f64,
    /// Label weight in the fused score (default: 0.45).
    pub label_weight: f64,
    /// Embedding weight in the fused score (default: 0.25).
    pub embedding_weight: f64,
    /// Structural weight in the fused score (default: 0.30).
    pub structural_weight: f64,
    /// Proposals at or above this confidence are auto-accepted (default: 0.92).
    pub auto_accept_threshold: f64,
    /// When true, only pair entities with the same [`crate::entity::EntityType`].
    pub require_same_type: bool,
    /// Dimension for the local char-trigram embedder (default: 64).
    pub local_embedding_dims: usize,
}

impl Default for AlignmentConfig {
    fn default() -> Self {
        Self {
            max_candidates: 10,
            label_candidate_threshold: 0.45,
            embedding_candidate_threshold: 0.55,
            label_weight: 0.45,
            embedding_weight: 0.25,
            structural_weight: 0.30,
            auto_accept_threshold: 0.92,
            require_same_type: true,
            local_embedding_dims: 64,
        }
    }
}

impl AlignmentConfig {
    /// Renormalize weights so they sum to 1.0 (no-op if sum is ~0).
    pub fn normalized_weights(&self) -> (f64, f64, f64) {
        let sum = self.label_weight + self.embedding_weight + self.structural_weight;
        if sum <= f64::EPSILON {
            return (1.0 / 3.0, 1.0 / 3.0, 1.0 / 3.0);
        }
        (
            self.label_weight / sum,
            self.embedding_weight / sum,
            self.structural_weight / sum,
        )
    }
}

// ---------------------------------------------------------------------------
// Embedding lookup
// ---------------------------------------------------------------------------

/// Optional external embedding source keyed by entity id.
///
/// Implementors may wrap HNSW / LLM embedders; tests use
/// [`MapEmbeddingLookup`] or rely on the local char-trigram fallback.
pub trait EmbeddingLookup {
    fn embedding(&self, id: &EntityId) -> Option<&[f32]>;
}

/// Simple map-backed [`EmbeddingLookup`].
#[derive(Debug, Default, Clone)]
pub struct MapEmbeddingLookup {
    pub map: HashMap<EntityId, Vec<f32>>,
}

impl EmbeddingLookup for MapEmbeddingLookup {
    fn embedding(&self, id: &EntityId) -> Option<&[f32]> {
        self.map.get(id).map(|v| v.as_slice())
    }
}

// ---------------------------------------------------------------------------
// Label normalization + edit distance
// ---------------------------------------------------------------------------

/// Normalize a label for multi-repo comparison.
///
/// `AuthService` / `auth-service` / `auth_service` all become `auth_service`.
pub fn normalize_label(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    let mut prev_was_sep = true;
    let mut prev_was_lower = false;
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() {
            if ch.is_ascii_uppercase() && prev_was_lower && !prev_was_sep {
                out.push('_');
            }
            out.push(ch.to_ascii_lowercase());
            prev_was_sep = false;
            prev_was_lower = ch.is_ascii_lowercase() || ch.is_ascii_digit();
        } else {
            if !prev_was_sep && !out.is_empty() {
                out.push('_');
            }
            prev_was_sep = true;
            prev_was_lower = false;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Normalized Levenshtein similarity in `[0, 1]` on normalized labels.
pub fn label_similarity(a: &str, b: &str) -> f64 {
    let a_n = normalize_label(a);
    let b_n = normalize_label(b);
    if a_n == b_n {
        return 1.0;
    }
    if a_n.is_empty() || b_n.is_empty() {
        return 0.0;
    }
    let dist = levenshtein(&a_n, &b_n);
    let max_len = a_n.chars().count().max(b_n.chars().count());
    1.0 - (dist as f64 / max_len as f64)
}

/// Classic Levenshtein distance (dynamic programming, O(n·m)).
pub(crate) fn levenshtein(a: &str, b: &str) -> usize {
    let a_chars: Vec<char> = a.chars().collect();
    let b_chars: Vec<char> = b.chars().collect();
    let n = a_chars.len();
    let m = b_chars.len();

    let mut prev: Vec<usize> = (0..=m).collect();
    let mut curr = vec![0usize; m + 1];

    for i in 1..=n {
        curr[0] = i;
        for j in 1..=m {
            let cost = if a_chars[i - 1] == b_chars[j - 1] {
                0
            } else {
                1
            };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[m]
}

// ---------------------------------------------------------------------------
// Local embedding (char-trigram bag) + cosine
// ---------------------------------------------------------------------------

/// Deterministic local embedding from text (char-trigram hash bag, L2-normalized).
///
/// Used when no external [`EmbeddingLookup`] vector is available for an entity.
pub fn local_label_embedding(text: &str, dims: usize) -> Vec<f32> {
    let dims = dims.max(8);
    let mut vec = vec![0.0f32; dims];
    let normed = normalize_label(text);
    let padded = format!("  {normed}  ");
    let chars: Vec<char> = padded.chars().collect();
    if chars.len() < 3 {
        return vec;
    }
    for window in chars.windows(3) {
        let tri: String = window.iter().collect();
        let h = fnv1a_32(tri.as_bytes()) as usize;
        let idx = h % dims;
        let sign = if (h / dims).is_multiple_of(2) {
            1.0
        } else {
            -1.0
        };
        vec[idx] += sign;
    }
    l2_normalize(&mut vec);
    vec
}

fn fnv1a_32(bytes: &[u8]) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for &b in bytes {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

fn l2_normalize(v: &mut [f32]) {
    let mut sum = 0.0f32;
    for x in v.iter() {
        sum += x * x;
    }
    if sum <= f32::EPSILON {
        return;
    }
    let inv = sum.sqrt().recip();
    for x in v.iter_mut() {
        *x *= inv;
    }
}

/// Cosine similarity mapped into `[0, 1]` (`(cos + 1) / 2`).
///
/// Returns 0.0 when either vector is empty or dimensions differ.
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.is_empty() || b.is_empty() || a.len() != b.len() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut na = 0.0f64;
    let mut nb = 0.0f64;
    for i in 0..a.len() {
        let x = f64::from(a[i]);
        let y = f64::from(b[i]);
        dot += x * y;
        na += x * x;
        nb += y * y;
    }
    if na <= f64::EPSILON || nb <= f64::EPSILON {
        return 0.0;
    }
    let cos = (dot / (na.sqrt() * nb.sqrt())).clamp(-1.0, 1.0);
    (cos + 1.0) / 2.0
}

/// Resolve embedding for an entity: external lookup first, else local bag of
/// `label + type + neighbor labels`.
pub(crate) fn entity_embedding(
    kg: &KnowledgeGraph,
    entity: &Entity,
    lookup: Option<&dyn EmbeddingLookup>,
    dims: usize,
) -> Vec<f32> {
    if let Some(v) = lookup.and_then(|l| l.embedding(&entity.id)) {
        return v.to_vec();
    }
    let mut text = format!(
        "{} {} {}",
        entity.label,
        entity.entity_type.discriminant(),
        entity.source_file.as_deref().unwrap_or("")
    );
    for n in kg.neighbors(&entity.id) {
        text.push(' ');
        text.push_str(&n.label);
    }
    local_label_embedding(&text, dims)
}

// ---------------------------------------------------------------------------
// Structural similarity
// ---------------------------------------------------------------------------

pub(crate) fn neighbor_labels(kg: &KnowledgeGraph, id: &EntityId) -> HashSet<String> {
    kg.neighbors(id)
        .into_iter()
        .map(|e| normalize_label(&e.label))
        .collect()
}

fn jaccard(a: &HashSet<String>, b: &HashSet<String>) -> f64 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        intersection as f64 / union as f64
    }
}

pub(crate) fn structural_similarity(
    kg_a: &KnowledgeGraph,
    id_a: &EntityId,
    kg_b: &KnowledgeGraph,
    id_b: &EntityId,
) -> (f64, Vec<String>) {
    let na = neighbor_labels(kg_a, id_a);
    let nb = neighbor_labels(kg_b, id_b);
    let mut shared: Vec<String> = na.intersection(&nb).cloned().collect();
    shared.sort();
    (jaccard(&na, &nb), shared)
}

pub(crate) fn relation_summary(kg: &KnowledgeGraph, id: &EntityId) -> String {
    let mut names: Vec<String> = kg
        .edges()
        .filter(|(s, t, _)| s.id == *id || t.id == *id)
        .map(|(_, _, r)| format!("{:?}", r.relation_type))
        .collect();
    names.sort();
    names.dedup();
    names.join(",")
}

pub(crate) fn build_evidence(
    ea: &Entity,
    eb: &Entity,
    shared_neighbors: Vec<String>,
    kg_a: &KnowledgeGraph,
    kg_b: &KnowledgeGraph,
) -> AlignmentEvidence {
    let ra = relation_summary(kg_a, &ea.id);
    let rb = relation_summary(kg_b, &eb.id);
    let relation_summary = if ra == rb { ra } else { format!("{ra}|{rb}") };
    AlignmentEvidence {
        label_a: ea.label.clone(),
        label_b: eb.label.clone(),
        type_a: ea.entity_type.discriminant().to_string(),
        type_b: eb.entity_type.discriminant().to_string(),
        shared_neighbors,
        relation_summary,
    }
}

pub(crate) fn fuse_scores(
    label: f64,
    embedding: f64,
    structural: f64,
    config: &AlignmentConfig,
) -> ScoreBreakdown {
    let (lw, ew, sw) = config.normalized_weights();
    let combined = lw * label + ew * embedding + sw * structural;
    ScoreBreakdown {
        label,
        embedding,
        structural,
        combined,
    }
}

pub(crate) fn classify_method(scores: &ScoreBreakdown) -> AlignmentMethod {
    let floors = [
        (scores.label, AlignmentMethod::LabelMatch, 0.75),
        (scores.embedding, AlignmentMethod::EmbeddingMatch, 0.75),
        (scores.structural, AlignmentMethod::StructuralMatch, 0.45),
    ];
    let strong: Vec<AlignmentMethod> = floors
        .iter()
        .filter(|(v, _, floor)| *v >= *floor)
        .map(|(_, method, _)| *method)
        .collect();
    if strong.len() >= 2 {
        AlignmentMethod::Combined
    } else if let Some(method) = strong.first() {
        *method
    } else if scores.label >= scores.embedding && scores.label >= scores.structural {
        AlignmentMethod::LabelMatch
    } else if scores.embedding >= scores.structural {
        AlignmentMethod::EmbeddingMatch
    } else {
        AlignmentMethod::StructuralMatch
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_pascal_and_snake() {
        assert_eq!(normalize_label("AuthService"), "auth_service");
        assert_eq!(normalize_label("auth_service"), "auth_service");
        assert_eq!(normalize_label("auth-service"), "auth_service");
    }

    #[test]
    fn label_similarity_identical_after_normalize() {
        assert!((label_similarity("AuthService", "auth_service") - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn label_similarity_typo() {
        let sim = label_similarity("auth_service", "auth_srvice");
        assert!(sim > 0.85, "got {sim}");
    }

    #[test]
    fn levenshtein_basic() {
        assert_eq!(levenshtein("kitten", "sitting"), 3);
        assert_eq!(levenshtein("", "abc"), 3);
        assert_eq!(levenshtein("abc", "abc"), 0);
    }

    #[test]
    fn local_embedding_cosine_similar_labels() {
        let a = local_label_embedding("auth_service", 64);
        let b = local_label_embedding("auth_service", 64);
        assert!((cosine_similarity(&a, &b) - 1.0).abs() < 1e-5);

        let c = local_label_embedding("auth_srvice", 64);
        let sim = cosine_similarity(&a, &c);
        assert!(sim > 0.7, "typo embedding sim={sim}");

        let d = local_label_embedding("zzzz_totally_unrelated_xyz", 64);
        let far = cosine_similarity(&a, &d);
        assert!(
            far < sim,
            "unrelated should be farther: far={far} sim={sim}"
        );
    }

    #[test]
    fn jaccard_partial() {
        let a: HashSet<String> = ["x", "y", "z"].iter().map(|s| s.to_string()).collect();
        let b: HashSet<String> = ["y", "z", "w"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard(&a, &b) - 0.5).abs() < f64::EPSILON);
    }

    #[test]
    fn jaccard_empty() {
        let a: HashSet<String> = HashSet::new();
        let b: HashSet<String> = HashSet::new();
        assert!(jaccard(&a, &b).abs() < f64::EPSILON);
    }

    #[test]
    fn cosine_dim_mismatch_is_zero() {
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 0.0]), 0.0);
    }
}
