//! Layout engine — converts a KnowledgeGraph + TopologySchema into positioned
//! geometry via tree calculus dispatch + layout algorithms.

pub mod force_layout;
pub mod positioned;
pub mod slicer;
pub mod sugiyama;
pub mod tree_layout;
pub mod triage;

use std::collections::HashMap;

use crate::entity::EntityId;
use crate::model::KnowledgeGraph;
use crate::relationship::RelationType;
use crate::topology::{Geometry, TopologySchema};

use positioned::{PositionedEdge, PositionedGraph, PositionedNode};

/// Lay out a knowledge graph using a topology schema.
///
/// This is the main entry point for the layout engine. It:
/// 1. Classifies each node via triage (Atom/Sequence/Branch)
/// 2. Selects a layout algorithm from the schema's geometry declaration
/// 3. Computes positions via the selected algorithm
/// 4. Produces a `PositionedGraph` for renderer consumption
pub fn layout_graph(
    kg: &KnowledgeGraph,
    schema: &TopologySchema,
    width: f64,
    height: f64,
) -> PositionedGraph {
    let entities: Vec<_> = kg.entities().collect();
    if entities.is_empty() {
        return PositionedGraph {
            nodes: vec![],
            edges: vec![],
            viewport: positioned::Rect {
                x: 0.0,
                y: 0.0,
                width,
                height,
            },
            schema_name: schema.name.clone(),
            schema_version: schema.version.clone(),
        };
    }

    // Determine the root geometry from the schema or auto-detect.
    let root_geometry = detect_geometry(kg, schema);

    // Compute positions based on geometry.
    let positions: HashMap<EntityId, (f64, f64)> = match root_geometry {
        Geometry::Tree | Geometry::Radial => layout_as_tree(kg, width, height),
        Geometry::Layered => layout_as_layered(kg, width, height),
        _ => layout_as_force(kg, width, height),
    };

    // Build positioned nodes.
    let mut nodes: Vec<PositionedNode> = Vec::with_capacity(entities.len());
    for entity in &entities {
        let (x, y) = positions
            .get(&entity.id)
            .copied()
            .unwrap_or((width / 2.0, height / 2.0));
        let type_key = entity.entity_type.discriminant();
        let config = schema.node_config(type_key);
        let has_children = !triage::children_of(kg, &entity.id).is_empty();

        let (shape, color, icon, radius) = match config {
            Some(c) => (
                c.style.shape,
                c.style.color.clone(),
                c.style.icon.clone(),
                ((c.style.min_radius + c.style.max_radius) / 2) as f64,
            ),
            None => (
                crate::topology::NodeShape::Circle,
                "#a3a3a3".into(),
                None,
                24.0,
            ),
        };

        nodes.push(PositionedNode {
            id: entity.id.to_hex(),
            x,
            y,
            width: radius * 2.0,
            height: radius * 2.0,
            label: entity.label.clone(),
            node_type: type_key.to_string(),
            iri: entity.iri.clone(),
            shape,
            color,
            icon,
            has_subgraph: has_children,
            disposition: None,
            metrics: HashMap::new(),
        });
    }

    // Build positioned edges.
    let mut edges: Vec<PositionedEdge> = Vec::new();
    for (src, tgt, rel) in kg.edges() {
        let src_pos = positions.get(&src.id).copied().unwrap_or((0.0, 0.0));
        let tgt_pos = positions.get(&tgt.id).copied().unwrap_or((0.0, 0.0));

        let rel_type_str = format!("{:?}", rel.relation_type);
        let edge_config = schema.edges.iter().find(|e| {
            e.edge_type == rel_type_str.to_lowercase() || e.edge_type == snake_case(&rel_type_str)
        });

        let (stroke, w, dash, arrow, animated) = match edge_config {
            Some(ec) => (
                ec.style.stroke.clone(),
                ec.style.width,
                ec.style.dash,
                ec.style.arrow,
                ec.animated,
            ),
            None => (
                "#888888".into(),
                1.0,
                crate::topology::DashStyle::Solid,
                true,
                false,
            ),
        };

        edges.push(PositionedEdge {
            source_id: src.id.to_hex(),
            target_id: tgt.id.to_hex(),
            label: Some(snake_case(&rel_type_str)),
            edge_type: snake_case(&rel_type_str),
            path: vec![[src_pos.0, src_pos.1], [tgt_pos.0, tgt_pos.1]],
            stroke,
            width: w,
            dash,
            arrow,
            animated,
        });
    }

    let mut graph = PositionedGraph {
        nodes,
        edges,
        viewport: positioned::Rect {
            x: 0.0,
            y: 0.0,
            width,
            height,
        },
        schema_name: schema.name.clone(),
        schema_version: schema.version.clone(),
    };
    graph.compute_viewport();
    graph
}

/// Auto-detect the best geometry from the graph structure.
pub fn detect_geometry(kg: &KnowledgeGraph, schema: &TopologySchema) -> Geometry {
    // 1. If schema declares root_geometry, use it.
    if schema.modes.structure.root_geometry != Geometry::Force {
        return schema.modes.structure.root_geometry;
    }

    // 2. Count Contains edges to detect tree structure.
    let total_edges = kg.relationship_count();
    if total_edges == 0 {
        return Geometry::Force;
    }

    let contains_count = kg
        .edges()
        .filter(|(_, _, rel)| matches!(rel.relation_type, RelationType::Contains))
        .count();

    let contains_ratio = contains_count as f64 / total_edges as f64;

    // 3. If >60% Contains edges, it's a tree.
    if contains_ratio > 0.6 {
        return Geometry::Tree;
    }

    // 4. Check for timeline (nodes with timestamps).
    let has_timestamps = kg.entities().any(|e| {
        e.metadata.get("timestamp").is_some()
            || e.metadata.get("created_at").is_some()
            || e.metadata.get("date").is_some()
    });
    let timestamp_ratio = if has_timestamps {
        kg.entities()
            .filter(|e| {
                e.metadata.get("timestamp").is_some()
                    || e.metadata.get("created_at").is_some()
                    || e.metadata.get("date").is_some()
            })
            .count() as f64
            / kg.entity_count() as f64
    } else {
        0.0
    };

    if timestamp_ratio > 0.5 {
        return Geometry::Timeline;
    }

    // 5. Default: force-directed.
    Geometry::Force
}

/// Lay out the graph as a tree using Contains edges for hierarchy.
fn layout_as_tree(kg: &KnowledgeGraph, width: f64, _height: f64) -> HashMap<EntityId, (f64, f64)> {
    // Build children map from Contains edges.
    let mut children_map: HashMap<EntityId, Vec<EntityId>> = HashMap::new();
    let mut has_parent: std::collections::HashSet<EntityId> = std::collections::HashSet::new();

    for (src, tgt, rel) in kg.edges() {
        if matches!(rel.relation_type, RelationType::Contains) {
            children_map
                .entry(src.id.clone())
                .or_default()
                .push(tgt.id.clone());
            has_parent.insert(tgt.id.clone());
        }
    }

    // Find root nodes (no parent).
    let roots: Vec<EntityId> = kg
        .entities()
        .filter(|e| !has_parent.contains(&e.id))
        .map(|e| e.id.clone())
        .collect();

    if roots.is_empty() {
        // Cyclic or no Contains edges — fall back to force.
        return layout_as_force(kg, width, _height);
    }

    let config = tree_layout::TreeConfig::default();

    if roots.len() == 1 {
        return tree_layout::layout(&roots[0], &children_map, &config);
    }

    // Multiple roots: lay out each tree, offset horizontally.
    let mut all_positions = HashMap::new();
    let mut x_offset = 0.0;

    for root in &roots {
        let positions = tree_layout::layout(root, &children_map, &config);
        let max_x = positions.values().map(|(x, _)| *x).fold(f64::MIN, f64::max);
        let min_x = positions.values().map(|(x, _)| *x).fold(f64::MAX, f64::min);

        for (id, (x, y)) in &positions {
            all_positions.insert(id.clone(), (*x + x_offset - min_x, *y));
        }
        x_offset += (max_x - min_x) + 100.0;
    }

    all_positions
}

/// Lay out the graph with Sugiyama hierarchical (layered digraph) layout.
///
/// Uses **all** directed edges (not only `Contains`). Cycle edges are reversed
/// for ranking; nodes receive discrete layer ranks so edges flow top→bottom.
/// This is intentionally distinct from [`layout_as_tree`] (Reingold–Tilford on
/// the `Contains` forest).
fn layout_as_layered(
    kg: &KnowledgeGraph,
    width: f64,
    height: f64,
) -> HashMap<EntityId, (f64, f64)> {
    let entities: Vec<_> = kg.entities().collect();
    let node_ids: Vec<EntityId> = entities.iter().map(|e| e.id.clone()).collect();
    let id_to_idx: HashMap<&EntityId, usize> =
        node_ids.iter().enumerate().map(|(i, id)| (id, i)).collect();

    let mut edge_pairs: Vec<(usize, usize)> = Vec::new();
    for (src, tgt, _) in kg.edges() {
        if let (Some(&si), Some(&ti)) = (id_to_idx.get(&src.id), id_to_idx.get(&tgt.id)) {
            edge_pairs.push((si, ti));
        }
    }

    sugiyama::layout(
        &node_ids,
        &edge_pairs,
        width,
        height,
        &sugiyama::LayeredConfig::default(),
    )
}

/// Lay out the graph using force-directed simulation.
fn layout_as_force(kg: &KnowledgeGraph, width: f64, height: f64) -> HashMap<EntityId, (f64, f64)> {
    let entities: Vec<_> = kg.entities().collect();
    let node_ids: Vec<EntityId> = entities.iter().map(|e| e.id.clone()).collect();
    let id_to_idx: HashMap<&EntityId, usize> =
        node_ids.iter().enumerate().map(|(i, id)| (id, i)).collect();

    let mut edge_pairs: Vec<(usize, usize)> = Vec::new();
    for (src, tgt, _) in kg.edges() {
        if let (Some(&si), Some(&ti)) = (id_to_idx.get(&src.id), id_to_idx.get(&tgt.id)) {
            edge_pairs.push((si, ti));
        }
    }

    force_layout::layout(
        &node_ids,
        &edge_pairs,
        width,
        height,
        &force_layout::ForceConfig::default(),
    )
}

/// Convert CamelCase to snake_case.
fn snake_case(s: &str) -> String {
    let mut result = String::with_capacity(s.len() + 4);
    for (i, ch) in s.chars().enumerate() {
        if ch.is_uppercase() && i > 0 {
            result.push('_');
        }
        result.push(ch.to_ascii_lowercase());
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityType, FileType};
    use crate::model::Entity;
    use crate::relationship::{Confidence, Relationship};

    fn entity(name: &str, etype: EntityType) -> Entity {
        Entity {
            id: EntityId::new(&DomainTag::Code, &etype, name, "test.rs"),
            entity_type: etype,
            label: name.to_string(),
            iri: None,
            source_file: Some("test.rs".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: None,
        }
    }

    fn contains(src: &Entity, tgt: &Entity) -> Relationship {
        Relationship {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation_type: RelationType::Contains,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    fn calls(src: &Entity, tgt: &Entity) -> Relationship {
        Relationship {
            source: src.id.clone(),
            target: tgt.id.clone(),
            relation_type: RelationType::Calls,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    fn test_schema() -> TopologySchema {
        TopologySchema::from_yaml(
            r##"
name: test
label: "Test"
version: "1.0.0"
nodes:
  module:
    geometry: tree
    contains: [function]
    style:
      shape: rect
      color: "#8b5cf6"
  function:
    geometry: force
    style:
      shape: circle
      color: "#22c55e"
  "*":
    geometry: force
edges:
  - type: contains
    from: module
    to: function
  - type: calls
    from: function
    to: function
modes:
  structure:
    root_geometry: force
"##,
        )
        .unwrap()
    }

    #[test]
    fn layout_empty_graph() {
        let kg = KnowledgeGraph::new();
        let schema = test_schema();
        let result = layout_graph(&kg, &schema, 800.0, 600.0);
        assert!(result.nodes.is_empty());
        assert!(result.edges.is_empty());
    }

    #[test]
    fn layout_single_node() {
        let mut kg = KnowledgeGraph::new();
        let f = entity("main", EntityType::Function);
        kg.add_entity(f);
        let schema = test_schema();
        let result = layout_graph(&kg, &schema, 800.0, 600.0);
        assert_eq!(result.nodes.len(), 1);
        assert_eq!(result.nodes[0].node_type, "function");
        assert_eq!(result.nodes[0].shape, crate::topology::NodeShape::Circle);
        assert_eq!(result.nodes[0].color, "#22c55e");
    }

    #[test]
    fn layout_tree_with_contains() {
        let mut kg = KnowledgeGraph::new();
        let m = entity("app", EntityType::Module);
        let f1 = entity("foo", EntityType::Function);
        let f2 = entity("bar", EntityType::Function);
        kg.add_entity(m.clone());
        kg.add_entity(f1.clone());
        kg.add_entity(f2.clone());
        kg.add_relationship(contains(&m, &f1));
        kg.add_relationship(contains(&m, &f2));

        let mut schema = test_schema();
        schema.modes.structure.root_geometry = Geometry::Tree;
        let result = layout_graph(&kg, &schema, 800.0, 600.0);

        assert_eq!(result.nodes.len(), 3);
        assert_eq!(result.edges.len(), 2);

        let mod_node = result
            .nodes
            .iter()
            .find(|n| n.node_type == "module")
            .unwrap();
        assert!(mod_node.has_subgraph);
        assert_eq!(mod_node.shape, crate::topology::NodeShape::Rect);
    }

    #[test]
    fn layout_force_with_calls() {
        let mut kg = KnowledgeGraph::new();
        let f1 = entity("foo", EntityType::Function);
        let f2 = entity("bar", EntityType::Function);
        let f3 = entity("baz", EntityType::Function);
        kg.add_entity(f1.clone());
        kg.add_entity(f2.clone());
        kg.add_entity(f3.clone());
        kg.add_relationship(calls(&f1, &f2));
        kg.add_relationship(calls(&f2, &f3));

        let schema = test_schema();
        let result = layout_graph(&kg, &schema, 800.0, 600.0);

        assert_eq!(result.nodes.len(), 3);
        assert_eq!(result.edges.len(), 2);
        assert!(result.edges.iter().all(|e| e.edge_type == "calls"));
    }

    #[test]
    fn auto_detect_tree_geometry() {
        let mut kg = KnowledgeGraph::new();
        let m = entity("app", EntityType::Module);
        let f1 = entity("foo", EntityType::Function);
        let f2 = entity("bar", EntityType::Function);
        let f3 = entity("baz", EntityType::Function);
        kg.add_entity(m.clone());
        kg.add_entity(f1.clone());
        kg.add_entity(f2.clone());
        kg.add_entity(f3.clone());
        kg.add_relationship(contains(&m, &f1));
        kg.add_relationship(contains(&m, &f2));
        kg.add_relationship(contains(&m, &f3));

        let schema = test_schema();
        let detected = detect_geometry(&kg, &schema);
        assert_eq!(detected, Geometry::Tree);
    }

    #[test]
    fn positioned_graph_serializes_to_json() {
        let mut kg = KnowledgeGraph::new();
        let f = entity("main", EntityType::Function);
        kg.add_entity(f);
        let schema = test_schema();
        let result = layout_graph(&kg, &schema, 800.0, 600.0);

        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("\"node_type\":\"function\""));
        assert!(json.contains("\"schema_name\":\"test\""));
    }

    #[test]
    fn layout_layered_chain_ranks_top_to_bottom() {
        // Calls edges form a chain; Layered must place sources above sinks.
        let mut kg = KnowledgeGraph::new();
        let a = entity("a", EntityType::Function);
        let b = entity("b", EntityType::Function);
        let c = entity("c", EntityType::Function);
        kg.add_entity(a.clone());
        kg.add_entity(b.clone());
        kg.add_entity(c.clone());
        kg.add_relationship(calls(&a, &b));
        kg.add_relationship(calls(&b, &c));

        let mut schema = test_schema();
        schema.modes.structure.root_geometry = Geometry::Layered;
        let result = layout_graph(&kg, &schema, 800.0, 600.0);

        assert_eq!(result.nodes.len(), 3);
        let pos = |label: &str| {
            let n = result.nodes.iter().find(|n| n.label == label).unwrap();
            (n.x, n.y)
        };
        let (_, ay) = pos("a");
        let (_, by) = pos("b");
        let (_, cy) = pos("c");
        assert!(ay < by, "layered: a above b ({ay} < {by})");
        assert!(by < cy, "layered: b above c ({by} < {cy})");
    }

    #[test]
    fn layout_layered_not_silent_tree_fallback() {
        // Graph with only Calls edges (no Contains). Tree layout walks
        // Contains only and places disconnected roots in a horizontal strip;
        // layered must rank the call DAG top→bottom and must not reuse that
        // tree path (WEFT-362 AC: no silent tree fallback for Layered).
        let mut kg = KnowledgeGraph::new();
        let a = entity("src", EntityType::Function);
        let b = entity("mid", EntityType::Function);
        let c = entity("sink", EntityType::Function);
        let d = entity("sink2", EntityType::Function);
        kg.add_entity(a.clone());
        kg.add_entity(b.clone());
        kg.add_entity(c.clone());
        kg.add_entity(d.clone());
        kg.add_relationship(calls(&a, &b));
        kg.add_relationship(calls(&b, &c));
        kg.add_relationship(calls(&b, &d));

        let mut layered_schema = test_schema();
        layered_schema.modes.structure.root_geometry = Geometry::Layered;
        let layered = layout_graph(&kg, &layered_schema, 800.0, 600.0);

        let mut tree_schema = test_schema();
        tree_schema.modes.structure.root_geometry = Geometry::Tree;
        let tree = layout_graph(&kg, &tree_schema, 800.0, 600.0);

        let y_of = |g: &PositionedGraph, label: &str| {
            g.nodes
                .iter()
                .find(|n| n.label == label)
                .map(|n| n.y)
                .unwrap()
        };

        // Layered: src above mid above {sink, sink2} (siblings share a layer).
        assert!(y_of(&layered, "src") < y_of(&layered, "mid"));
        assert!(y_of(&layered, "mid") < y_of(&layered, "sink"));
        assert!(
            (y_of(&layered, "sink") - y_of(&layered, "sink2")).abs() < 1.0,
            "layered siblings share a rank"
        );

        // Tree path (no Contains): every node is a root → all y ≈ 0 (horizontal).
        // Layered must therefore differ (strict multi-rank y span).
        let layered_y_span = {
            let ys: Vec<f64> = layered.nodes.iter().map(|n| n.y).collect();
            ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                - ys.iter().cloned().fold(f64::INFINITY, f64::min)
        };
        let tree_y_span = {
            let ys: Vec<f64> = tree.nodes.iter().map(|n| n.y).collect();
            ys.iter().cloned().fold(f64::NEG_INFINITY, f64::max)
                - ys.iter().cloned().fold(f64::INFINITY, f64::min)
        };
        assert!(
            layered_y_span > tree_y_span + 10.0,
            "layered must produce multi-rank y span (got layered={layered_y_span}, tree={tree_y_span}); \
             silent tree fallback would collapse ranks"
        );

        assert_eq!(layered.nodes.len(), 4);
        assert!(
            layered
                .nodes
                .iter()
                .all(|n| n.x.is_finite() && n.y.is_finite())
        );
    }

    #[test]
    fn layout_layered_diamond_with_contains_and_calls() {
        // Mix Contains + Calls; layered must use all edges for ranking.
        let mut kg = KnowledgeGraph::new();
        let m = entity("mod", EntityType::Module);
        let f1 = entity("f1", EntityType::Function);
        let f2 = entity("f2", EntityType::Function);
        let f3 = entity("f3", EntityType::Function);
        kg.add_entity(m.clone());
        kg.add_entity(f1.clone());
        kg.add_entity(f2.clone());
        kg.add_entity(f3.clone());
        kg.add_relationship(contains(&m, &f1));
        kg.add_relationship(contains(&m, &f2));
        kg.add_relationship(calls(&f1, &f3));
        kg.add_relationship(calls(&f2, &f3));

        let mut schema = test_schema();
        schema.modes.structure.root_geometry = Geometry::Layered;
        let result = layout_graph(&kg, &schema, 900.0, 700.0);

        assert_eq!(result.nodes.len(), 4);
        assert_eq!(result.edges.len(), 4);

        let y = |label: &str| result.nodes.iter().find(|n| n.label == label).unwrap().y;
        // Module contains f1/f2; f1/f2 call f3 → mod is above leaves.
        assert!(
            y("mod") < y("f3"),
            "module should rank above deepest callee"
        );
    }
}
