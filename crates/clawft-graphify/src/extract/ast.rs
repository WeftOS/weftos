//! Generic LanguageConfig-driven tree-sitter extraction framework.
//!
//! GRAPH-006: Implements the two-pass extraction algorithm:
//!   Pass 1: Walk AST, collect class/function/import entities and containment edges
//!   Pass 2: Build label_to_nid map, walk function bodies for call expressions,
//!           emit INFERRED call edges
//!
//! When `ast-extract` feature is disabled, all functions return empty results.

use std::collections::HashMap;
use std::path::Path;

use crate::GraphifyError;
use crate::entity::{EntityId, EntityType};
use crate::model::{Entity, ExtractionResult};
use crate::relationship::{Confidence, RelationType, Relationship};

use super::lang::LanguageId;

// ── make_id: Python-compatible node ID generation ───────────────────────────

/// Build a stable node ID from one or more name parts.
/// Exact port of Python `_make_id(*parts)`:
///   join with `_`, strip leading/trailing `_.`, replace non-alphanumeric with `_`,
///   strip `_`, lowercase.
pub fn make_id(parts: &[&str]) -> String {
    let combined: String = parts
        .iter()
        .filter(|p| !p.is_empty())
        .map(|p| p.trim_matches(|c: char| c == '_' || c == '.'))
        .collect::<Vec<_>>()
        .join("_");

    let cleaned: String = combined
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { '_' })
        .collect();

    cleaned.trim_matches('_').to_lowercase()
}

// ── LanguageConfig ──────────────────────────────────────────────────────────

/// Configuration for generic AST extraction for a given language.
/// Ports the Python `LanguageConfig` dataclass.
pub struct LanguageConfig {
    /// AST node types representing class/struct/interface declarations.
    pub class_types: &'static [&'static str],
    /// AST node types representing function/method declarations.
    pub function_types: &'static [&'static str],
    /// AST node types representing import/use statements.
    pub import_types: &'static [&'static str],
    /// AST node types representing call expressions.
    pub call_types: &'static [&'static str],

    /// Field name to extract entity name (default: "name").
    pub name_field: &'static str,
    /// Child node types to try if name_field is absent.
    pub name_fallback_child_types: &'static [&'static str],

    /// Field name for body block (default: "body").
    pub body_field: &'static str,
    /// Child types to try for body.
    pub body_fallback_child_types: &'static [&'static str],

    /// Field on call node for the callee (default: "function").
    pub call_function_field: &'static str,
    /// Member/attribute accessor node types.
    pub call_accessor_node_types: &'static [&'static str],
    /// Field on accessor for method name (default: "attribute").
    pub call_accessor_field: &'static str,

    /// Stop recursion in walk_calls at these types.
    pub function_boundary_types: &'static [&'static str],

    /// If true, function labels get "name()" format.
    pub function_label_parens: bool,
}

impl Default for LanguageConfig {
    fn default() -> Self {
        Self {
            class_types: &[],
            function_types: &[],
            import_types: &[],
            call_types: &[],
            name_field: "name",
            name_fallback_child_types: &[],
            body_field: "body",
            body_fallback_child_types: &[],
            call_function_field: "function",
            call_accessor_node_types: &[],
            call_accessor_field: "attribute",
            function_boundary_types: &[],
            function_label_parens: true,
        }
    }
}

// ── Extraction node/edge helpers (used by all extractors) ───────────────────

/// A raw extracted node (before conversion to Entity).
#[derive(Debug, Clone)]
pub struct RawNode {
    pub id: String,
    pub label: String,
    pub file_type: String,
    pub source_file: String,
    pub source_location: String,
}

/// A raw extracted edge (before conversion to Relationship).
#[derive(Debug, Clone)]
pub struct RawEdge {
    pub source: String,
    pub target: String,
    pub relation: String,
    pub confidence: String,
    pub source_file: String,
    pub source_location: String,
    pub weight: f64,
}

/// Raw extraction result before conversion to typed model.
#[derive(Debug, Clone, Default)]
pub struct RawExtractionResult {
    pub nodes: Vec<RawNode>,
    pub edges: Vec<RawEdge>,
}

impl RawExtractionResult {
    /// Convert to typed ExtractionResult.
    pub fn into_extraction_result(self, path: &Path) -> ExtractionResult {
        use crate::entity::FileType;

        let entities: Vec<Entity> = self
            .nodes
            .into_iter()
            .map(|n| {
                let file_type = match n.file_type.as_str() {
                    "code" => FileType::Code,
                    "document" => FileType::Document,
                    "config" => FileType::Config,
                    _ => FileType::Unknown,
                };
                Entity {
                    id: EntityId::from_legacy_string(&n.id),
                    entity_type: EntityType::Concept,
                    label: n.label,
                    iri: None,
                    source_file: Some(n.source_file),
                    source_location: Some(n.source_location),
                    file_type,
                    metadata: serde_json::json!({}),
                    legacy_id: Some(n.id),
                }
            })
            .collect();

        let relationships: Vec<Relationship> = self
            .edges
            .into_iter()
            .map(|e| {
                let relation_type = match e.relation.to_lowercase().as_str() {
                    "contains" => RelationType::Contains,
                    "calls" => RelationType::Calls,
                    "imports" => RelationType::Imports,
                    "extends" => RelationType::Extends,
                    "implements" => RelationType::Implements,
                    "depends_on" => RelationType::DependsOn,
                    "method_of" => RelationType::MethodOf,
                    "references" | "uses" => RelationType::RelatedTo,
                    other => RelationType::Custom(other.to_string()),
                };
                Relationship {
                    source: EntityId::from_legacy_string(&e.source),
                    target: EntityId::from_legacy_string(&e.target),
                    relation_type,
                    confidence: match e.confidence.as_str() {
                        "EXTRACTED" => Confidence::Extracted,
                        "INFERRED" => Confidence::Inferred,
                        _ => Confidence::Ambiguous,
                    },
                    weight: e.weight as f32,
                    source_file: Some(e.source_file),
                    source_location: Some(e.source_location),
                    metadata: serde_json::json!({}),
                    embedding: None,
                }
            })
            .collect();

        ExtractionResult {
            source_file: path.to_string_lossy().to_string(),
            entities,
            relationships,
            hyperedges: Vec::new(),
            input_tokens: 0,
            output_tokens: 0,
            errors: Vec::new(),
        }
    }
}

// ── Extraction context (shared state for walk) ──────────────────────────────

/// Shared mutable state during AST walk.
pub struct ExtractionContext {
    pub stem: String,
    pub str_path: String,
    pub file_nid: String,
    pub nodes: Vec<RawNode>,
    pub edges: Vec<RawEdge>,
    pub seen_ids: std::collections::HashSet<String>,
    pub function_bodies_indices: Vec<(String, usize, usize)>, // (nid, start_byte, end_byte)
}

impl ExtractionContext {
    pub fn new(path: &Path) -> Self {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown")
            .to_string();
        let str_path = path.to_string_lossy().to_string();
        let file_nid = make_id(&[&stem]);
        let file_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("unknown");

        let mut ctx = Self {
            stem,
            str_path,
            file_nid: file_nid.clone(),
            nodes: Vec::new(),
            edges: Vec::new(),
            seen_ids: std::collections::HashSet::new(),
            function_bodies_indices: Vec::new(),
        };

        ctx.add_node(&file_nid, file_name, 1);
        ctx
    }

    pub fn add_node(&mut self, nid: &str, label: &str, line: usize) {
        if self.seen_ids.insert(nid.to_string()) {
            self.nodes.push(RawNode {
                id: nid.to_string(),
                label: label.to_string(),
                file_type: "code".to_string(),
                source_file: self.str_path.clone(),
                source_location: format!("L{line}"),
            });
        }
    }

    pub fn add_node_with_file_type(
        &mut self,
        nid: &str,
        label: &str,
        line: usize,
        file_type: &str,
    ) {
        if self.seen_ids.insert(nid.to_string()) {
            self.nodes.push(RawNode {
                id: nid.to_string(),
                label: label.to_string(),
                file_type: file_type.to_string(),
                source_file: self.str_path.clone(),
                source_location: format!("L{line}"),
            });
        }
    }

    pub fn add_phantom_node(&mut self, nid: &str, label: &str) {
        if self.seen_ids.insert(nid.to_string()) {
            self.nodes.push(RawNode {
                id: nid.to_string(),
                label: label.to_string(),
                file_type: "code".to_string(),
                source_file: String::new(),
                source_location: String::new(),
            });
        }
    }

    pub fn add_edge(
        &mut self,
        src: &str,
        tgt: &str,
        relation: &str,
        line: usize,
        confidence: &str,
        weight: f64,
    ) {
        self.edges.push(RawEdge {
            source: src.to_string(),
            target: tgt.to_string(),
            relation: relation.to_string(),
            confidence: confidence.to_string(),
            source_file: self.str_path.clone(),
            source_location: format!("L{line}"),
            weight,
        });
    }

    pub fn add_extracted_edge(&mut self, src: &str, tgt: &str, relation: &str, line: usize) {
        self.add_edge(src, tgt, relation, line, "EXTRACTED", 1.0);
    }

    /// Clean edges: filter out edges where source is not in seen_ids,
    /// but allow import targets to external modules.
    pub fn clean_edges(&mut self) {
        let valid_ids = &self.seen_ids;
        self.edges.retain(|e| {
            valid_ids.contains(&e.source)
                && (valid_ids.contains(&e.target)
                    || e.relation == "imports"
                    || e.relation == "imports_from")
        });
    }

    /// Build the label_to_nid map for call graph resolution.
    pub fn build_label_map(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        for n in &self.nodes {
            let normalized = n.label.trim_matches(|c: char| c == '(' || c == ')');
            let normalized = normalized.trim_start_matches('.');
            map.insert(normalized.to_lowercase(), n.id.clone());
        }
        map
    }

    /// Emit a call edge if callee resolves and is not self-referential.
    pub fn emit_call_edge(
        &mut self,
        caller_nid: &str,
        callee_name: &str,
        line: usize,
        label_to_nid: &HashMap<String, String>,
        seen_call_pairs: &mut std::collections::HashSet<(String, String)>,
    ) {
        if let Some(tgt_nid) = label_to_nid.get(&callee_name.to_lowercase())
            && tgt_nid != caller_nid
        {
            let pair = (caller_nid.to_string(), tgt_nid.to_string());
            if seen_call_pairs.insert(pair) {
                self.add_edge(caller_nid, tgt_nid, "calls", line, "INFERRED", 0.8);
            }
        }
    }

    /// Convert to RawExtractionResult.
    pub fn into_raw_result(self) -> RawExtractionResult {
        RawExtractionResult {
            nodes: self.nodes,
            edges: self.edges,
        }
    }
}

// ── Main dispatch ───────────────────────────────────────────────────────────

/// Extract entities and relationships from a source file using tree-sitter.
///
/// Dispatches to per-language extractors. When the `ast-extract` feature is
/// disabled, returns an empty result.
#[cfg(feature = "ast-extract")]
pub fn extract_ast(path: &Path, lang: LanguageId) -> Result<ExtractionResult, GraphifyError> {
    match lang {
        LanguageId::Python => super::lang::python::extract_python(path),
        LanguageId::JavaScript | LanguageId::TypeScript => {
            super::lang::javascript::extract_js(path)
        }
        LanguageId::Rust => super::lang::rust_lang::extract_rust(path),
        LanguageId::Go => super::lang::go::extract_go(path),
        // Stubs for languages not yet implemented with tree-sitter
        _ => Ok(ExtractionResult::empty(path)),
    }
}

#[cfg(not(feature = "ast-extract"))]
pub fn extract_ast(path: &Path, _lang: LanguageId) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_id_basic() {
        assert_eq!(make_id(&["auth", "AuthService"]), "auth_authservice");
    }

    #[test]
    fn make_id_strips_dots_and_underscores() {
        assert_eq!(make_id(&["_foo_", ".bar."]), "foo_bar");
    }

    #[test]
    fn make_id_non_alphanumeric_replaced() {
        assert_eq!(make_id(&["my-func", "a.b"]), "my_func_a_b");
    }

    #[test]
    fn make_id_empty_parts_filtered() {
        assert_eq!(make_id(&["", "hello", ""]), "hello");
    }

    #[test]
    fn make_id_all_lowercase() {
        assert_eq!(make_id(&["MyClass"]), "myclass");
    }

    #[test]
    fn extraction_context_basics() {
        let ctx = ExtractionContext::new(Path::new("/tmp/test.py"));
        assert_eq!(ctx.stem, "test");
        assert_eq!(ctx.file_nid, "test");
        assert_eq!(ctx.nodes.len(), 1); // file node
        assert_eq!(ctx.nodes[0].label, "test.py");
    }

    #[test]
    fn extraction_context_dedup_nodes() {
        let mut ctx = ExtractionContext::new(Path::new("/tmp/test.py"));
        ctx.add_node("foo", "Foo", 1);
        ctx.add_node("foo", "Foo", 2); // duplicate
        assert_eq!(ctx.nodes.len(), 2); // file + foo
    }

    #[test]
    fn build_label_map_normalizes() {
        let mut ctx = ExtractionContext::new(Path::new("/tmp/test.py"));
        ctx.add_node("test_foo", "foo()", 2);
        ctx.add_node("test_bar", ".Bar()", 3);
        let map = ctx.build_label_map();
        assert_eq!(map.get("foo"), Some(&"test_foo".to_string()));
        assert_eq!(map.get("bar"), Some(&"test_bar".to_string()));
    }

    #[test]
    fn clean_edges_filters_dangling() {
        let mut ctx = ExtractionContext::new(Path::new("/tmp/test.py"));
        ctx.add_node("a", "A", 1);
        // Edge to known node -- kept
        ctx.add_extracted_edge("test", "a", "contains", 1);
        // Edge to unknown node -- removed (not import)
        ctx.add_extracted_edge("test", "unknown", "calls", 2);
        // Import to external -- kept
        ctx.add_extracted_edge("test", "external", "imports", 3);

        ctx.clean_edges();
        assert_eq!(ctx.edges.len(), 2);
    }

    #[test]
    fn raw_result_to_extraction_result() {
        let mut ctx = ExtractionContext::new(Path::new("/tmp/test.py"));
        ctx.add_node("test_foo", "foo()", 2);
        ctx.add_extracted_edge("test", "test_foo", "contains", 2);

        let raw = ctx.into_raw_result();
        let result = raw.into_extraction_result(Path::new("/tmp/test.py"));
        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.relationships.len(), 1);
    }
}
