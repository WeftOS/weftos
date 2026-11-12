//! LightRAG dual-level keyword retrieval (WEFT-517).
//!
//! Implements the dual-level retrieval idea from Guo et al. 2410.05779
//! (LightRAG): split a query into **low-level** entity keywords and
//! **high-level** theme / community keywords, then fuse hits from both
//! levels. Token cost is far lower than full GraphRAG community
//! summarization because we only emit keyword lists + top entity ids.
//!
//! # Feature surface
//!
//! Always compiled (no extra Cargo feature) — call sites opt in by using
//! [`dual_level_retrieve`] instead of full graph walks. Related prior
//! work in-tree: LightRAG P4 ([`crate::graph_rerank`]), P5
//! ([`crate::edge_embed`]).
//!
//! # Token-cost note
//!
//! See [`token_cost_estimate`] and the ADR note at
//! `docs/adr/adr-091-lightrag-dual-level.md`.

use std::collections::{HashMap, HashSet};

use crate::entity::EntityId;
use crate::model::KnowledgeGraph;

/// One hit from dual-level retrieval.
#[derive(Debug, Clone, PartialEq)]
pub struct DualLevelHit {
    /// Entity id.
    pub entity_id: EntityId,
    /// Fused score (low-level + high-level contributions).
    pub score: f64,
    /// How much came from entity-keyword matches.
    pub low_level_score: f64,
    /// How much came from community / theme keywords.
    pub high_level_score: f64,
}

/// Result of a dual-level retrieval pass.
#[derive(Debug, Clone)]
pub struct DualLevelResult {
    /// Ranked hits (descending score).
    pub hits: Vec<DualLevelHit>,
    /// Low-level keywords extracted from the query.
    pub low_keywords: Vec<String>,
    /// High-level keywords (community labels + abstract tokens).
    pub high_keywords: Vec<String>,
    /// Rough token-cost estimate for this retrieval path.
    pub token_cost: TokenCostEstimate,
}

/// Token-cost estimate comparing dual-level vs a GraphRAG-style dump.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenCostEstimate {
    /// Tokens for dual-level path (keywords + hit labels).
    pub dual_level_tokens: usize,
    /// Tokens if we dumped full community summaries (GraphRAG-ish).
    pub graphrag_tokens: usize,
}

impl TokenCostEstimate {
    /// Ratio `graphrag / dual_level` (how many × fewer tokens dual-level uses).
    pub fn savings_ratio(&self) -> f64 {
        if self.dual_level_tokens == 0 {
            return f64::INFINITY;
        }
        self.graphrag_tokens as f64 / self.dual_level_tokens as f64
    }
}

/// Stop words shared with conversation retrieval.
const STOP: &[&str] = &[
    "a", "an", "the", "is", "are", "was", "were", "be", "been", "being", "have", "has", "had",
    "do", "does", "did", "will", "would", "could", "should", "may", "might", "must", "shall",
    "can", "need", "dare", "ought", "used", "to", "of", "in", "for", "on", "with", "at", "by",
    "from", "as", "into", "through", "during", "before", "after", "above", "below", "between",
    "out", "off", "over", "under", "again", "further", "then", "once", "here", "there", "when",
    "where", "why", "how", "all", "each", "few", "more", "most", "other", "some", "such", "no",
    "nor", "not", "only", "own", "same", "so", "than", "too", "very", "just", "and", "but", "if",
    "or", "because", "until", "while", "about", "against", "what", "which", "who", "whom", "this",
    "that", "these", "those", "i", "me", "my", "we", "our", "you", "your", "he", "him", "his",
    "she", "her", "it", "its", "they", "them", "their",
];

/// High-level theme tokens that bias toward community labels.
const THEME_HINTS: &[&str] = &[
    "architecture",
    "security",
    "auth",
    "authentication",
    "network",
    "mesh",
    "storage",
    "memory",
    "pipeline",
    "governance",
    "kernel",
    "agent",
    "voice",
    "browser",
    "wasm",
    "graph",
    "cluster",
    "community",
    "module",
    "service",
    "api",
    "protocol",
    "config",
    "deploy",
    "overview",
    "design",
    "system",
    "subsystem",
];

/// Tokenize `text` into lowercase alphanumeric keywords (stop words removed).
pub fn tokenize_keywords(text: &str) -> Vec<String> {
    text.to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|s| !s.is_empty() && s.len() > 1 && !STOP.contains(s))
        .map(|s| s.to_string())
        .collect()
}

/// Split query keywords into low-level (entity-ish) and high-level (theme).
pub fn split_levels(keywords: &[String]) -> (Vec<String>, Vec<String>) {
    let mut low = Vec::new();
    let mut high = Vec::new();
    for kw in keywords {
        // Exact theme-hint match only — do not classify "AuthService" as
        // high merely because it contains the substring "auth".
        if THEME_HINTS.contains(&kw.as_str()) {
            high.push(kw.clone());
        } else {
            low.push(kw.clone());
        }
    }
    // Always promote at least one high-level token so dual-level is
    // meaningful even for pure entity queries: reuse the longest low token.
    if high.is_empty()
        && let Some(longest) = low.iter().max_by_key(|s| s.len())
    {
        high.push(longest.clone());
    }
    (low, high)
}

/// True when `haystack` and `needle` share a meaningful stem (prefix ≥ 4
/// chars, or either contains the other).
fn loose_match(haystack: &str, needle: &str) -> bool {
    if haystack.is_empty() || needle.is_empty() {
        return false;
    }
    if haystack.contains(needle) || needle.contains(haystack) {
        return true;
    }
    // Shared prefix of at least 4 chars (auth↔authentication, mesh↔meshing).
    let n = haystack
        .chars()
        .zip(needle.chars())
        .take_while(|(a, b)| a == b)
        .count();
    n >= 4
}

/// Dual-level keyword retrieval over a knowledge graph.
///
/// * **Low level** — score entities by keyword overlap with labels.
/// * **High level** — score entities by membership in communities whose
///   labels match theme keywords (or by neighbor community diversity).
///
/// `community_of` maps entity → community id; `community_labels` maps
/// community id → human label. When community data is empty, high-level
/// scoring falls back to label substring match against high keywords.
pub fn dual_level_retrieve(
    kg: &KnowledgeGraph,
    query: &str,
    community_of: &HashMap<EntityId, usize>,
    community_labels: &HashMap<usize, String>,
    top_k: usize,
) -> DualLevelResult {
    let all_kw = tokenize_keywords(query);
    let (low_keywords, high_keywords) = split_levels(&all_kw);

    let mut scores: HashMap<EntityId, (f64, f64)> = HashMap::new();

    // Low-level: entity label keyword overlap.
    for e in kg.entities() {
        let label_lower = e.label.to_lowercase();
        let label_tokens: HashSet<&str> = label_lower
            .split(|c: char| !c.is_alphanumeric() && c != '_')
            .filter(|s| !s.is_empty())
            .collect();
        let mut low = 0.0;
        for kw in &low_keywords {
            if label_tokens.contains(kw.as_str()) {
                low += 1.0;
            } else if label_lower.contains(kw.as_str()) {
                low += 0.5;
            }
        }
        if low > 0.0 {
            scores.entry(e.id.clone()).or_default().0 += low;
        }
    }

    // High-level: community label match → boost all members.
    let mut matching_comms: HashSet<usize> = HashSet::new();
    for (cid, label) in community_labels {
        let ll = label.to_lowercase();
        for kw in &high_keywords {
            if loose_match(&ll, kw) {
                matching_comms.insert(*cid);
                break;
            }
        }
        // Also accept any token of the community label that loosely
        // matches a high keyword (e.g. "auth" in label vs "authentication").
        for token in ll.split(|c: char| !c.is_alphanumeric()) {
            if token.len() < 3 {
                continue;
            }
            for kw in &high_keywords {
                if loose_match(token, kw) {
                    matching_comms.insert(*cid);
                }
            }
        }
    }
    for (eid, cid) in community_of {
        if matching_comms.contains(cid) {
            scores.entry(eid.clone()).or_default().1 += 1.0;
        }
    }

    // Fallback high-level: direct label match on high keywords when no
    // community labels hit.
    if matching_comms.is_empty() {
        for e in kg.entities() {
            let label_lower = e.label.to_lowercase();
            let mut high = 0.0;
            for kw in &high_keywords {
                if label_lower.contains(kw.as_str()) {
                    high += 0.75;
                }
            }
            if high > 0.0 {
                scores.entry(e.id.clone()).or_default().1 += high;
            }
        }
    }

    let mut hits: Vec<DualLevelHit> = scores
        .into_iter()
        .map(
            |(entity_id, (low_level_score, high_level_score))| DualLevelHit {
                entity_id,
                score: low_level_score + 0.8 * high_level_score,
                low_level_score,
                high_level_score,
            },
        )
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(top_k.max(1));

    let token_cost = token_cost_estimate(
        &low_keywords,
        &high_keywords,
        &hits,
        kg,
        community_labels.len(),
    );

    DualLevelResult {
        hits,
        low_keywords,
        high_keywords,
        token_cost,
    }
}

/// Estimate dual-level vs GraphRAG token cost for a representative pass.
///
/// Heuristic (not a tokenizer): ~1 token per 4 chars of keyword/label
/// text. GraphRAG path assumes ~250 tokens per community summary.
pub fn token_cost_estimate(
    low_keywords: &[String],
    high_keywords: &[String],
    hits: &[DualLevelHit],
    kg: &KnowledgeGraph,
    community_count: usize,
) -> TokenCostEstimate {
    let mut dual_chars = 0usize;
    for k in low_keywords.iter().chain(high_keywords.iter()) {
        dual_chars += k.len();
    }
    for h in hits {
        if let Some(e) = kg.entity(&h.entity_id) {
            dual_chars += e.label.len();
        }
    }
    let dual_level_tokens = (dual_chars / 4).max(1);
    let graphrag_tokens = community_count.max(1) * 250;
    TokenCostEstimate {
        dual_level_tokens,
        graphrag_tokens,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::model::{Entity, KnowledgeGraph};

    fn entity(name: &str) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Module, name, "test.rs"),
            entity_type: EntityType::Module,
            label: name.to_string(),
            source_file: Some("test.rs".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        }
    }

    fn sample_kg() -> (
        KnowledgeGraph,
        HashMap<EntityId, usize>,
        HashMap<usize, String>,
    ) {
        let mut kg = KnowledgeGraph::new();
        let auth = entity("AuthService");
        let mesh = entity("MeshRuntime");
        let gate = entity("GovernanceGate");
        let auth_id = auth.id.clone();
        let mesh_id = mesh.id.clone();
        let gate_id = gate.id.clone();
        kg.add_entity(auth);
        kg.add_entity(mesh);
        kg.add_entity(gate);

        let mut community_of = HashMap::new();
        community_of.insert(auth_id, 0);
        community_of.insert(gate_id, 0);
        community_of.insert(mesh_id, 1);

        let mut labels = HashMap::new();
        labels.insert(0, "security / auth subsystem".into());
        labels.insert(1, "mesh networking".into());

        (kg, community_of, labels)
    }

    #[test]
    fn dual_level_finds_auth_via_theme() {
        let (kg, community_of, labels) = sample_kg();
        let res = dual_level_retrieve(
            &kg,
            "how does authentication architecture work?",
            &community_of,
            &labels,
            5,
        );
        assert!(
            !res.hits.is_empty(),
            "expected hits, got none; high={:?} low={:?}",
            res.high_keywords,
            res.low_keywords
        );
        // AuthService or GovernanceGate should rank via high-level community 0.
        let labels_hit: Vec<String> = res
            .hits
            .iter()
            .filter_map(|h| kg.entity(&h.entity_id).map(|e| e.label.clone()))
            .collect();
        assert!(
            labels_hit
                .iter()
                .any(|l| l.contains("Auth") || l.contains("Governance")),
            "expected auth/governance hit, got {labels_hit:?}"
        );
        assert!(
            res.token_cost.savings_ratio() > 10.0,
            "expected large savings vs GraphRAG, got {:?}",
            res.token_cost
        );
    }

    #[test]
    fn split_levels_promotes_theme() {
        let kw = tokenize_keywords("AuthService security mesh");
        let (low, high) = split_levels(&kw);
        assert!(high.iter().any(|h| h == "security" || h == "mesh"));
        assert!(
            low.iter().any(|l| l == "authservice"),
            "AuthService should stay low-level; got low={low:?} high={high:?}"
        );
    }

    #[test]
    fn token_cost_dual_cheaper() {
        let est = TokenCostEstimate {
            dual_level_tokens: 40,
            graphrag_tokens: 2500,
        };
        assert!((est.savings_ratio() - 62.5).abs() < 0.1);
    }
}
