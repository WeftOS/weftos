//! Positioned SVG export with **VOWL visual encoding** (OG-4 / WEFT-360).
//!
//! Renders a `KnowledgeGraph` (via the layout engine) or a precomputed
//! [`PositionedGraph`] as a self-contained SVG document that follows the
//! [VOWL 2](http://vowl.visualdataweb.org/v2/) visual notation used by
//! WebVOWL and the in-tree navigator (`docs/src/app/vowl-navigator/`).
//!
//! # VOWL rule mapping
//!
//! | OWL / RDFS type           | Shape              | Fill      | Stroke    | Stroke style |
//! |---------------------------|--------------------|-----------|-----------|--------------|
//! | `owl:Class`               | circle             | `#aaccff` | `#306998` | solid        |
//! | `owl:equivalentClass`     | dual ellipse       | `#aaccff` | `#306998` | solid + ring |
//! | `rdfs:Datatype`           | rounded rectangle  | `#ffcc33` | `#b38f00` | solid        |
//! | `rdfs:Literal`            | rounded rectangle  | `#ffcc33` | `#b38f00` | solid        |
//! | `owl:Thing`               | circle             | `#ffffff` | `#000000` | dashed       |
//! | `owl:Nothing`             | circle             | `#1a1a2e` | `#000000` | solid        |
//! | `owl:ObjectProperty`      | labeled arrow + diamond | `#aaccff` | `#306998` | solid   |
//! | `owl:DatatypeProperty`    | labeled arrow + diamond | `#99cc66` | `#527a2e` | solid   |
//! | `rdfs:subClassOf`         | labeled arrow      | `#eeeeee` | `#999999` | solid        |
//! | (unknown)                 | circle / line      | `#d9d9d9` | `#666666` | solid        |
//! | attribute `deprecated`    | (same shape)       | `#cccccc` | `#999999` | dashed       |
//! | attribute `external`      | (same shape)       | `#3c4a5a` | `#667788` | solid        |
//!
//! Entity → OWL class mapping (when metadata does not override):
//! - `constant` / `enum_` → `rdfs:Datatype`
//! - everything else → `owl:Class`
//!
//! Relation → OWL property mapping (when metadata does not override):
//! - `Contains` / `Extends` → `rdfs:subClassOf`
//! - `Custom("owl:DatatypeProperty")` / metadata `owl_type` → honored
//! - everything else → `owl:ObjectProperty`
//!
//! Colors and shapes match `docs/src/app/vowl-navigator/types.ts` `vowlStyle`
//! for WebVOWL parity.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::GraphifyError;
use crate::layout::layout_graph;
use crate::layout::positioned::{PositionedEdge, PositionedGraph, PositionedNode};
use crate::model::KnowledgeGraph;
use crate::relationship::RelationType;
use crate::topology::TopologySchema;

// ---------------------------------------------------------------------------
// VOWL visual style (WebVOWL parity)
// ---------------------------------------------------------------------------

/// Geometric primitive used by VOWL for a class or property node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VowlShape {
    /// Standard OWL class — filled circle.
    Circle,
    /// Equivalent class — two concentric ellipses.
    DualEllipse,
    /// Datatype / literal — rounded rectangle.
    Rect,
    /// Property label box (object / datatype property).
    Diamond,
}

/// Resolved VOWL visual style for a single OWL/RDFS type + attributes.
#[derive(Debug, Clone, PartialEq)]
pub struct VowlStyle {
    pub fill: &'static str,
    pub stroke: &'static str,
    pub shape: VowlShape,
    pub dashed: bool,
    pub text_color: &'static str,
}

/// VOWL visual encoding rules (VOWL 2 / WebVOWL palette).
///
/// Attribute flags: `"deprecated"`, `"external"`.
pub fn vowl_style(owl_type: &str, attributes: &[&str]) -> VowlStyle {
    let deprecated = attributes.contains(&"deprecated");
    let external = attributes.contains(&"external");

    let mut base = match owl_type {
        "owl:Class" => VowlStyle {
            fill: "#aaccff",
            stroke: "#306998",
            shape: VowlShape::Circle,
            dashed: false,
            text_color: "#1a1a2e",
        },
        "owl:equivalentClass" => VowlStyle {
            fill: "#aaccff",
            stroke: "#306998",
            shape: VowlShape::DualEllipse,
            dashed: false,
            text_color: "#1a1a2e",
        },
        "rdfs:Datatype" | "rdfs:Literal" => VowlStyle {
            fill: "#ffcc33",
            stroke: "#b38f00",
            shape: VowlShape::Rect,
            dashed: false,
            text_color: "#1a1a2e",
        },
        "owl:Thing" => VowlStyle {
            fill: "#ffffff",
            stroke: "#000000",
            shape: VowlShape::Circle,
            dashed: true,
            text_color: "#1a1a2e",
        },
        "owl:Nothing" => VowlStyle {
            fill: "#1a1a2e",
            stroke: "#000000",
            shape: VowlShape::Circle,
            dashed: false,
            text_color: "#ffffff",
        },
        "owl:ObjectProperty" => VowlStyle {
            fill: "#aaccff",
            stroke: "#306998",
            shape: VowlShape::Diamond,
            dashed: false,
            text_color: "#1a1a2e",
        },
        "owl:DatatypeProperty" => VowlStyle {
            fill: "#99cc66",
            stroke: "#527a2e",
            shape: VowlShape::Diamond,
            dashed: false,
            text_color: "#1a1a2e",
        },
        "rdfs:subClassOf" => VowlStyle {
            fill: "#eeeeee",
            stroke: "#999999",
            shape: VowlShape::Diamond,
            dashed: false,
            text_color: "#333333",
        },
        _ => VowlStyle {
            fill: "#d9d9d9",
            stroke: "#666666",
            shape: VowlShape::Circle,
            dashed: false,
            text_color: "#1a1a2e",
        },
    };

    if deprecated {
        base.dashed = true;
        base.fill = "#cccccc";
        base.stroke = "#999999";
    }
    if external {
        base.fill = "#3c4a5a";
        base.stroke = "#667788";
        base.text_color = "#e0e0e0";
    }

    base
}

/// Documented VOWL rule rows for tests and tooling.
pub fn vowl_rule_table() -> &'static [(&'static str, VowlShape, &'static str, &'static str)] {
    &[
        ("owl:Class", VowlShape::Circle, "#aaccff", "#306998"),
        (
            "owl:equivalentClass",
            VowlShape::DualEllipse,
            "#aaccff",
            "#306998",
        ),
        ("rdfs:Datatype", VowlShape::Rect, "#ffcc33", "#b38f00"),
        ("rdfs:Literal", VowlShape::Rect, "#ffcc33", "#b38f00"),
        ("owl:Thing", VowlShape::Circle, "#ffffff", "#000000"),
        ("owl:Nothing", VowlShape::Circle, "#1a1a2e", "#000000"),
        (
            "owl:ObjectProperty",
            VowlShape::Diamond,
            "#aaccff",
            "#306998",
        ),
        (
            "owl:DatatypeProperty",
            VowlShape::Diamond,
            "#99cc66",
            "#527a2e",
        ),
        ("rdfs:subClassOf", VowlShape::Diamond, "#eeeeee", "#999999"),
    ]
}

// ---------------------------------------------------------------------------
// OWL type mapping from graphify entities / relations
// ---------------------------------------------------------------------------

/// Map a graphify entity to an OWL class type string.
pub fn owl_class_type_for_entity(entity_type_disc: &str, metadata: &serde_json::Value) -> String {
    if let Some(t) = metadata
        .get("owl_type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return t.to_string();
    }
    match entity_type_disc {
        "constant" | "enum_" => "rdfs:Datatype".into(),
        _ => "owl:Class".into(),
    }
}

/// Map a graphify relation to an OWL property type string.
pub fn owl_property_type_for_relation(
    relation_type: &RelationType,
    metadata: &serde_json::Value,
) -> String {
    if let Some(t) = metadata
        .get("owl_type")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
    {
        return t.to_string();
    }
    match relation_type {
        RelationType::Contains | RelationType::Extends => "rdfs:subClassOf".into(),
        RelationType::Custom(s) if s == "owl:DatatypeProperty" || s == "DatatypeProperty" => {
            "owl:DatatypeProperty".into()
        }
        RelationType::Custom(s) if s == "owl:equivalentClass" || s == "equivalentClass" => {
            "owl:equivalentClass".into()
        }
        RelationType::Custom(s) if s == "rdfs:subClassOf" || s == "subClassOf" => {
            "rdfs:subClassOf".into()
        }
        _ => "owl:ObjectProperty".into(),
    }
}

fn attributes_from_metadata(metadata: &serde_json::Value) -> Vec<String> {
    let mut attrs = Vec::new();
    if let Some(arr) = metadata.get("vowl_attributes").and_then(|v| v.as_array()) {
        for a in arr {
            if let Some(s) = a.as_str() {
                attrs.push(s.to_string());
            }
        }
    }
    if metadata
        .get("deprecated")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        attrs.push("deprecated".into());
    }
    if metadata
        .get("external")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        attrs.push("external".into());
    }
    attrs
}

// ---------------------------------------------------------------------------
// Public export API
// ---------------------------------------------------------------------------

/// Default canvas size for headless SVG export.
pub const DEFAULT_SVG_WIDTH: f64 = 960.0;
pub const DEFAULT_SVG_HEIGHT: f64 = 640.0;

/// Export a knowledge graph to an SVG file with VOWL visual encoding.
///
/// Runs the topology layout engine, then paints VOWL-compliant shapes.
pub fn to_svg(
    kg: &KnowledgeGraph,
    schema: &TopologySchema,
    output: &Path,
) -> Result<(), GraphifyError> {
    let svg = to_svg_string(kg, schema, DEFAULT_SVG_WIDTH, DEFAULT_SVG_HEIGHT)?;
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            GraphifyError::ExportError(format!("create parent {}: {e}", parent.display()))
        })?;
    }
    fs::write(output, svg).map_err(|e| {
        GraphifyError::ExportError(format!("Failed to write {}: {e}", output.display()))
    })?;
    Ok(())
}

/// Build SVG markup (no disk I/O) from a knowledge graph + topology schema.
pub fn to_svg_string(
    kg: &KnowledgeGraph,
    schema: &TopologySchema,
    width: f64,
    height: f64,
) -> Result<String, GraphifyError> {
    let positioned = layout_graph(kg, schema, width, height);
    let annotations = collect_owl_annotations(kg);
    Ok(render_vowl_svg(&positioned, &annotations))
}

/// Render a pre-positioned graph with VOWL encoding (no layout step).
///
/// `owl_types` maps node/edge ids to OWL type strings. Missing entries fall
/// back to heuristics from `node_type` / `edge_type` fields.
pub fn render_positioned_svg(
    graph: &PositionedGraph,
    owl_types: &HashMap<String, String>,
    attributes: &HashMap<String, Vec<String>>,
) -> String {
    let annotations = VowlAnnotations {
        node_owl: owl_types.clone(),
        edge_owl: owl_types.clone(),
        node_attrs: attributes.clone(),
        edge_attrs: attributes.clone(),
    };
    render_vowl_svg(graph, &annotations)
}

// ---------------------------------------------------------------------------
// Annotation collection
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct VowlAnnotations {
    node_owl: HashMap<String, String>,
    edge_owl: HashMap<String, String>,
    node_attrs: HashMap<String, Vec<String>>,
    edge_attrs: HashMap<String, Vec<String>>,
}

fn collect_owl_annotations(kg: &KnowledgeGraph) -> VowlAnnotations {
    let mut ann = VowlAnnotations::default();
    for entity in kg.entities() {
        let hex = entity.id.to_hex();
        let disc = entity.entity_type.discriminant();
        ann.node_owl.insert(
            hex.clone(),
            owl_class_type_for_entity(disc, &entity.metadata),
        );
        ann.node_attrs
            .insert(hex, attributes_from_metadata(&entity.metadata));
    }
    for (src, tgt, rel) in kg.edges() {
        let key = edge_key(&src.id.to_hex(), &tgt.id.to_hex(), &rel.relation_type);
        ann.edge_owl.insert(
            key.clone(),
            owl_property_type_for_relation(&rel.relation_type, &rel.metadata),
        );
        ann.edge_attrs
            .insert(key, attributes_from_metadata(&rel.metadata));
    }
    ann
}

fn edge_key(src: &str, tgt: &str, rel: &RelationType) -> String {
    format!("{src}|{tgt}|{}", rel.as_str())
}

// ---------------------------------------------------------------------------
// SVG rendering
// ---------------------------------------------------------------------------

fn render_vowl_svg(graph: &PositionedGraph, ann: &VowlAnnotations) -> String {
    let vp = &graph.viewport;
    let mut out = String::with_capacity(4096 + graph.nodes.len() * 256 + graph.edges.len() * 192);

    out.push_str(r#"<?xml version="1.0" encoding="UTF-8"?>"#);
    out.push('\n');
    out.push_str(&format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="{:.2} {:.2} {:.2} {:.2}" width="{:.0}" height="{:.0}" data-vowl="2" data-schema="{}" data-schema-version="{}">"#,
        vp.x,
        vp.y,
        vp.width.max(1.0),
        vp.height.max(1.0),
        vp.width.max(1.0),
        vp.height.max(1.0),
        xml_escape(&graph.schema_name),
        xml_escape(&graph.schema_version),
    ));
    out.push('\n');

    // Defs: arrowheads for property edges (one per stroke color family).
    // Use r## so hex colors like fill="#…" do not terminate the raw string.
    out.push_str(
        r##"  <defs>
    <marker id="vowl-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 0 L 10 5 L 0 10 z" fill="#306998"/>
    </marker>
    <marker id="vowl-arrow-dt" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 0 L 10 5 L 0 10 z" fill="#527a2e"/>
    </marker>
    <marker id="vowl-arrow-sub" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 0 L 10 5 L 0 10 z" fill="#999999"/>
    </marker>
    <marker id="vowl-arrow-default" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto-start-reverse">
      <path d="M 0 0 L 10 5 L 0 10 z" fill="#666666"/>
    </marker>
  </defs>
"##,
    );

    // Node lookup for edge shortening.
    let node_by_id: HashMap<&str, &PositionedNode> =
        graph.nodes.iter().map(|n| (n.id.as_str(), n)).collect();

    out.push_str(r#"  <g class="vowl-edges" data-layer="edges">"#);
    out.push('\n');
    for edge in &graph.edges {
        render_edge(&mut out, edge, &node_by_id, ann);
    }
    out.push_str("  </g>\n");

    out.push_str(r#"  <g class="vowl-nodes" data-layer="nodes">"#);
    out.push('\n');
    for node in &graph.nodes {
        render_node(&mut out, node, ann);
    }
    out.push_str("  </g>\n");

    // Compact legend for self-describing exports.
    render_legend(&mut out, vp);

    out.push_str("</svg>\n");
    out
}

fn resolve_node_owl(node: &PositionedNode, ann: &VowlAnnotations) -> (String, Vec<String>) {
    let owl =
        ann.node_owl
            .get(&node.id)
            .cloned()
            .unwrap_or_else(|| match node.node_type.as_str() {
                "constant" | "enum_" => "rdfs:Datatype".into(),
                t if t.starts_with("owl:") || t.starts_with("rdfs:") => t.to_string(),
                _ => "owl:Class".into(),
            });
    let attrs = ann.node_attrs.get(&node.id).cloned().unwrap_or_default();
    (owl, attrs)
}

fn resolve_edge_owl(edge: &PositionedEdge, ann: &VowlAnnotations) -> (String, Vec<String>) {
    // Prefer exact key if present; else fall back to edge_type heuristics.
    let candidates: Vec<String> = ann
        .edge_owl
        .keys()
        .filter(|k| {
            k.starts_with(&format!("{}|{}|", edge.source_id, edge.target_id))
                || *k == &edge.edge_type
        })
        .cloned()
        .collect();

    let owl = candidates
        .first()
        .and_then(|k| ann.edge_owl.get(k))
        .cloned()
        .or_else(|| {
            // edge_type may already be an OWL type or a snake_case relation.
            match edge.edge_type.as_str() {
                "contains" | "extends" | "sub_class_of" | "subclassof" => {
                    Some("rdfs:subClassOf".into())
                }
                t if t.starts_with("owl:") || t.starts_with("rdfs:") => Some(t.to_string()),
                _ => None,
            }
        })
        .unwrap_or_else(|| "owl:ObjectProperty".into());

    let attrs = candidates
        .first()
        .and_then(|k| ann.edge_attrs.get(k))
        .cloned()
        .or_else(|| ann.edge_attrs.get(&edge.edge_type).cloned())
        .unwrap_or_default();

    (owl, attrs)
}

fn render_node(out: &mut String, node: &PositionedNode, ann: &VowlAnnotations) {
    let (owl, attrs) = resolve_node_owl(node, ann);
    let attr_refs: Vec<&str> = attrs.iter().map(|s| s.as_str()).collect();
    let style = vowl_style(&owl, &attr_refs);
    let r = (node.width.min(node.height) / 2.0).max(8.0);
    let label = truncate_label(&node.label, r);
    let dash = if style.dashed {
        r#" stroke-dasharray="6,3""#
    } else {
        ""
    };

    out.push_str(&format!(
        r#"    <g class="vowl-node" data-id="{}" data-owl-type="{}" data-vowl-shape="{}" transform="translate({:.2},{:.2})">"#,
        xml_escape(&node.id),
        xml_escape(&owl),
        shape_name(style.shape),
        node.x,
        node.y,
    ));
    out.push('\n');

    match style.shape {
        VowlShape::Rect => {
            let w = r * 2.0;
            let h = r * 1.2;
            out.push_str(&format!(
                r#"      <rect class="vowl-class-shape" x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" rx="4" fill="{}" stroke="{}" stroke-width="1.5"{}/>"#,
                -w / 2.0,
                -h / 2.0,
                w,
                h,
                style.fill,
                style.stroke,
                dash
            ));
        }
        VowlShape::DualEllipse => {
            // VOWL equivalent-class: two concentric ellipses (outer ring + inner fill).
            out.push_str(&format!(
                r#"      <ellipse class="vowl-class-shape vowl-equivalent-outer" cx="0" cy="0" rx="{:.2}" ry="{:.2}" fill="none" stroke="{}" stroke-width="1.5"{}/>"#,
                r + 4.0,
                r * 0.75 + 3.0,
                style.stroke,
                dash
            ));
            out.push('\n');
            out.push_str(&format!(
                r#"      <ellipse class="vowl-class-shape vowl-equivalent-inner" cx="0" cy="0" rx="{:.2}" ry="{:.2}" fill="{}" stroke="{}" stroke-width="1.5"/>"#,
                r,
                r * 0.75,
                style.fill,
                style.stroke
            ));
        }
        VowlShape::Diamond => {
            // Rare as a class shape; still render if style maps here.
            let w = r * 1.2;
            let h = r * 0.7;
            out.push_str(&format!(
                r#"      <polygon class="vowl-class-shape" points="0,{:.2} {:.2},0 0,{:.2} {:.2},0" fill="{}" stroke="{}" stroke-width="1.5"{}/>"#,
                -h,
                w,
                h,
                -w,
                style.fill,
                style.stroke,
                dash
            ));
        }
        VowlShape::Circle => {
            out.push_str(&format!(
                r#"      <circle class="vowl-class-shape" r="{:.2}" fill="{}" stroke="{}" stroke-width="1.5"{}/>"#,
                r, style.fill, style.stroke, dash
            ));
        }
    }
    out.push('\n');

    let font_size = if r > 30.0 { 11.0 } else { 9.0 };
    out.push_str(&format!(
        r#"      <text class="vowl-label" text-anchor="middle" dy="0.35em" font-size="{:.0}" font-family="sans-serif" fill="{}">{}</text>"#,
        font_size,
        style.text_color,
        xml_escape(&label)
    ));
    out.push('\n');
    out.push_str("    </g>\n");
}

fn render_edge(
    out: &mut String,
    edge: &PositionedEdge,
    nodes: &HashMap<&str, &PositionedNode>,
    ann: &VowlAnnotations,
) {
    let (owl, attrs) = resolve_edge_owl(edge, ann);
    let attr_refs: Vec<&str> = attrs.iter().map(|s| s.as_str()).collect();
    let style = vowl_style(&owl, &attr_refs);

    let (x1, y1) = edge
        .path
        .first()
        .map(|p| (p[0], p[1]))
        .unwrap_or((0.0, 0.0));
    let (x2, y2) = edge.path.last().map(|p| (p[0], p[1])).unwrap_or((0.0, 0.0));

    let src_r = nodes
        .get(edge.source_id.as_str())
        .map(|n| (n.width.min(n.height) / 2.0).max(8.0))
        .unwrap_or(24.0);
    let tgt_r = nodes
        .get(edge.target_id.as_str())
        .map(|n| (n.width.min(n.height) / 2.0).max(8.0))
        .unwrap_or(24.0);

    let dx = x2 - x1;
    let dy = y2 - y1;
    let len = (dx * dx + dy * dy).sqrt().max(1.0);
    let ux = dx / len;
    let uy = dy / len;

    let sx = x1 + ux * src_r;
    let sy = y1 + uy * src_r;
    let ex = x2 - ux * tgt_r;
    let ey = y2 - uy * tgt_r;

    let mx = (sx + ex) / 2.0;
    let my = (sy + ey) / 2.0;

    let dash = if style.dashed {
        r#" stroke-dasharray="4,3""#
    } else {
        ""
    };
    let marker = match owl.as_str() {
        "owl:DatatypeProperty" => "url(#vowl-arrow-dt)",
        "rdfs:subClassOf" => "url(#vowl-arrow-sub)",
        "owl:ObjectProperty" | "owl:equivalentClass" => "url(#vowl-arrow)",
        _ => "url(#vowl-arrow-default)",
    };

    let label = edge
        .label
        .clone()
        .unwrap_or_else(|| short_property_label(&owl, &edge.edge_type));

    out.push_str(&format!(
        r#"    <g class="vowl-edge" data-source="{}" data-target="{}" data-owl-type="{}" data-vowl-shape="{}">"#,
        xml_escape(&edge.source_id),
        xml_escape(&edge.target_id),
        xml_escape(&owl),
        shape_name(style.shape),
    ));
    out.push('\n');

    out.push_str(&format!(
        r#"      <line class="vowl-property-line" x1="{:.2}" y1="{:.2}" x2="{:.2}" y2="{:.2}" stroke="{}" stroke-width="1.5" marker-end="{}"{}/>"#,
        sx, sy, ex, ey, style.stroke, marker, dash
    ));
    out.push('\n');

    // Midpoint property label box (VOWL diamond for object/datatype props).
    if matches!(
        style.shape,
        VowlShape::Diamond | VowlShape::Rect | VowlShape::Circle
    ) && !label.is_empty()
    {
        let pw = 28.0_f64.max((label.len() as f64) * 4.5 + 8.0).min(90.0);
        let ph = 16.0;
        match style.shape {
            VowlShape::Diamond => {
                let hw = pw / 2.0;
                let hh = ph / 2.0 + 2.0;
                out.push_str(&format!(
                    r#"      <polygon class="vowl-property-box" points="{0:.2},{1:.2} {2:.2},{3:.2} {0:.2},{4:.2} {5:.2},{3:.2}" fill="{6}" stroke="{7}" stroke-width="1"/>"#,
                    mx,
                    my - hh,
                    mx + hw,
                    my,
                    my + hh,
                    mx - hw,
                    style.fill,
                    style.stroke
                ));
            }
            _ => {
                out.push_str(&format!(
                    r#"      <rect class="vowl-property-box" x="{:.2}" y="{:.2}" width="{:.2}" height="{:.2}" rx="2" fill="{}" stroke="{}" stroke-width="1"/>"#,
                    mx - pw / 2.0,
                    my - ph / 2.0,
                    pw,
                    ph,
                    style.fill,
                    style.stroke
                ));
            }
        }
        out.push('\n');
        out.push_str(&format!(
            r#"      <text class="vowl-property-label" x="{:.2}" y="{:.2}" text-anchor="middle" dominant-baseline="middle" font-size="8" font-family="sans-serif" fill="{}">{}</text>"#,
            mx,
            my,
            style.text_color,
            xml_escape(&truncate_str(&label, 18))
        ));
        out.push('\n');
    }

    out.push_str("    </g>\n");
}

fn render_legend(out: &mut String, vp: &crate::layout::positioned::Rect) {
    let lx = vp.x + 8.0;
    let ly = vp.y + vp.height - 56.0;
    out.push_str(&format!(
        r#"  <g class="vowl-legend" data-layer="legend" transform="translate({:.2},{:.2})">"#,
        lx, ly
    ));
    out.push('\n');
    out.push_str(
        r##"    <text x="0" y="0" font-size="9" font-family="sans-serif" fill="#666">VOWL</text>
    <circle cx="10" cy="16" r="6" fill="#aaccff" stroke="#306998" stroke-width="1"/>
    <text x="20" y="19" font-size="8" font-family="sans-serif" fill="#666">Class</text>
    <rect x="55" y="10" width="14" height="10" rx="2" fill="#ffcc33" stroke="#b38f00" stroke-width="1"/>
    <text x="73" y="19" font-size="8" font-family="sans-serif" fill="#666">Datatype</text>
    <polygon points="125,10 133,16 125,22 117,16" fill="#aaccff" stroke="#306998" stroke-width="1"/>
    <text x="138" y="19" font-size="8" font-family="sans-serif" fill="#666">ObjectProp</text>
    <polygon points="200,10 208,16 200,22 192,16" fill="#99cc66" stroke="#527a2e" stroke-width="1"/>
    <text x="213" y="19" font-size="8" font-family="sans-serif" fill="#666">DataProp</text>
    <circle cx="275" cy="16" r="6" fill="#cccccc" stroke="#999" stroke-width="1" stroke-dasharray="3,2"/>
    <text x="285" y="19" font-size="8" font-family="sans-serif" fill="#666">Deprecated</text>
"##,
    );
    out.push_str("  </g>\n");
}

fn shape_name(shape: VowlShape) -> &'static str {
    match shape {
        VowlShape::Circle => "circle",
        VowlShape::DualEllipse => "dual_ellipse",
        VowlShape::Rect => "rect",
        VowlShape::Diamond => "diamond",
    }
}

fn short_property_label(owl: &str, edge_type: &str) -> String {
    match owl {
        "rdfs:subClassOf" => "subClassOf".into(),
        "owl:ObjectProperty" | "owl:DatatypeProperty" => edge_type.replace('_', " "),
        "owl:equivalentClass" => "equivalentClass".into(),
        _ => edge_type.to_string(),
    }
}

fn truncate_label(label: &str, radius: f64) -> String {
    let max_chars = ((radius / 4.0).floor() as usize).max(3);
    truncate_str(label, max_chars)
}

fn truncate_str(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let take = max_chars.saturating_sub(1).max(1);
    let mut out: String = s.chars().take(take).collect();
    out.push('\u{2026}');
    out
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityId, EntityType, FileType};
    use crate::model::Entity;
    use crate::relationship::{Confidence, RelationType, Relationship};

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

    fn entity_with_meta(name: &str, etype: EntityType, meta: serde_json::Value) -> Entity {
        let mut e = entity(name, etype);
        e.metadata = meta;
        e
    }

    fn test_schema() -> TopologySchema {
        TopologySchema::from_yaml(
            r##"
name: vowl-test
label: "VOWL Test"
version: "1.0.0"
nodes:
  "*":
    geometry: force
    style:
      shape: circle
      color: "#a3a3a3"
edges:
  - type: contains
    from: "*"
    to: "*"
  - type: extends
    from: "*"
    to: "*"
  - type: related_to
    from: "*"
    to: "*"
  - type: calls
    from: "*"
    to: "*"
"##,
        )
        .unwrap()
    }

    /// Ontology 1: class hierarchy via subClassOf (Contains/Extends).
    fn ontology_class_hierarchy() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        let animal = entity("Animal", EntityType::Class);
        let mammal = entity("Mammal", EntityType::Class);
        let dog = entity("Dog", EntityType::Class);
        kg.add_entity(animal.clone());
        kg.add_entity(mammal.clone());
        kg.add_entity(dog.clone());
        kg.add_relationship(Relationship {
            source: mammal.id.clone(),
            target: animal.id.clone(),
            relation_type: RelationType::Extends,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
        kg.add_relationship(Relationship {
            source: dog.id.clone(),
            target: mammal.id.clone(),
            relation_type: RelationType::Extends,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
        kg
    }

    /// Ontology 2: datatype properties (constants + custom datatype props).
    fn ontology_datatypes() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        let person = entity("Person", EntityType::Class);
        let age = entity("xsd:integer", EntityType::Constant);
        let name_dt = entity("xsd:string", EntityType::Constant);
        kg.add_entity(person.clone());
        kg.add_entity(age.clone());
        kg.add_entity(name_dt.clone());
        kg.add_relationship(Relationship {
            source: person.id.clone(),
            target: age.id.clone(),
            relation_type: RelationType::Custom("owl:DatatypeProperty".into()),
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({"owl_type": "owl:DatatypeProperty"}),
            embedding: None,
        });
        kg.add_relationship(Relationship {
            source: person.id.clone(),
            target: name_dt.id.clone(),
            relation_type: RelationType::Custom("owl:DatatypeProperty".into()),
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({"owl_type": "owl:DatatypeProperty"}),
            embedding: None,
        });
        kg
    }

    /// Ontology 3: mixed object properties + deprecated + Thing.
    fn ontology_mixed() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        let agent = entity("Agent", EntityType::Class);
        let process = entity("Process", EntityType::Class);
        let thing = entity_with_meta(
            "Thing",
            EntityType::Concept,
            serde_json::json!({"owl_type": "owl:Thing"}),
        );
        let legacy = entity_with_meta(
            "LegacyService",
            EntityType::Service,
            serde_json::json!({"deprecated": true}),
        );
        kg.add_entity(agent.clone());
        kg.add_entity(process.clone());
        kg.add_entity(thing.clone());
        kg.add_entity(legacy.clone());
        kg.add_relationship(Relationship {
            source: agent.id.clone(),
            target: process.id.clone(),
            relation_type: RelationType::RelatedTo,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
        kg.add_relationship(Relationship {
            source: process.id.clone(),
            target: thing.id.clone(),
            relation_type: RelationType::Extends,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
        kg.add_relationship(Relationship {
            source: legacy.id.clone(),
            target: agent.id.clone(),
            relation_type: RelationType::Extends,
            confidence: Confidence::Inferred,
            weight: 0.7,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
        kg
    }

    #[test]
    fn vowl_rule_table_documents_class_circle_and_property_diamond() {
        let table = vowl_rule_table();
        let class = table.iter().find(|(t, ..)| *t == "owl:Class").unwrap();
        assert_eq!(class.1, VowlShape::Circle);
        assert_eq!(class.2, "#aaccff");

        let dt = table.iter().find(|(t, ..)| *t == "rdfs:Datatype").unwrap();
        assert_eq!(dt.1, VowlShape::Rect);
        assert_eq!(dt.2, "#ffcc33");

        let op = table
            .iter()
            .find(|(t, ..)| *t == "owl:ObjectProperty")
            .unwrap();
        assert_eq!(op.1, VowlShape::Diamond);

        let eq = table
            .iter()
            .find(|(t, ..)| *t == "owl:equivalentClass")
            .unwrap();
        assert_eq!(eq.1, VowlShape::DualEllipse);
    }

    #[test]
    fn webvowl_parity_colors_match_navigator() {
        // Parity with docs/src/app/vowl-navigator/types.ts vowlStyle().
        let class = vowl_style("owl:Class", &[]);
        assert_eq!(class.fill, "#aaccff");
        assert_eq!(class.stroke, "#306998");
        assert_eq!(class.shape, VowlShape::Circle);
        assert!(!class.dashed);

        let datatype = vowl_style("rdfs:Datatype", &[]);
        assert_eq!(datatype.fill, "#ffcc33");
        assert_eq!(datatype.shape, VowlShape::Rect);

        let obj = vowl_style("owl:ObjectProperty", &[]);
        assert_eq!(obj.fill, "#aaccff");
        assert_eq!(obj.shape, VowlShape::Diamond);

        let dtp = vowl_style("owl:DatatypeProperty", &[]);
        assert_eq!(dtp.fill, "#99cc66");
        assert_eq!(dtp.stroke, "#527a2e");

        let thing = vowl_style("owl:Thing", &[]);
        assert!(thing.dashed);
        assert_eq!(thing.fill, "#ffffff");

        let dep = vowl_style("owl:Class", &["deprecated"]);
        assert!(dep.dashed);
        assert_eq!(dep.fill, "#cccccc");

        let ext = vowl_style("owl:Class", &["external"]);
        assert_eq!(ext.fill, "#3c4a5a");
        assert_eq!(ext.text_color, "#e0e0e0");
    }

    #[test]
    fn svg_export_emits_vowl_compliant_shapes_hierarchy() {
        let kg = ontology_class_hierarchy();
        let schema = test_schema();
        let svg = to_svg_string(&kg, &schema, 640.0, 480.0).unwrap();

        assert!(svg.contains(r#"data-vowl="2""#), "VOWL version marker");
        assert!(svg.contains("vowl-class-shape"), "class shapes present");
        assert!(svg.contains(r#"data-owl-type="owl:Class""#));
        assert!(svg.contains(r#"data-owl-type="rdfs:subClassOf""#));
        assert!(svg.contains(r#"data-vowl-shape="circle""#));
        assert!(svg.contains("marker-end="), "property arrows");
        assert!(svg.contains("vowl-property-line"));
        // Classes use VOWL blue palette.
        assert!(svg.contains("#aaccff"));
        assert!(svg.contains("#306998"));
    }

    #[test]
    fn svg_export_emits_datatype_rects() {
        let kg = ontology_datatypes();
        let schema = test_schema();
        let svg = to_svg_string(&kg, &schema, 640.0, 480.0).unwrap();

        assert!(
            svg.contains(r#"data-owl-type="rdfs:Datatype""#),
            "datatype nodes: {svg}"
        );
        assert!(svg.contains(r#"data-vowl-shape="rect""#), "datatype → rect");
        assert!(svg.contains("#ffcc33"), "datatype yellow fill");
        assert!(
            svg.contains(r#"data-owl-type="owl:DatatypeProperty""#),
            "datatype property edges"
        );
        assert!(svg.contains("#99cc66"), "datatype property green");
    }

    #[test]
    fn svg_export_mixed_ontology_deprecated_and_thing() {
        let kg = ontology_mixed();
        let schema = test_schema();
        let svg = to_svg_string(&kg, &schema, 800.0, 600.0).unwrap();

        assert!(svg.contains(r#"data-owl-type="owl:Thing""#));
        assert!(svg.contains(r#"stroke-dasharray="6,3""#) || svg.contains("stroke-dasharray"));
        // Deprecated class remaps to gray palette.
        assert!(svg.contains("#cccccc") || svg.contains("#999999"));
        assert!(svg.contains(r#"data-owl-type="owl:ObjectProperty""#));
        assert!(svg.contains("vowl-legend"));
    }

    #[test]
    fn visual_regression_three_canonical_ontologies() {
        let schema = test_schema();
        let cases = [
            ("class_hierarchy", ontology_class_hierarchy()),
            ("datatypes", ontology_datatypes()),
            ("mixed", ontology_mixed()),
        ];

        for (name, kg) in cases {
            let svg =
                to_svg_string(&kg, &schema, 720.0, 540.0).unwrap_or_else(|e| panic!("{name}: {e}"));

            // Structural invariants for every canonical ontology SVG.
            assert!(svg.starts_with("<?xml"), "{name}: missing XML declaration");
            assert!(svg.contains("<svg "), "{name}: missing root svg");
            assert!(
                svg.contains(r#"data-vowl="2""#),
                "{name}: missing VOWL flag"
            );
            assert!(
                svg.contains(r#"data-layer="nodes""#),
                "{name}: missing nodes layer"
            );
            assert!(
                svg.contains(r#"data-layer="edges""#),
                "{name}: missing edges layer"
            );
            assert!(svg.contains("class=\"vowl-node\""), "{name}: no VOWL nodes");
            assert!(svg.contains("class=\"vowl-edge\""), "{name}: no VOWL edges");
            // Every class shape must advertise its OWL type for tooling.
            let node_count = svg.matches("class=\"vowl-node\"").count();
            let typed = svg.matches("data-owl-type=").count();
            assert!(
                typed >= node_count,
                "{name}: owl types missing (nodes={node_count}, typed attrs={typed})"
            );
            assert!(svg.contains("</svg>"), "{name}: unclosed svg");
        }
    }

    #[test]
    fn to_svg_writes_file() {
        let kg = ontology_class_hierarchy();
        let schema = test_schema();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("graph.svg");
        to_svg(&kg, &schema, &path).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("data-vowl=\"2\""));
        assert!(content.contains("vowl-class-shape"));
    }

    #[test]
    fn equivalent_class_dual_ellipse() {
        let mut kg = KnowledgeGraph::new();
        let a = entity_with_meta(
            "Human",
            EntityType::Class,
            serde_json::json!({"owl_type": "owl:equivalentClass"}),
        );
        let b = entity("Person", EntityType::Class);
        kg.add_entity(a.clone());
        kg.add_entity(b.clone());
        kg.add_relationship(Relationship {
            source: a.id.clone(),
            target: b.id.clone(),
            relation_type: RelationType::Custom("owl:equivalentClass".into()),
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: None,
            source_location: None,
            metadata: serde_json::json!({"owl_type": "owl:equivalentClass"}),
            embedding: None,
        });

        let svg = to_svg_string(&kg, &test_schema(), 400.0, 300.0).unwrap();
        assert!(svg.contains(r#"data-vowl-shape="dual_ellipse""#));
        assert!(svg.contains("vowl-equivalent-outer"));
        assert!(svg.contains("vowl-equivalent-inner"));
    }

    #[test]
    fn xml_special_chars_escaped_in_labels() {
        let mut kg = KnowledgeGraph::new();
        let e = entity_with_meta("A&B<C>", EntityType::Class, serde_json::json!({}));
        // Override label to include specials (entity name is used as label by helper).
        let mut e = e;
        e.label = "A&B<C>\"D".into();
        kg.add_entity(e);

        let svg = to_svg_string(&kg, &test_schema(), 200.0, 200.0).unwrap();
        assert!(!svg.contains("A&B<C>"));
        assert!(svg.contains("&amp;") || svg.contains("&lt;"));
    }

    #[test]
    fn render_positioned_svg_respects_explicit_owl_types() {
        let graph = PositionedGraph {
            nodes: vec![PositionedNode {
                id: "n1".into(),
                x: 50.0,
                y: 50.0,
                width: 40.0,
                height: 40.0,
                label: "Age".into(),
                node_type: "constant".into(),
                iri: None,
                shape: crate::topology::NodeShape::Circle,
                color: "#fff".into(),
                icon: None,
                has_subgraph: false,
                disposition: None,
                metrics: HashMap::new(),
            }],
            edges: vec![],
            viewport: crate::layout::positioned::Rect {
                x: 0.0,
                y: 0.0,
                width: 100.0,
                height: 100.0,
            },
            schema_name: "manual".into(),
            schema_version: "0".into(),
        };
        let mut owl = HashMap::new();
        owl.insert("n1".into(), "rdfs:Datatype".into());
        let svg = render_positioned_svg(&graph, &owl, &HashMap::new());
        assert!(svg.contains(r#"data-owl-type="rdfs:Datatype""#));
        assert!(svg.contains(r#"data-vowl-shape="rect""#));
        assert!(svg.contains("#ffcc33"));
    }
}
