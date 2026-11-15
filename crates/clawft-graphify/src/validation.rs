//! Schema validation for extraction JSON, matching Python's `validate.py`.
//!
//! # WEFT-382 — schema-based edge validation
//!
//! In addition to JSON-shape checks, this module validates edges against a
//! declared schema of allowed `(relation, from_entity_type, to_entity_type)`
//! triples. Schemas load from:
//!
//! - [`TopologySchema`] YAML (canonical software / investigation schemas)
//! - WebVOWL-compatible JSON (`propertyAttribute` domain/range)
//! - RDF-style domain/range pairs
//! - programmatic constraint lists

use std::collections::HashMap;

use crate::GraphifyError;
use crate::entity::FileType;
use crate::model::KnowledgeGraph;
use crate::relationship::Confidence;
use crate::topology::{EdgeTypeConfig, TopologySchema};

/// Valid file type strings (Python compatibility -- does NOT include "config" or "unknown"
/// which are Rust-only extensions; the Python validator only knows these 4).
const VALID_FILE_TYPES_PYTHON: &[&str] = &["code", "document", "paper", "image"];

/// Required fields on each node object.
const REQUIRED_NODE_FIELDS: &[&str] = &["id", "label", "file_type", "source_file"];

/// Required fields on each edge object.
const REQUIRED_EDGE_FIELDS: &[&str] =
    &["source", "target", "relation", "confidence", "source_file"];

// ---------------------------------------------------------------------------
// Edge validation schema (WEFT-382)
// ---------------------------------------------------------------------------

/// A single allowed (relation, from_entity_type, to_entity_type) triple.
///
/// `from` / `to` may be `"*"` to match any entity type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeConstraint {
    pub relation: String,
    pub from: String,
    pub to: String,
}

impl EdgeConstraint {
    /// Construct a constraint with normalized (snake_case-ish lowercase) fields.
    pub fn new(
        relation: impl Into<String>,
        from: impl Into<String>,
        to: impl Into<String>,
    ) -> Self {
        Self {
            relation: normalize_type_key(&relation.into()),
            from: normalize_type_key(&from.into()),
            to: normalize_type_key(&to.into()),
        }
    }
}

/// Schema-driven edge validator: relationship type ↔ allowed entity-type pairs.
///
/// When `strict_unknown_relations` is true (default), a relation with no
/// declared constraint is rejected. When false, only declared relations are
/// constrained; undeclared relations pass.
#[derive(Debug, Clone)]
pub struct EdgeValidationSchema {
    constraints: Vec<EdgeConstraint>,
    /// When true, unknown relation types are errors.
    pub strict_unknown_relations: bool,
}

impl EdgeValidationSchema {
    /// Empty schema (no allowed edges when strict).
    pub fn empty() -> Self {
        Self {
            constraints: Vec::new(),
            strict_unknown_relations: true,
        }
    }

    /// Programmatic construction from an explicit constraint list.
    pub fn programmatic(constraints: impl IntoIterator<Item = EdgeConstraint>) -> Self {
        Self {
            constraints: constraints.into_iter().collect(),
            strict_unknown_relations: true,
        }
    }

    /// Load from a topology schema (YAML / programmatic `TopologySchema`).
    pub fn from_topology(schema: &TopologySchema) -> Self {
        Self::from_edge_configs(&schema.edges)
    }

    /// Load from edge type configs (shared by topology + custom loaders).
    pub fn from_edge_configs(edges: &[EdgeTypeConfig]) -> Self {
        let constraints = edges
            .iter()
            .map(|e| EdgeConstraint::new(&e.edge_type, &e.from, &e.to))
            .collect();
        Self {
            constraints,
            strict_unknown_relations: true,
        }
    }

    /// Load from RDF-style `(relation, domain, range)` triples
    /// (e.g. `rdfs:domain` / `rdfs:range` local names or IRIs).
    pub fn from_domain_range_pairs(
        pairs: impl IntoIterator<Item = (String, String, String)>,
    ) -> Self {
        let constraints = pairs
            .into_iter()
            .map(|(rel, domain, range)| {
                EdgeConstraint::new(
                    local_name_or_key(&rel),
                    local_name_or_key(&domain),
                    local_name_or_key(&range),
                )
            })
            .collect();
        Self {
            constraints,
            strict_unknown_relations: true,
        }
    }

    /// Load from WebVOWL-compatible JSON.
    ///
    /// Uses `propertyAttribute[].label` (or `iri`) as the relation type and
    /// resolves `domain` / `range` class IDs via `classAttribute` labels / IRIs.
    /// Falls back to treating domain/range as already-resolved type keys when
    /// no matching classAttribute entry exists.
    pub fn from_vowl(vowl: &serde_json::Value) -> Result<Self, GraphifyError> {
        let props = vowl
            .get("propertyAttribute")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                GraphifyError::ValidationError("VOWL schema missing propertyAttribute array".into())
            })?;

        // class id → type key (prefer IRI local name, then English label, then id)
        let mut class_types: HashMap<String, String> = HashMap::new();
        if let Some(attrs) = vowl.get("classAttribute").and_then(|v| v.as_array()) {
            for ca in attrs {
                let id = ca.get("id").and_then(|v| v.as_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let key = class_type_key(ca);
                class_types.insert(id.to_string(), key);
            }
        }

        let mut constraints = Vec::new();
        for prop in props {
            let relation = property_relation_key(prop);
            if relation.is_empty() {
                continue;
            }
            let domain_raw = prop.get("domain").and_then(|v| v.as_str()).unwrap_or("*");
            let range_raw = prop.get("range").and_then(|v| v.as_str()).unwrap_or("*");

            let from = class_types
                .get(domain_raw)
                .cloned()
                .unwrap_or_else(|| normalize_type_key(domain_raw));
            let to = class_types
                .get(range_raw)
                .cloned()
                .unwrap_or_else(|| normalize_type_key(range_raw));

            constraints.push(EdgeConstraint::new(relation, from, to));
        }

        Ok(Self {
            constraints,
            strict_unknown_relations: true,
        })
    }

    /// Number of declared constraints.
    pub fn constraint_count(&self) -> usize {
        self.constraints.len()
    }

    /// Iterate constraints.
    pub fn constraints(&self) -> impl Iterator<Item = &EdgeConstraint> {
        self.constraints.iter()
    }

    /// Whether `(relation, from_type, to_type)` is allowed by this schema.
    pub fn allows(&self, relation: &str, from_type: &str, to_type: &str) -> bool {
        let rel = normalize_type_key(relation);
        let from = normalize_type_key(from_type);
        let to = normalize_type_key(to_type);

        let matching: Vec<&EdgeConstraint> = self
            .constraints
            .iter()
            .filter(|c| c.relation == rel || c.relation == "*")
            .collect();

        if matching.is_empty() {
            return !self.strict_unknown_relations;
        }

        matching
            .iter()
            .any(|c| type_matches(&c.from, &from) && type_matches(&c.to, &to))
    }
}

fn type_matches(pattern: &str, actual: &str) -> bool {
    pattern == "*" || pattern == actual
}

/// Normalize relation / entity type keys for comparison.
fn normalize_type_key(s: &str) -> String {
    let s = s.trim();
    if s.is_empty() {
        return String::new();
    }
    // IRI → local name, then lowercase with spaces/hyphens → underscores.
    let local = local_name_or_key(s);
    local
        .chars()
        .map(|c| match c {
            ' ' | '-' => '_',
            other => other.to_ascii_lowercase(),
        })
        .collect()
}

fn local_name_or_key(s: &str) -> String {
    if let Some(i) = s.rfind('#') {
        let rest = &s[i + 1..];
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    if s.contains("://")
        && let Some(i) = s.rfind('/')
    {
        let rest = &s[i + 1..];
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    s.to_string()
}

fn class_type_key(class_attr: &serde_json::Value) -> String {
    if let Some(iri) = class_attr.get("iri").and_then(|v| v.as_str()) {
        return normalize_type_key(iri);
    }
    if let Some(base) = class_attr.get("baseIri").and_then(|v| v.as_str()) {
        return normalize_type_key(base);
    }
    if let Some(label) = class_attr
        .get("label")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("en").or_else(|| m.values().next()))
        .and_then(|v| v.as_str())
    {
        return normalize_type_key(label);
    }
    if let Some(label) = class_attr.get("label").and_then(|v| v.as_str()) {
        return normalize_type_key(label);
    }
    class_attr
        .get("id")
        .and_then(|v| v.as_str())
        .map(normalize_type_key)
        .unwrap_or_default()
}

fn property_relation_key(prop: &serde_json::Value) -> String {
    if let Some(iri) = prop.get("iri").and_then(|v| v.as_str()) {
        return normalize_type_key(iri);
    }
    if let Some(label) = prop
        .get("label")
        .and_then(|v| v.as_object())
        .and_then(|m| m.get("en").or_else(|| m.values().next()))
        .and_then(|v| v.as_str())
    {
        return normalize_type_key(label);
    }
    if let Some(label) = prop.get("label").and_then(|v| v.as_str()) {
        return normalize_type_key(label);
    }
    prop.get("id")
        .and_then(|v| v.as_str())
        .map(normalize_type_key)
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Validation errors with entity context
// ---------------------------------------------------------------------------

/// A single schema edge-validation failure with entity context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeValidationError {
    /// Index in the edges array when validating JSON (None for KG walks).
    pub edge_index: Option<usize>,
    pub relation: String,
    pub source_id: String,
    pub source_type: String,
    pub source_label: Option<String>,
    pub target_id: String,
    pub target_type: String,
    pub target_label: Option<String>,
    pub reason: String,
}

impl EdgeValidationError {
    /// Human-readable error line for logs / `validate_extraction` style lists.
    pub fn to_message(&self) -> String {
        let idx = self
            .edge_index
            .map(|i| format!("Edge {i} "))
            .unwrap_or_default();
        let src_label = self
            .source_label
            .as_deref()
            .map(|l| format!(" label={l:?}"))
            .unwrap_or_default();
        let tgt_label = self
            .target_label
            .as_deref()
            .map(|l| format!(" label={l:?}"))
            .unwrap_or_default();
        format!(
            "{idx}relation '{}' not allowed from {}:{}{src_label} → {}:{}{tgt_label}: {}",
            self.relation,
            self.source_type,
            self.source_id,
            self.target_type,
            self.target_id,
            self.reason,
        )
    }
}

impl std::fmt::Display for EdgeValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.to_message())
    }
}

// ---------------------------------------------------------------------------
// JSON shape validation (existing)
// ---------------------------------------------------------------------------

/// Validate an extraction JSON value against the graphify schema.
///
/// Returns a list of error/warning strings. An empty list means the data is
/// valid. Dangling edge references (source/target not matching any node ID)
/// are included as warnings but are expected for external/stdlib imports.
pub fn validate_extraction(data: &serde_json::Value) -> Vec<String> {
    let mut errors = Vec::new();

    if !data.is_object() {
        errors.push("Extraction must be a JSON object".into());
        return errors;
    }

    // -- Nodes --
    match data.get("nodes") {
        None => errors.push("Missing required key 'nodes'".into()),
        Some(nodes) => {
            if let Some(arr) = nodes.as_array() {
                for (i, node) in arr.iter().enumerate() {
                    if !node.is_object() {
                        errors.push(format!("Node {i} must be an object"));
                        continue;
                    }
                    let node_id_display = node.get("id").and_then(|v| v.as_str()).unwrap_or("?");
                    for &field in REQUIRED_NODE_FIELDS {
                        if node.get(field).is_none() {
                            errors.push(format!(
                                "Node {i} (id={node_id_display:?}) missing required field '{field}'"
                            ));
                        }
                    }
                    if let Some(ft) = node.get("file_type").and_then(|v| v.as_str()) {
                        // Accept both Python-compatible and Rust-extended file types.
                        if !VALID_FILE_TYPES_PYTHON.contains(&ft)
                            && FileType::from_str_loose(ft).is_none()
                        {
                            errors.push(format!(
                                "Node {i} (id={node_id_display:?}) has invalid file_type '{ft}'"
                            ));
                        }
                    }
                }
            } else {
                errors.push("'nodes' must be a list".into());
            }
        }
    }

    // -- Edges --
    match data.get("edges") {
        None => errors.push("Missing required key 'edges'".into()),
        Some(edges) => {
            if let Some(arr) = edges.as_array() {
                // Build set of node IDs for reference checking.
                let node_ids: std::collections::HashSet<&str> = data
                    .get("nodes")
                    .and_then(|v| v.as_array())
                    .map(|nodes| {
                        nodes
                            .iter()
                            .filter_map(|n| n.get("id").and_then(|v| v.as_str()))
                            .collect()
                    })
                    .unwrap_or_default();

                for (i, edge) in arr.iter().enumerate() {
                    if !edge.is_object() {
                        errors.push(format!("Edge {i} must be an object"));
                        continue;
                    }
                    for &field in REQUIRED_EDGE_FIELDS {
                        if edge.get(field).is_none() {
                            errors.push(format!("Edge {i} missing required field '{field}'"));
                        }
                    }
                    if let Some(conf) = edge.get("confidence").and_then(|v| v.as_str())
                        && Confidence::from_str_loose(conf).is_none()
                    {
                        errors.push(format!(
                            "Edge {i} has invalid confidence '{conf}' - must be one of {:?}",
                            Confidence::VALID_STRINGS,
                        ));
                    }
                    // Dangling references: warn but don't block.
                    if !node_ids.is_empty() {
                        if let Some(src) = edge.get("source").and_then(|v| v.as_str())
                            && !node_ids.contains(src)
                        {
                            errors.push(format!(
                                "Edge {i} source '{src}' does not match any node id"
                            ));
                        }
                        if let Some(tgt) = edge.get("target").and_then(|v| v.as_str())
                            && !node_ids.contains(tgt)
                        {
                            errors.push(format!(
                                "Edge {i} target '{tgt}' does not match any node id"
                            ));
                        }
                    }
                }
            } else {
                errors.push("'edges' must be a list".into());
            }
        }
    }

    errors
}

/// JSON-shape validation plus optional schema-based edge checks (WEFT-382).
pub fn validate_extraction_with_schema(
    data: &serde_json::Value,
    edge_schema: Option<&EdgeValidationSchema>,
) -> Vec<String> {
    let mut errors = validate_extraction(data);
    if let Some(schema) = edge_schema {
        for err in validate_extraction_edges(data, schema) {
            errors.push(err.to_message());
        }
    }
    errors
}

// ---------------------------------------------------------------------------
// Schema-based edge validation (WEFT-382)
// ---------------------------------------------------------------------------

/// Validate extraction JSON edges against an [`EdgeValidationSchema`].
///
/// Nodes should carry `entity_type` (falls back to `"concept"` when missing,
/// matching `build_from_json`). Edges that reference missing endpoints are
/// skipped here (shape validation already reports dangling refs).
pub fn validate_extraction_edges(
    data: &serde_json::Value,
    schema: &EdgeValidationSchema,
) -> Vec<EdgeValidationError> {
    let mut errors = Vec::new();

    let nodes = match data.get("nodes").and_then(|v| v.as_array()) {
        Some(n) => n,
        None => return errors,
    };
    let edges = match data.get("edges").and_then(|v| v.as_array()) {
        Some(e) => e,
        None => return errors,
    };

    // id → (entity_type, label)
    let mut node_meta: HashMap<&str, (&str, Option<&str>)> = HashMap::new();
    for node in nodes {
        let id = match node.get("id").and_then(|v| v.as_str()) {
            Some(id) => id,
            None => continue,
        };
        let et = node
            .get("entity_type")
            .and_then(|v| v.as_str())
            .unwrap_or("concept");
        let label = node.get("label").and_then(|v| v.as_str());
        node_meta.insert(id, (et, label));
    }

    for (i, edge) in edges.iter().enumerate() {
        if !edge.is_object() {
            continue;
        }
        let relation = edge.get("relation").and_then(|v| v.as_str()).unwrap_or("");
        let src_id = edge.get("source").and_then(|v| v.as_str()).unwrap_or("");
        let tgt_id = edge.get("target").and_then(|v| v.as_str()).unwrap_or("");

        let (src_type, src_label) = match node_meta.get(src_id) {
            Some((t, l)) => (*t, l.map(|s| s.to_string())),
            None => continue, // dangling — reported by shape validator
        };
        let (tgt_type, tgt_label) = match node_meta.get(tgt_id) {
            Some((t, l)) => (*t, l.map(|s| s.to_string())),
            None => continue,
        };

        if schema.allows(relation, src_type, tgt_type) {
            continue;
        }

        let reason = if schema
            .constraints
            .iter()
            .any(|c| c.relation == normalize_type_key(relation) || c.relation == "*")
        {
            format!("no constraint allows {src_type} -[{relation}]-> {tgt_type}")
        } else {
            format!("unknown relation type '{relation}' (not declared in schema)")
        };

        errors.push(EdgeValidationError {
            edge_index: Some(i),
            relation: relation.to_string(),
            source_id: src_id.to_string(),
            source_type: src_type.to_string(),
            source_label: src_label,
            target_id: tgt_id.to_string(),
            target_type: tgt_type.to_string(),
            target_label: tgt_label,
            reason,
        });
    }

    errors
}

/// Validate edges of an in-memory [`KnowledgeGraph`] against a schema.
pub fn validate_kg_edges(
    kg: &KnowledgeGraph,
    schema: &EdgeValidationSchema,
) -> Vec<EdgeValidationError> {
    let mut errors = Vec::new();

    for (i, (src, tgt, rel)) in kg.edges().enumerate() {
        let relation = rel.relation_type_str();
        let src_type = src.entity_type.discriminant();
        let tgt_type = tgt.entity_type.discriminant();

        if schema.allows(&relation, src_type, tgt_type) {
            continue;
        }

        let reason = if schema
            .constraints
            .iter()
            .any(|c| c.relation == normalize_type_key(&relation) || c.relation == "*")
        {
            format!("no constraint allows {src_type} -[{relation}]-> {tgt_type}")
        } else {
            format!("unknown relation type '{relation}' (not declared in schema)")
        };

        errors.push(EdgeValidationError {
            edge_index: Some(i),
            relation,
            source_id: src.legacy_id.clone().unwrap_or_else(|| src.id.to_hex()),
            source_type: src_type.to_string(),
            source_label: Some(src.label.clone()),
            target_id: tgt.legacy_id.clone().unwrap_or_else(|| tgt.id.to_hex()),
            target_type: tgt_type.to_string(),
            target_label: Some(tgt.label.clone()),
            reason,
        });
    }

    errors
}

/// Convenience: reject invalid edges (returns `Err` with the first error message
/// and a count) or `Ok(())` when every edge is allowed.
pub fn reject_invalid_edges(
    kg: &KnowledgeGraph,
    schema: &EdgeValidationSchema,
) -> Result<(), GraphifyError> {
    let errors = validate_kg_edges(kg, schema);
    if errors.is_empty() {
        return Ok(());
    }
    Err(GraphifyError::ValidationError(format!(
        "{} schema edge error(s): {}",
        errors.len(),
        errors[0].to_message()
    )))
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

    const SOFTWARE_SCHEMA_YAML: &str = include_str!("../schemas/software.yaml");

    fn software_edge_schema() -> EdgeValidationSchema {
        let topo = TopologySchema::from_yaml(SOFTWARE_SCHEMA_YAML).expect("software.yaml parses");
        EdgeValidationSchema::from_topology(&topo)
    }

    #[test]
    fn valid_extraction_passes() {
        let data = serde_json::json!({
            "nodes": [
                {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            ],
            "edges": [
                {"source": "a", "target": "a", "relation": "self_ref", "confidence": "EXTRACTED", "source_file": "a.py"},
            ],
        });
        let errs = validate_extraction(&data);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    #[test]
    fn missing_nodes_key() {
        let data = serde_json::json!({"edges": []});
        let errs = validate_extraction(&data);
        assert!(
            errs.iter()
                .any(|e| e.contains("Missing required key 'nodes'"))
        );
    }

    #[test]
    fn invalid_file_type() {
        let data = serde_json::json!({
            "nodes": [
                {"id": "x", "label": "X", "file_type": "banana", "source_file": "x.py"},
            ],
            "edges": [],
        });
        let errs = validate_extraction(&data);
        assert!(errs.iter().any(|e| e.contains("invalid file_type")));
    }

    #[test]
    fn invalid_confidence() {
        let data = serde_json::json!({
            "nodes": [
                {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            ],
            "edges": [
                {"source": "a", "target": "a", "relation": "r", "confidence": "MAYBE", "source_file": "a.py"},
            ],
        });
        let errs = validate_extraction(&data);
        assert!(errs.iter().any(|e| e.contains("invalid confidence")));
    }

    #[test]
    fn dangling_edge_reference_is_warning() {
        let data = serde_json::json!({
            "nodes": [
                {"id": "a", "label": "A", "file_type": "code", "source_file": "a.py"},
            ],
            "edges": [
                {"source": "a", "target": "external", "relation": "imports", "confidence": "EXTRACTED", "source_file": "a.py"},
            ],
        });
        let errs = validate_extraction(&data);
        assert!(
            errs.iter()
                .any(|e| e.contains("does not match any node id"))
        );
    }

    #[test]
    fn missing_edge_fields() {
        let data = serde_json::json!({
            "nodes": [],
            "edges": [{"source": "a"}],
        });
        let errs = validate_extraction(&data);
        assert!(errs.iter().any(|e| e.contains("missing required field")));
    }

    #[test]
    fn rust_extended_file_types_valid() {
        let data = serde_json::json!({
            "nodes": [
                {"id": "a", "label": "A", "file_type": "config", "source_file": "a.toml"},
                {"id": "b", "label": "B", "file_type": "unknown", "source_file": "b.bin"},
            ],
            "edges": [],
        });
        let errs = validate_extraction(&data);
        assert!(errs.is_empty(), "unexpected errors: {errs:?}");
    }

    // ---- WEFT-382: schema-based edge validation ----

    #[test]
    fn software_schema_loads_edge_constraints() {
        let schema = software_edge_schema();
        assert!(
            schema.constraint_count() >= 7,
            "expected software.yaml edges, got {}",
            schema.constraint_count()
        );
        // calls: function → function
        assert!(schema.allows("calls", "function", "function"));
        // implements: class → interface
        assert!(schema.allows("implements", "class", "interface"));
        // contains is * → *
        assert!(schema.allows("contains", "module", "function"));
        assert!(schema.allows("contains", "package", "file"));
    }

    #[test]
    fn software_schema_rejects_invalid_pairs() {
        let schema = software_edge_schema();
        // calls only function → function
        assert!(!schema.allows("calls", "module", "function"));
        assert!(!schema.allows("calls", "class", "class"));
        // implements only class → interface
        assert!(!schema.allows("implements", "function", "interface"));
        assert!(!schema.allows("implements", "class", "class"));
        // method_of: function → class only
        assert!(!schema.allows("method_of", "function", "module"));
        // unknown relation under strict mode
        assert!(!schema.allows("teleports", "function", "function"));
    }

    #[test]
    fn programmatic_schema_allows_custom_constraints() {
        let schema = EdgeValidationSchema::programmatic(vec![
            EdgeConstraint::new("witnessed_by", "event", "person"),
            EdgeConstraint::new("located_at", "person", "location"),
        ]);
        assert!(schema.allows("witnessed_by", "event", "person"));
        assert!(!schema.allows("witnessed_by", "person", "event"));
        assert!(!schema.allows("calls", "function", "function"));
    }

    #[test]
    fn domain_range_pairs_from_rdf_style() {
        let schema = EdgeValidationSchema::from_domain_range_pairs(vec![
            (
                "https://weftos.weavelogic.ai/ontology/code#calls".into(),
                "https://weftos.weavelogic.ai/ontology/code#Function".into(),
                "https://weftos.weavelogic.ai/ontology/code#Function".into(),
            ),
            (
                "https://weftos.weavelogic.ai/ontology/code#implements".into(),
                "Class".into(),
                "Interface".into(),
            ),
        ]);
        assert!(schema.allows("calls", "function", "function"));
        assert!(schema.allows("implements", "class", "interface"));
        assert!(!schema.allows("implements", "class", "class"));
    }

    #[test]
    fn from_vowl_loads_property_domain_range() {
        let vowl = serde_json::json!({
            "classAttribute": [
                {
                    "id": "c0",
                    "iri": "https://weftos.weavelogic.ai/ontology/code#Function",
                    "label": {"en": "Function"}
                },
                {
                    "id": "c1",
                    "iri": "https://weftos.weavelogic.ai/ontology/code#Class",
                    "label": {"en": "Class"}
                },
                {
                    "id": "c2",
                    "iri": "https://weftos.weavelogic.ai/ontology/code#Interface",
                    "label": {"en": "Interface"}
                }
            ],
            "propertyAttribute": [
                {
                    "id": "p0",
                    "iri": "https://weftos.weavelogic.ai/ontology/code#calls",
                    "domain": "c0",
                    "range": "c0",
                    "label": {"en": "calls"}
                },
                {
                    "id": "p1",
                    "iri": "https://weftos.weavelogic.ai/ontology/code#implements",
                    "domain": "c1",
                    "range": "c2",
                    "label": {"en": "implements"}
                }
            ]
        });
        let schema = EdgeValidationSchema::from_vowl(&vowl).expect("vowl loads");
        assert_eq!(schema.constraint_count(), 2);
        assert!(schema.allows("calls", "function", "function"));
        assert!(schema.allows("implements", "class", "interface"));
        assert!(!schema.allows("calls", "class", "function"));
    }

    #[test]
    fn validate_extraction_edges_accepts_valid_software_graph() {
        let schema = software_edge_schema();
        let data = serde_json::json!({
            "nodes": [
                {"id": "m1", "label": "auth", "file_type": "code", "source_file": "auth.py", "entity_type": "module"},
                {"id": "f1", "label": "login", "file_type": "code", "source_file": "auth.py", "entity_type": "function"},
                {"id": "f2", "label": "hash_pw", "file_type": "code", "source_file": "auth.py", "entity_type": "function"},
                {"id": "c1", "label": "User", "file_type": "code", "source_file": "user.py", "entity_type": "class"},
                {"id": "i1", "label": "Authable", "file_type": "code", "source_file": "user.py", "entity_type": "interface"},
            ],
            "edges": [
                {"source": "m1", "target": "f1", "relation": "contains", "confidence": "EXTRACTED", "source_file": "auth.py"},
                {"source": "f1", "target": "f2", "relation": "calls", "confidence": "EXTRACTED", "source_file": "auth.py"},
                {"source": "c1", "target": "i1", "relation": "implements", "confidence": "EXTRACTED", "source_file": "user.py"},
                {"source": "f1", "target": "c1", "relation": "method_of", "confidence": "EXTRACTED", "source_file": "auth.py"},
            ],
        });
        let errs = validate_extraction_edges(&data, &schema);
        assert!(errs.is_empty(), "unexpected: {errs:?}");

        let combined = validate_extraction_with_schema(&data, Some(&schema));
        assert!(combined.is_empty(), "unexpected: {combined:?}");
    }

    #[test]
    fn validate_extraction_edges_rejects_invalid_with_entity_context() {
        let schema = software_edge_schema();
        let data = serde_json::json!({
            "nodes": [
                {"id": "m1", "label": "auth", "file_type": "code", "source_file": "auth.py", "entity_type": "module"},
                {"id": "f1", "label": "login", "file_type": "code", "source_file": "auth.py", "entity_type": "function"},
            ],
            "edges": [
                // invalid: calls must be function → function
                {"source": "m1", "target": "f1", "relation": "calls", "confidence": "EXTRACTED", "source_file": "auth.py"},
                // invalid: unknown relation
                {"source": "f1", "target": "m1", "relation": "teleports", "confidence": "EXTRACTED", "source_file": "auth.py"},
            ],
        });
        let errs = validate_extraction_edges(&data, &schema);
        assert_eq!(errs.len(), 2, "expected 2 rejections, got {errs:?}");

        let msg0 = errs[0].to_message();
        assert!(msg0.contains("calls"), "{msg0}");
        assert!(msg0.contains("module"), "{msg0}");
        assert!(msg0.contains("function"), "{msg0}");
        assert!(msg0.contains("auth") || msg0.contains("m1"), "{msg0}");
        assert!(msg0.contains("login") || msg0.contains("f1"), "{msg0}");

        let msg1 = errs[1].to_message();
        assert!(msg1.contains("teleports"), "{msg1}");
        assert!(
            msg1.contains("unknown relation") || msg1.contains("not declared"),
            "{msg1}"
        );
    }

    #[test]
    fn validate_kg_edges_and_reject_invalid() {
        let schema = software_edge_schema();
        let mut kg = KnowledgeGraph::new();

        let f1 = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Function, "login", "a.py"),
            entity_type: EntityType::Function,
            label: "login".into(),
            iri: None,
            source_file: Some("a.py".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: Some("f1".into()),
        };
        let f2 = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Function, "hash", "a.py"),
            entity_type: EntityType::Function,
            label: "hash".into(),
            iri: None,
            source_file: Some("a.py".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: Some("f2".into()),
        };
        let m1 = Entity {
            id: EntityId::new(&DomainTag::Code, &EntityType::Module, "auth", "a.py"),
            entity_type: EntityType::Module,
            label: "auth".into(),
            iri: None,
            source_file: Some("a.py".into()),
            source_location: None,
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: Some("m1".into()),
        };

        kg.add_entity(f1.clone());
        kg.add_entity(f2.clone());
        kg.add_entity(m1.clone());

        // Valid: function calls function
        kg.add_relationship(Relationship {
            source: f1.id.clone(),
            target: f2.id.clone(),
            relation_type: RelationType::Calls,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: Some("a.py".into()),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
        assert!(reject_invalid_edges(&kg, &schema).is_ok());

        // Invalid: module calls function
        kg.add_relationship(Relationship {
            source: m1.id.clone(),
            target: f1.id.clone(),
            relation_type: RelationType::Calls,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: Some("a.py".into()),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
        let errs = validate_kg_edges(&kg, &schema);
        assert_eq!(errs.len(), 1);
        assert!(errs[0].to_message().contains("module"));
        assert!(errs[0].source_label.as_deref() == Some("auth"));
        assert!(reject_invalid_edges(&kg, &schema).is_err());
    }

    #[test]
    fn non_strict_allows_unknown_relations() {
        let mut schema = EdgeValidationSchema::programmatic(vec![EdgeConstraint::new(
            "calls", "function", "function",
        )]);
        schema.strict_unknown_relations = false;
        assert!(schema.allows("calls", "function", "function"));
        assert!(!schema.allows("calls", "module", "function"));
        // Unknown relation is allowed in non-strict mode.
        assert!(schema.allows("teleports", "function", "module"));
    }

    #[test]
    fn wildcard_relation_constraint() {
        let schema =
            EdgeValidationSchema::programmatic(vec![EdgeConstraint::new("*", "person", "*")]);
        assert!(schema.allows("anything", "person", "location"));
        assert!(!schema.allows("anything", "event", "location"));
    }
}
