//! Neo4j Cypher text export (GRAPH-024 / WEFT-387).
//!
//! Generates a `.cypher` script of `MERGE` statements that can be re-run
//! idempotently against Neo4j. This is a **standalone text exporter** — no
//! Neo4j driver or kernel bridge is required. The empty `neo4j-export`
//! feature remains reserved for a future live-driver push path.
//!
//! # Format
//!
//! - Node labels: derived from `file_type` (capitalized, ASCII alphanumeric
//!   only; fallback `Entity`)
//! - Relationship types: uppercase; non-alphanumeric → `_`
//! - Injection protection: backslash and single-quote escaped in string literals

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::GraphifyError;
use crate::entity::{EntityId, FileType};
use crate::model::KnowledgeGraph;

/// Escape a string for safe embedding inside a Cypher single-quoted literal.
///
/// Escapes `\` and `'` only (Neo4j Cypher string rules).
pub fn cypher_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// Sanitize a Neo4j node label from a graphify [`FileType`].
///
/// Capitalized, ASCII alphanumeric only; empty results fall back to `"Entity"`.
pub fn node_label_from_file_type(file_type: FileType) -> String {
    let raw = match file_type {
        FileType::Code => "Code",
        FileType::Document => "Document",
        FileType::Paper => "Paper",
        FileType::Image => "Image",
        FileType::Config => "Config",
        FileType::Unknown => "Unknown",
    };
    let alnum: String = raw.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    if alnum.is_empty() {
        "Entity".into()
    } else {
        alnum
    }
}

/// Sanitize an arbitrary label string (capitalized alnum, fallback `Entity`).
pub fn sanitize_node_label(raw: &str) -> String {
    let mut chars = raw.chars().filter(|c| c.is_ascii_alphanumeric());
    let Some(first) = chars.next() else {
        return "Entity".into();
    };
    let mut out = String::new();
    out.push(first.to_ascii_uppercase());
    for c in chars {
        out.push(c);
    }
    if out.is_empty() { "Entity".into() } else { out }
}

/// Sanitize a relationship type for Cypher: uppercase, non-alnum → `_`.
///
/// Empty results fall back to `RELATED_TO`.
pub fn sanitize_rel_type(relation: &str) -> String {
    let s: String = relation
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_uppercase()
            } else {
                '_'
            }
        })
        .collect();
    if s.is_empty() || s.chars().all(|c| c == '_') {
        "RELATED_TO".into()
    } else {
        s
    }
}

fn entity_id_display(entity: &crate::model::Entity) -> String {
    if let Some(legacy) = entity.legacy_id.as_deref()
        && !legacy.is_empty()
    {
        return legacy.to_string();
    }
    entity.id.to_hex()
}

fn community_map(kg: &KnowledgeGraph) -> HashMap<&EntityId, usize> {
    kg.communities
        .as_ref()
        .map(|comms| {
            comms
                .iter()
                .flat_map(|(&cid, ids)| ids.iter().map(move |id| (id, cid)))
                .collect()
        })
        .unwrap_or_default()
}

/// Build Cypher script text without writing to disk.
pub fn to_cypher_string(kg: &KnowledgeGraph) -> String {
    let communities = community_map(kg);
    let mut lines: Vec<String> =
        Vec::with_capacity(8 + kg.entity_count() * 3 + kg.relationship_count() * 4);

    lines.push("// Graphify Neo4j Cypher export".into());
    lines.push("// MERGE-based — safe to re-run".into());
    lines.push(format!(
        "// nodes: {}  edges: {}",
        kg.entity_count(),
        kg.relationship_count()
    ));
    lines.push(String::new());

    // Nodes
    for entity in kg.entities() {
        let id = cypher_escape(&entity_id_display(entity));
        let label = node_label_from_file_type(entity.file_type);
        let node_label_prop = cypher_escape(&entity.label);
        let entity_type = cypher_escape(entity.entity_type.discriminant());
        let source_file = entity
            .source_file
            .as_deref()
            .map(cypher_escape)
            .unwrap_or_default();
        let source_location = entity
            .source_location
            .as_deref()
            .map(cypher_escape)
            .unwrap_or_default();

        let ft = file_type_str(entity.file_type);
        lines.push(format!("MERGE (n:{label} {{id: '{id}'}})"));
        let mut set = format!(
            "SET n.label = '{node_label_prop}', n.entity_type = '{entity_type}', n.source_file = '{source_file}', n.source_location = '{source_location}', n.file_type = '{ft}'"
        );
        if let Some(&cid) = communities.get(&entity.id) {
            set.push_str(&format!(", n.community = {cid}"));
        }
        if let Some(iri) = entity.iri.as_deref() {
            set.push_str(&format!(", n.iri = '{}'", cypher_escape(iri)));
        }
        lines.push(set);
        lines.push(";".into());
        lines.push(String::new());
    }

    // Relationships
    for (src, tgt, rel) in kg.edges() {
        let src_id = cypher_escape(&entity_id_display(src));
        let tgt_id = cypher_escape(&entity_id_display(tgt));
        let rel_type = sanitize_rel_type(&rel.relation_type.as_str());
        let confidence = rel.confidence.as_str();
        let weight = rel.weight;
        let source_file = rel
            .source_file
            .as_deref()
            .or(src.source_file.as_deref())
            .map(cypher_escape)
            .unwrap_or_default();

        lines.push(format!(
            "MATCH (a {{id: '{src_id}'}}), (b {{id: '{tgt_id}'}})"
        ));
        lines.push(format!("MERGE (a)-[r:{rel_type}]->(b)"));
        lines.push(format!(
            "SET r.confidence = '{confidence}', r.weight = {weight}, r.source_file = '{source_file}'"
        ));
        lines.push(";".into());
        lines.push(String::new());
    }

    lines.join("\n")
}

fn file_type_str(ft: FileType) -> &'static str {
    match ft {
        FileType::Code => "code",
        FileType::Document => "document",
        FileType::Paper => "paper",
        FileType::Image => "image",
        FileType::Config => "config",
        FileType::Unknown => "unknown",
    }
}

/// Export the knowledge graph to a Neo4j Cypher script file.
pub fn to_cypher(kg: &KnowledgeGraph, output: &Path) -> Result<(), GraphifyError> {
    let body = to_cypher_string(kg);
    if let Some(parent) = output.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent).map_err(|e| {
            GraphifyError::ExportError(format!("create parent {}: {e}", parent.display()))
        })?;
    }
    fs::write(output, body).map_err(|e| {
        GraphifyError::ExportError(format!("Failed to write {}: {e}", output.display()))
    })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::{DomainTag, EntityId, EntityType, FileType};
    use crate::model::Entity;
    use crate::relationship::{Confidence, RelationType, Relationship};
    use tempfile::TempDir;

    fn sample_kg() -> KnowledgeGraph {
        let mut kg = KnowledgeGraph::new();
        let e1 = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Module, "auth", "auth.py"),
            entity_type: EntityType::Module,
            label: "auth".into(),
            source_file: Some("auth.py".into()),
            source_location: Some("L1".into()),
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: Some("auth".into()),
            iri: None,
        };
        let e2 = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Module, "db", "db.py"),
            entity_type: EntityType::Module,
            label: "db".into(),
            source_file: Some("db.py".into()),
            source_location: Some("L1".into()),
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: Some("db".into()),
            iri: None,
        };
        kg.add_entity(e1.clone());
        kg.add_entity(e2.clone());
        kg.add_relationship(Relationship {
            source: e1.id.clone(),
            target: e2.id.clone(),
            relation_type: RelationType::Imports,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: Some("auth.py".into()),
            source_location: Some("L3".into()),
            metadata: serde_json::json!({}),
            embedding: None,
        });
        kg
    }

    #[test]
    fn cypher_escape_quotes_and_backslash() {
        assert_eq!(cypher_escape(r"it's a \path"), r"it\'s a \\path");
        assert_eq!(cypher_escape("plain"), "plain");
    }

    #[test]
    fn node_label_from_file_types() {
        assert_eq!(node_label_from_file_type(FileType::Code), "Code");
        assert_eq!(node_label_from_file_type(FileType::Document), "Document");
        assert_eq!(sanitize_node_label(""), "Entity");
        assert_eq!(sanitize_node_label("code!"), "Code");
    }

    #[test]
    fn rel_type_sanitization() {
        assert_eq!(sanitize_rel_type("imports"), "IMPORTS");
        assert_eq!(sanitize_rel_type("imports_from"), "IMPORTS_FROM");
        assert_eq!(sanitize_rel_type("owl:subClassOf"), "OWL_SUBCLASSOF");
        assert_eq!(sanitize_rel_type(""), "RELATED_TO");
        assert_eq!(sanitize_rel_type("!!!"), "RELATED_TO");
    }

    #[test]
    fn to_cypher_string_contains_merge_nodes_and_edges() {
        let kg = sample_kg();
        let body = to_cypher_string(&kg);
        assert!(body.contains("MERGE (n:Code {id: 'auth'})"), "{body}");
        assert!(body.contains("MERGE (n:Code {id: 'db'})"), "{body}");
        assert!(body.contains("MERGE (a)-[r:IMPORTS]->(b)"), "{body}");
        assert!(
            body.contains("MATCH (a {id: 'auth'}), (b {id: 'db'})"),
            "{body}"
        );
        assert!(body.contains("r.confidence = 'EXTRACTED'"), "{body}");
        // Idempotent: MERGE only, never bare CREATE for nodes/edges.
        assert!(!body.contains("CREATE ("), "{body}");
    }

    #[test]
    fn to_cypher_escapes_special_chars_in_labels() {
        let mut kg = KnowledgeGraph::new();
        let e = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Class, "evil", "x.py"),
            entity_type: EntityType::Class,
            label: r"O'Brien\path".into(),
            source_file: Some(r"dir\file.py".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: Some("evil".into()),
            iri: None,
        };
        kg.add_entity(e);
        let body = to_cypher_string(&kg);
        assert!(body.contains(r"O\'Brien\\path"), "{body}");
        assert!(body.contains(r"dir\\file.py"), "{body}");
    }

    #[test]
    fn community_property_emitted() {
        let mut kg = sample_kg();
        let ids: Vec<EntityId> = kg.node_ids().cloned().collect();
        let mut comms = HashMap::new();
        comms.insert(0, ids);
        kg.communities = Some(comms);

        let body = to_cypher_string(&kg);
        assert!(body.contains("n.community = 0"), "{body}");
    }

    #[test]
    fn to_cypher_writes_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("out").join("graph.cypher");
        let kg = sample_kg();
        to_cypher(&kg, &path).unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("MERGE (n:Code"));
        assert!(content.contains("IMPORTS"));
    }

    #[test]
    fn export_dispatcher_wires_cypher() {
        use crate::export::{ExportFormat, export};
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("graph.cypher");
        let kg = sample_kg();
        export(&kg, ExportFormat::Cypher, &path).unwrap();
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("MERGE"));
    }
}
