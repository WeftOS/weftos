//! WEFT-377: Hyperedge discovery pipeline step.
//!
//! # Algorithm (motif-based)
//!
//! The pairwise knowledge graph only stores binary edges. Multi-participant
//! relationships (n-ary groups) are recovered by motif detectors that lift
//! recurring patterns into [`Hyperedge`]s. Three complementary detectors run
//! independently; results are merged and deduplicated by sorted entity-id set.
//!
//! | Detector | Motif | Example |
//! |----------|-------|---------|
//! | **Shared context** | ≥`min_arity` entities with the same non-empty `source_file` | functions/classes co-located in `auth.rs` |
//! | **Co-occurrence** | Star motif: hub + ≥`min_arity-1` leaves under one relation type | `dispatch` *calls* `{a, b, c}` |
//! | **Schema** | Parent groups under structural relations (`contains`, `method_of`, `implements`, …) | class *contains* methods; classes *implement* interface |
//!
//! Hyperedges with arity &lt; `min_arity` (default **3**) are discarded.
//! Extraction-supplied hyperedges on the graph are preserved when merging.

use crate::entity::EntityId;
use crate::model::{Hyperedge, KnowledgeGraph};
use crate::relationship::RelationType;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// Configuration for [`discover_hyperedges_with_config`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HyperedgeDiscoveryConfig {
    /// Minimum number of distinct entities in a discovered hyperedge (default 3).
    pub min_arity: usize,
    /// Enable shared-context (same source file) detection.
    pub shared_context: bool,
    /// Enable co-occurrence (star / common-neighbor) detection.
    pub co_occurrence: bool,
    /// Enable schema / structural parent-group detection.
    pub schema: bool,
    /// Soft cap on total discovered hyperedges (0 = unlimited).
    pub max_hyperedges: usize,
}

impl Default for HyperedgeDiscoveryConfig {
    fn default() -> Self {
        Self {
            min_arity: 3,
            shared_context: true,
            co_occurrence: true,
            schema: true,
            max_hyperedges: 0,
        }
    }
}

// ---------------------------------------------------------------------------
// Kind tags (stored in Hyperedge.metadata)
// ---------------------------------------------------------------------------

/// Detector kind string written into hyperedge metadata.
pub const KIND_SHARED_CONTEXT: &str = "shared_context";
/// Co-occurrence / star motif.
pub const KIND_CO_OCCURRENCE: &str = "co_occurrence";
/// Schema / structural grouping.
pub const KIND_SCHEMA: &str = "schema";

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Discover multi-node hyperedges with default configuration.
///
/// See module docs for the motif-based algorithm.
pub fn discover_hyperedges(kg: &KnowledgeGraph) -> Vec<Hyperedge> {
    discover_hyperedges_with_config(kg, &HyperedgeDiscoveryConfig::default())
}

/// Discover multi-node hyperedges with an explicit config.
pub fn discover_hyperedges_with_config(
    kg: &KnowledgeGraph,
    config: &HyperedgeDiscoveryConfig,
) -> Vec<Hyperedge> {
    let min_arity = config.min_arity.max(2);
    let mut found: Vec<Hyperedge> = Vec::new();

    if config.shared_context {
        found.extend(detect_shared_context(kg, min_arity));
    }
    if config.co_occurrence {
        found.extend(detect_co_occurrence(kg, min_arity));
    }
    if config.schema {
        found.extend(detect_schema_groups(kg, min_arity));
    }

    // Deterministic order: kind, then label, then first entity id.
    found.sort_by(|a, b| {
        let ka = kind_of(a);
        let kb = kind_of(b);
        ka.cmp(&kb)
            .then_with(|| a.label.cmp(&b.label))
            .then_with(|| {
                let ea = a.entity_ids.first().map(|e| e.0).unwrap_or([0u8; 32]);
                let eb = b.entity_ids.first().map(|e| e.0).unwrap_or([0u8; 32]);
                ea.cmp(&eb)
            })
    });

    if config.max_hyperedges > 0 && found.len() > config.max_hyperedges {
        found.truncate(config.max_hyperedges);
    }

    found
}

/// Merge discovered hyperedges into `existing`, deduplicating by the set of
/// entity IDs (order-insensitive). Existing entries win on collision.
pub fn merge_hyperedges(existing: Vec<Hyperedge>, discovered: Vec<Hyperedge>) -> Vec<Hyperedge> {
    let mut seen: HashSet<Vec<EntityId>> = HashSet::new();
    let mut out: Vec<Hyperedge> = Vec::with_capacity(existing.len() + discovered.len());

    for mut he in existing.into_iter().chain(discovered) {
        normalize_entity_ids(&mut he.entity_ids);
        let key = he.entity_ids.clone();
        if seen.insert(key) {
            out.push(he);
        }
    }
    out
}

/// Run discovery and merge results into `kg.hyperedges` (in place).
///
/// Returns the number of newly added hyperedges.
pub fn apply_discovered_hyperedges(
    kg: &mut KnowledgeGraph,
    config: &HyperedgeDiscoveryConfig,
) -> usize {
    let before = kg.hyperedges.len();
    let discovered = discover_hyperedges_with_config(kg, config);
    kg.hyperedges = merge_hyperedges(std::mem::take(&mut kg.hyperedges), discovered);
    kg.hyperedges.len().saturating_sub(before)
}

// ---------------------------------------------------------------------------
// Detectors
// ---------------------------------------------------------------------------

/// Shared context: entities co-located in the same source file.
fn detect_shared_context(kg: &KnowledgeGraph, min_arity: usize) -> Vec<Hyperedge> {
    let mut by_file: BTreeMap<String, Vec<EntityId>> = BTreeMap::new();

    for entity in kg.entities() {
        let Some(ref sf) = entity.source_file else {
            continue;
        };
        if sf.is_empty() {
            continue;
        }
        by_file
            .entry(sf.clone())
            .or_default()
            .push(entity.id.clone());
    }

    let mut out = Vec::new();
    for (file, mut ids) in by_file {
        normalize_entity_ids(&mut ids);
        ids.dedup();
        if ids.len() < min_arity {
            continue;
        }
        let short = short_path(&file);
        out.push(Hyperedge {
            label: format!("shared context: {short}"),
            entity_ids: ids,
            metadata: serde_json::json!({
                "kind": KIND_SHARED_CONTEXT,
                "source_file": file,
                "detector": "weft-377",
            }),
        });
    }
    out
}

/// Co-occurrence: star motifs (hub → leaves under one relation type).
///
/// For each directed relation type, group targets by source (outgoing star)
/// and sources by target (incoming star). When the member set (hub + leaves)
/// has arity ≥ `min_arity`, emit a hyperedge.
fn detect_co_occurrence(kg: &KnowledgeGraph, min_arity: usize) -> Vec<Hyperedge> {
    // key: (relation_str, hub_id_hex, direction) → leaf entity ids
    // We store EntityId for hub separately via a parallel map of hub labels.
    let mut outgoing: HashMap<(String, EntityId), HashSet<EntityId>> = HashMap::new();
    let mut incoming: HashMap<(String, EntityId), HashSet<EntityId>> = HashMap::new();

    for (src, tgt, rel) in kg.edges() {
        // Skip pure structural containment — handled by schema detector.
        if is_structural_parent_relation(&rel.relation_type) {
            continue;
        }
        let r = rel.relation_type_str();
        outgoing
            .entry((r.clone(), src.id.clone()))
            .or_default()
            .insert(tgt.id.clone());
        incoming
            .entry((r, tgt.id.clone()))
            .or_default()
            .insert(src.id.clone());
    }

    let mut out = Vec::new();
    let mut seen_keys: HashSet<Vec<EntityId>> = HashSet::new();

    for ((relation, hub), leaves) in outgoing {
        if leaves.len() + 1 < min_arity {
            continue;
        }
        let hub_label = entity_label(kg, &hub);
        let mut ids: Vec<EntityId> = leaves.into_iter().collect();
        ids.push(hub.clone());
        normalize_entity_ids(&mut ids);
        if !seen_keys.insert(ids.clone()) {
            continue;
        }
        out.push(Hyperedge {
            label: format!("{hub_label} {relation} co-occurrence"),
            entity_ids: ids,
            metadata: serde_json::json!({
                "kind": KIND_CO_OCCURRENCE,
                "relation": relation,
                "hub": hub.to_hex(),
                "direction": "outgoing",
                "detector": "weft-377",
            }),
        });
    }

    for ((relation, hub), leaves) in incoming {
        if leaves.len() + 1 < min_arity {
            continue;
        }
        let hub_label = entity_label(kg, &hub);
        let mut ids: Vec<EntityId> = leaves.into_iter().collect();
        ids.push(hub.clone());
        normalize_entity_ids(&mut ids);
        if !seen_keys.insert(ids.clone()) {
            continue;
        }
        out.push(Hyperedge {
            label: format!("{relation} → {hub_label} co-occurrence"),
            entity_ids: ids,
            metadata: serde_json::json!({
                "kind": KIND_CO_OCCURRENCE,
                "relation": relation,
                "hub": hub.to_hex(),
                "direction": "incoming",
                "detector": "weft-377",
            }),
        });
    }

    out
}

/// Schema groups: structural parent relations that encode multi-member groups.
///
/// - `contains` / `method_of` / `configures` / `case_of`: parent + children
/// - `implements` / `extends`: interface/base + implementors (incoming star)
///
/// These mirror schema-level multi-arity patterns that pairwise edges only
/// approximate.
fn detect_schema_groups(kg: &KnowledgeGraph, min_arity: usize) -> Vec<Hyperedge> {
    let mut parent_children: HashMap<(String, EntityId), HashSet<EntityId>> = HashMap::new();

    for (src, tgt, rel) in kg.edges() {
        let Some(role) = structural_group_role(&rel.relation_type) else {
            continue;
        };
        let r = rel.relation_type_str();
        match role {
            // Parent is source (contains, configures, case_of)
            StructuralRole::ParentIsSource => {
                parent_children
                    .entry((r, src.id.clone()))
                    .or_default()
                    .insert(tgt.id.clone());
            }
            // Parent is target (method_of, implements, extends)
            StructuralRole::ParentIsTarget => {
                parent_children
                    .entry((r, tgt.id.clone()))
                    .or_default()
                    .insert(src.id.clone());
            }
        }
    }

    let mut out = Vec::new();
    for ((relation, parent), children) in parent_children {
        if children.len() + 1 < min_arity {
            continue;
        }
        let parent_label = entity_label(kg, &parent);
        let mut ids: Vec<EntityId> = children.into_iter().collect();
        ids.push(parent.clone());
        normalize_entity_ids(&mut ids);
        out.push(Hyperedge {
            label: format!("schema {relation}: {parent_label}"),
            entity_ids: ids,
            metadata: serde_json::json!({
                "kind": KIND_SCHEMA,
                "relation": relation,
                "parent": parent.to_hex(),
                "detector": "weft-377",
            }),
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

enum StructuralRole {
    ParentIsSource,
    ParentIsTarget,
}

fn is_structural_parent_relation(rt: &RelationType) -> bool {
    structural_group_role(rt).is_some()
}

fn structural_group_role(rt: &RelationType) -> Option<StructuralRole> {
    match rt {
        RelationType::Contains | RelationType::Configures | RelationType::CaseOf => {
            Some(StructuralRole::ParentIsSource)
        }
        RelationType::MethodOf | RelationType::Implements | RelationType::Extends => {
            Some(StructuralRole::ParentIsTarget)
        }
        _ => None,
    }
}

fn entity_label(kg: &KnowledgeGraph, id: &EntityId) -> String {
    kg.entity(id)
        .map(|e| e.label.clone())
        .unwrap_or_else(|| id.to_hex())
}

fn short_path(path: &str) -> String {
    path.rsplit(['/', '\\']).next().unwrap_or(path).to_owned()
}

fn normalize_entity_ids(ids: &mut [EntityId]) {
    ids.sort_by_key(|a| a.0);
}

fn kind_of(he: &Hyperedge) -> String {
    he.metadata
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::model::Entity;
    use crate::relationship::{Confidence, Relationship};
    use std::collections::BTreeSet;

    fn entity(name: &str, et: EntityType, file: &str) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &et, name, file),
            entity_type: et,
            label: name.to_owned(),
            source_file: Some(file.to_owned()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
            iri: None,
        }
    }

    fn rel(src: &Entity, tgt: &Entity, rt: RelationType) -> Relationship {
        Relationship {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation_type: rt,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: src.source_file.clone(),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    /// Canonical graph: three functions co-located in one file, plus a caller
    /// that calls all three, plus a class containing three methods.
    fn canonical_graph() -> KnowledgeGraph {
        let f1 = entity("alpha", EntityType::Function, "mod.rs");
        let f2 = entity("beta", EntityType::Function, "mod.rs");
        let f3 = entity("gamma", EntityType::Function, "mod.rs");
        let dispatch = entity("dispatch", EntityType::Function, "dispatch.rs");
        let cls = entity("Handler", EntityType::Class, "handler.rs");
        let m1 = entity("on_start", EntityType::Function, "handler.rs");
        let m2 = entity("on_stop", EntityType::Function, "handler.rs");
        let m3 = entity("on_tick", EntityType::Function, "handler.rs");

        let entities = vec![
            f1.clone(),
            f2.clone(),
            f3.clone(),
            dispatch.clone(),
            cls.clone(),
            m1.clone(),
            m2.clone(),
            m3.clone(),
        ];
        let relationships = vec![
            rel(&dispatch, &f1, RelationType::Calls),
            rel(&dispatch, &f2, RelationType::Calls),
            rel(&dispatch, &f3, RelationType::Calls),
            rel(&cls, &m1, RelationType::Contains),
            rel(&cls, &m2, RelationType::Contains),
            rel(&cls, &m3, RelationType::Contains),
            rel(&m1, &cls, RelationType::MethodOf),
            rel(&m2, &cls, RelationType::MethodOf),
            rel(&m3, &cls, RelationType::MethodOf),
        ];
        KnowledgeGraph::from_parts(entities, relationships, vec![])
    }

    #[test]
    fn shared_context_groups_same_file() {
        let kg = canonical_graph();
        let hes = discover_hyperedges_with_config(
            &kg,
            &HyperedgeDiscoveryConfig {
                shared_context: true,
                co_occurrence: false,
                schema: false,
                ..Default::default()
            },
        );
        // mod.rs has 3 functions; handler.rs has class + 3 methods = 4
        assert!(
            hes.iter().any(|h| {
                kind_of(h) == KIND_SHARED_CONTEXT
                    && h.entity_ids.len() >= 3
                    && h.metadata.get("source_file").and_then(|v| v.as_str()) == Some("mod.rs")
            }),
            "expected shared_context hyperedge for mod.rs, got {hes:?}"
        );
        assert!(hes.iter().any(|h| {
            kind_of(h) == KIND_SHARED_CONTEXT
                && h.metadata.get("source_file").and_then(|v| v.as_str()) == Some("handler.rs")
        }));
    }

    #[test]
    fn co_occurrence_detects_call_star() {
        let kg = canonical_graph();
        let hes = discover_hyperedges_with_config(
            &kg,
            &HyperedgeDiscoveryConfig {
                shared_context: false,
                co_occurrence: true,
                schema: false,
                ..Default::default()
            },
        );
        let star = hes.iter().find(|h| {
            kind_of(h) == KIND_CO_OCCURRENCE
                && h.metadata.get("relation").and_then(|v| v.as_str()) == Some("calls")
                && h.metadata.get("direction").and_then(|v| v.as_str()) == Some("outgoing")
        });
        assert!(
            star.is_some(),
            "expected calls co-occurrence star, got {hes:?}"
        );
        let star = star.unwrap();
        // hub + 3 callees
        assert_eq!(star.entity_ids.len(), 4);
        assert!(star.label.contains("dispatch"));
    }

    #[test]
    fn schema_detects_contains_and_method_of() {
        let kg = canonical_graph();
        let hes = discover_hyperedges_with_config(
            &kg,
            &HyperedgeDiscoveryConfig {
                shared_context: false,
                co_occurrence: false,
                schema: true,
                ..Default::default()
            },
        );
        assert!(
            hes.iter().any(|h| {
                kind_of(h) == KIND_SCHEMA
                    && h.metadata.get("relation").and_then(|v| v.as_str()) == Some("contains")
                    && h.entity_ids.len() == 4
            }),
            "expected contains schema hyperedge, got {hes:?}"
        );
        assert!(
            hes.iter().any(|h| {
                kind_of(h) == KIND_SCHEMA
                    && h.metadata.get("relation").and_then(|v| v.as_str()) == Some("method_of")
            }),
            "expected method_of schema hyperedge, got {hes:?}"
        );
    }

    #[test]
    fn full_discovery_finds_all_kinds() {
        let kg = canonical_graph();
        let hes = discover_hyperedges(&kg);
        let kinds: BTreeSet<String> = hes.iter().map(kind_of).collect();
        assert!(kinds.contains(KIND_SHARED_CONTEXT), "kinds={kinds:?}");
        assert!(kinds.contains(KIND_CO_OCCURRENCE), "kinds={kinds:?}");
        assert!(kinds.contains(KIND_SCHEMA), "kinds={kinds:?}");
        // Every hyperedge has arity >= 3
        assert!(hes.iter().all(|h| h.entity_ids.len() >= 3));
    }

    #[test]
    fn min_arity_filters_pairs() {
        let a = entity("a", EntityType::Function, "x.rs");
        let b = entity("b", EntityType::Function, "x.rs");
        let kg = KnowledgeGraph::from_parts(vec![a, b], vec![], vec![]);
        let hes = discover_hyperedges(&kg);
        assert!(
            hes.is_empty(),
            "two entities should not form min_arity=3 hyperedge"
        );
    }

    #[test]
    fn merge_preserves_existing_and_dedups() {
        let a = EntityId::new(&DomainTag::Code, &EntityType::Function, "a", "f.rs");
        let b = EntityId::new(&DomainTag::Code, &EntityType::Function, "b", "f.rs");
        let c = EntityId::new(&DomainTag::Code, &EntityType::Function, "c", "f.rs");
        let mut ids = vec![a.clone(), b.clone(), c.clone()];
        normalize_entity_ids(&mut ids);

        let existing = vec![Hyperedge {
            label: "from extraction".into(),
            entity_ids: ids.clone(),
            metadata: serde_json::json!({"kind": "extraction"}),
        }];
        let discovered = vec![Hyperedge {
            label: "from discovery".into(),
            entity_ids: ids,
            metadata: serde_json::json!({"kind": KIND_SHARED_CONTEXT}),
        }];
        let merged = merge_hyperedges(existing, discovered);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].label, "from extraction");
    }

    #[test]
    fn apply_mutates_graph() {
        let mut kg = canonical_graph();
        assert!(kg.hyperedges.is_empty());
        let added = apply_discovered_hyperedges(&mut kg, &HyperedgeDiscoveryConfig::default());
        assert!(added > 0);
        assert_eq!(kg.hyperedges.len(), added);
    }

    #[test]
    fn empty_graph_yields_no_hyperedges() {
        let kg = KnowledgeGraph::new();
        assert!(discover_hyperedges(&kg).is_empty());
    }

    #[test]
    fn implements_schema_group() {
        let iface = entity("Drawable", EntityType::Interface, "draw.rs");
        let c1 = entity("Circle", EntityType::Class, "circle.rs");
        let c2 = entity("Square", EntityType::Class, "square.rs");
        let c3 = entity("Triangle", EntityType::Class, "tri.rs");
        let relationships = vec![
            rel(&c1, &iface, RelationType::Implements),
            rel(&c2, &iface, RelationType::Implements),
            rel(&c3, &iface, RelationType::Implements),
        ];
        let kg = KnowledgeGraph::from_parts(vec![iface, c1, c2, c3], relationships, vec![]);
        let hes = discover_hyperedges_with_config(
            &kg,
            &HyperedgeDiscoveryConfig {
                shared_context: false,
                co_occurrence: false,
                schema: true,
                ..Default::default()
            },
        );
        assert_eq!(hes.len(), 1);
        assert_eq!(hes[0].entity_ids.len(), 4);
        assert_eq!(
            hes[0].metadata.get("relation").and_then(|v| v.as_str()),
            Some("implements")
        );
    }
}
