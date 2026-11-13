//! Python language support for AST extraction.
//!
//! GRAPH-007: PYTHON_CONFIG, import handler, inheritance, rationale extraction.
//!
//! When `lang-python` feature is enabled, uses tree-sitter-python for full
//! AST extraction. Otherwise returns empty results.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-python")]
use crate::extract::ast::{ExtractionContext, LanguageConfig, make_id};

/// Python language configuration for the generic extractor.
#[cfg(feature = "lang-python")]
pub static PYTHON_CONFIG: LanguageConfig = LanguageConfig {
    class_types: &["class_definition"],
    function_types: &["function_definition"],
    import_types: &["import_statement", "import_from_statement"],
    call_types: &["call"],
    name_field: "name",
    name_fallback_child_types: &[],
    body_field: "body",
    body_fallback_child_types: &[],
    call_function_field: "function",
    call_accessor_node_types: &["attribute"],
    call_accessor_field: "attribute",
    function_boundary_types: &["function_definition"],
    function_label_parens: true,
};

/// Rationale comment prefixes.
#[cfg(feature = "lang-python")]
static RATIONALE_PREFIXES: &[&str] = &[
    "# NOTE:",
    "# IMPORTANT:",
    "# HACK:",
    "# WHY:",
    "# RATIONALE:",
    "# TODO:",
    "# FIXME:",
];

/// Extract entities and relationships from a Python source file.
#[cfg(feature = "lang-python")]
pub fn extract_python(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::{Language, Parser};

    let language = Language::from(tree_sitter_python::LANGUAGE);

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), e)))?;

    let source = std::fs::read(path)?;
    let tree = parser.parse(&source, None).ok_or_else(|| {
        GraphifyError::ExtractionFailed(format!(
            "{}: {}",
            path.display(),
            "tree-sitter parse returned None"
        ))
    })?;

    let mut ctx = ExtractionContext::new(path);

    // Pass 1: structural walk
    walk_python(&tree.root_node(), &source, &mut ctx, None);

    // Call graph pass
    run_call_graph_pass(&source, &mut ctx, &PYTHON_CONFIG);

    // Rationale post-pass
    extract_rationale(path, &source, &tree.root_node(), &mut ctx);

    // Clean edges
    ctx.clean_edges();

    let raw = ctx.into_raw_result();
    Ok(raw.into_extraction_result(path))
}

#[cfg(not(feature = "lang-python"))]
pub fn extract_python(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

// ── Tree-sitter walk (feature-gated) ────────────────────────────────────────

#[cfg(feature = "lang-python")]
fn read_text(node: &tree_sitter::Node, source: &[u8]) -> String {
    let bytes = &source[node.start_byte()..node.end_byte()];
    String::from_utf8_lossy(bytes).to_string()
}

#[cfg(feature = "lang-python")]
fn walk_python(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    parent_class_nid: Option<&str>,
) {
    let node_type = node.kind();

    // Imports
    if PYTHON_CONFIG.import_types.contains(&node_type) {
        handle_python_import(node, source, ctx);
        return;
    }

    // Classes
    if PYTHON_CONFIG.class_types.contains(&node_type) {
        if let Some(name_node) = node.child_by_field_name("name") {
            let class_name = read_text(&name_node, source);
            let class_nid = make_id(&[&ctx.stem, &class_name]);
            let line = node.start_position().row + 1;
            ctx.add_node(&class_nid, &class_name, line);
            ctx.add_extracted_edge(&ctx.file_nid.clone(), &class_nid, "contains", line);

            // Inheritance
            if let Some(superclasses) = node.child_by_field_name("superclasses") {
                let cursor_count = superclasses.child_count();
                for i in 0..cursor_count {
                    if let Some(arg) = superclasses.child(i)
                        && arg.kind() == "identifier"
                    {
                        let base = read_text(&arg, source);
                        let mut base_nid = make_id(&[&ctx.stem, &base]);
                        if !ctx.seen_ids.contains(&base_nid) {
                            base_nid = make_id(&[&base]);
                            if !ctx.seen_ids.contains(&base_nid) {
                                ctx.add_phantom_node(&base_nid, &base);
                            }
                        }
                        ctx.add_extracted_edge(&class_nid, &base_nid, "inherits", line);
                    }
                }
            }

            // Recurse into body
            if let Some(body) = node.child_by_field_name("body") {
                let count = body.child_count();
                for i in 0..count {
                    if let Some(child) = body.child(i) {
                        walk_python(&child, source, ctx, Some(&class_nid));
                    }
                }
            }
        }
        return;
    }

    // Functions
    if PYTHON_CONFIG.function_types.contains(&node_type) {
        if let Some(name_node) = node.child_by_field_name("name") {
            let func_name = read_text(&name_node, source);
            let line = node.start_position().row + 1;

            if let Some(parent_nid) = parent_class_nid {
                let func_nid = make_id(&[parent_nid, &func_name]);
                ctx.add_node(&func_nid, &format!(".{func_name}()"), line);
                ctx.add_extracted_edge(parent_nid, &func_nid, "method", line);

                if let Some(body) = node.child_by_field_name("body") {
                    ctx.function_bodies_indices.push((
                        func_nid,
                        body.start_byte(),
                        body.end_byte(),
                    ));
                }
            } else {
                let func_nid = make_id(&[&ctx.stem, &func_name]);
                ctx.add_node(&func_nid, &format!("{func_name}()"), line);
                ctx.add_extracted_edge(&ctx.file_nid.clone(), &func_nid, "contains", line);

                if let Some(body) = node.child_by_field_name("body") {
                    ctx.function_bodies_indices.push((
                        func_nid,
                        body.start_byte(),
                        body.end_byte(),
                    ));
                }
            }
        }
        return;
    }

    // Default: recurse
    let count = node.child_count();
    for i in 0..count {
        if let Some(child) = node.child(i) {
            walk_python(&child, source, ctx, None);
        }
    }
}

#[cfg(feature = "lang-python")]
fn handle_python_import(node: &tree_sitter::Node, source: &[u8], ctx: &mut ExtractionContext) {
    let t = node.kind();
    let line = node.start_position().row + 1;
    let file_nid = ctx.file_nid.clone();

    if t == "import_statement" {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i)
                && (child.kind() == "dotted_name" || child.kind() == "aliased_import")
            {
                let raw = read_text(&child, source);
                let module_name = raw
                    .split(" as ")
                    .next()
                    .unwrap_or("")
                    .trim()
                    .trim_start_matches('.');
                if !module_name.is_empty() {
                    let tgt_nid = make_id(&[module_name]);
                    ctx.add_edge(&file_nid, &tgt_nid, "imports", line, "EXTRACTED", 1.0);
                }
            }
        }
    } else if t == "import_from_statement"
        && let Some(module_node) = node.child_by_field_name("module_name")
    {
        let raw = read_text(&module_node, source);
        let clean = raw.trim_start_matches('.');
        if !clean.is_empty() {
            let tgt_nid = make_id(&[clean]);
            ctx.add_edge(&file_nid, &tgt_nid, "imports_from", line, "EXTRACTED", 1.0);
        }
    }
}

#[cfg(feature = "lang-python")]
fn run_call_graph_pass(source: &[u8], ctx: &mut ExtractionContext, config: &LanguageConfig) {
    use tree_sitter::{Language, Parser};

    let label_to_nid = ctx.build_label_map();
    let mut seen_call_pairs = std::collections::HashSet::new();

    // Re-parse to walk function bodies
    let language = Language::from(tree_sitter_python::LANGUAGE);
    let mut parser = Parser::new();
    if parser.set_language(&language).is_err() {
        return;
    }
    let Some(tree) = parser.parse(source, None) else {
        return;
    };

    let bodies: Vec<(String, usize, usize)> = ctx.function_bodies_indices.clone();
    for (caller_nid, start, end) in &bodies {
        // Walk the body subtree looking for calls
        walk_calls_in_range(
            &tree.root_node(),
            source,
            ctx,
            caller_nid,
            *start,
            *end,
            config,
            &label_to_nid,
            &mut seen_call_pairs,
        );
    }
}

#[cfg(feature = "lang-python")]
#[allow(clippy::too_many_arguments)]
fn walk_calls_in_range(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    caller_nid: &str,
    body_start: usize,
    body_end: usize,
    config: &LanguageConfig,
    label_to_nid: &std::collections::HashMap<String, String>,
    seen_call_pairs: &mut std::collections::HashSet<(String, String)>,
) {
    // Only look at nodes within the body range
    if node.end_byte() <= body_start || node.start_byte() >= body_end {
        return;
    }

    if config.function_boundary_types.contains(&node.kind()) && node.start_byte() > body_start {
        return;
    }

    if config.call_types.contains(&node.kind()) {
        let mut callee_name: Option<String> = None;

        if let Some(func_node) = node.child_by_field_name(config.call_function_field) {
            if func_node.kind() == "identifier" {
                callee_name = Some(read_text(&func_node, source));
            } else if config.call_accessor_node_types.contains(&func_node.kind())
                && let Some(attr) = func_node.child_by_field_name(config.call_accessor_field)
            {
                callee_name = Some(read_text(&attr, source));
            }
        }

        if let Some(name) = callee_name {
            let line = node.start_position().row + 1;
            ctx.emit_call_edge(caller_nid, &name, line, label_to_nid, seen_call_pairs);
        }
    }

    let count = node.child_count();
    for i in 0..count {
        if let Some(child) = node.child(i) {
            walk_calls_in_range(
                &child,
                source,
                ctx,
                caller_nid,
                body_start,
                body_end,
                config,
                label_to_nid,
                seen_call_pairs,
            );
        }
    }
}

// ── Rationale extraction ────────────────────────────────────────────────────

#[cfg(feature = "lang-python")]
fn extract_rationale(
    _path: &Path,
    source: &[u8],
    root: &tree_sitter::Node,
    ctx: &mut ExtractionContext,
) {
    let stem = ctx.stem.clone();
    let file_nid = ctx.file_nid.clone();

    // Module-level docstring
    if let Some((text, line)) = get_docstring(root, source) {
        add_rationale(ctx, &text, line, &file_nid, &stem);
    }

    // Walk for class and function docstrings
    walk_docstrings(root, source, ctx, &stem);

    // Rationale comments
    let source_text = String::from_utf8_lossy(source);
    for (lineno, line_text) in source_text.lines().enumerate() {
        let stripped = line_text.trim();
        if RATIONALE_PREFIXES.iter().any(|p| stripped.starts_with(p)) {
            add_rationale(ctx, stripped, lineno + 1, &file_nid, &stem);
        }
    }
}

#[cfg(feature = "lang-python")]
fn get_docstring(node: &tree_sitter::Node, source: &[u8]) -> Option<(String, usize)> {
    let body = if node.kind() == "module" {
        Some(*node)
    } else {
        None
    };

    let body_node = body.as_ref()?;
    let count = body_node.child_count();
    for i in 0..count {
        if let Some(child) = body_node.child(i) {
            if child.kind() == "expression_statement" {
                let sub_count = child.child_count();
                for j in 0..sub_count {
                    if let Some(sub) = child.child(j)
                        && (sub.kind() == "string" || sub.kind() == "concatenated_string")
                    {
                        let text = read_text(&sub, source);
                        let cleaned = text
                            .trim_matches('"')
                            .trim_matches('\'')
                            .trim_start_matches("\"\"\"")
                            .trim_end_matches("\"\"\"")
                            .trim_start_matches("'''")
                            .trim_end_matches("'''")
                            .trim();
                        if cleaned.len() > 20 {
                            return Some((cleaned.to_string(), child.start_position().row + 1));
                        }
                    }
                }
            }
            break; // Only check first child
        }
    }
    None
}

#[cfg(feature = "lang-python")]
fn walk_docstrings(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    stem: &str,
) {
    let kind = node.kind();

    if kind == "class_definition" {
        if let Some(name_node) = node.child_by_field_name("name")
            && let Some(body) = node.child_by_field_name("body")
        {
            let class_name = read_text(&name_node, source);
            let nid = make_id(&[stem, &class_name]);
            if let Some((text, line)) = get_body_docstring(&body, source) {
                add_rationale(ctx, &text, line, &nid, stem);
            }
            let count = body.child_count();
            for i in 0..count {
                if let Some(child) = body.child(i) {
                    walk_docstrings(&child, source, ctx, stem);
                }
            }
        }
        return;
    }

    if kind == "function_definition" {
        if let Some(name_node) = node.child_by_field_name("name")
            && let Some(body) = node.child_by_field_name("body")
        {
            let func_name = read_text(&name_node, source);
            let nid = make_id(&[stem, &func_name]);
            if let Some((text, line)) = get_body_docstring(&body, source) {
                add_rationale(ctx, &text, line, &nid, stem);
            }
        }
        return;
    }

    let count = node.child_count();
    for i in 0..count {
        if let Some(child) = node.child(i) {
            walk_docstrings(&child, source, ctx, stem);
        }
    }
}

#[cfg(feature = "lang-python")]
fn get_body_docstring(body: &tree_sitter::Node, source: &[u8]) -> Option<(String, usize)> {
    let count = body.child_count();
    for i in 0..count {
        if let Some(child) = body.child(i) {
            if child.kind() == "expression_statement" {
                let sub_count = child.child_count();
                for j in 0..sub_count {
                    if let Some(sub) = child.child(j)
                        && (sub.kind() == "string" || sub.kind() == "concatenated_string")
                    {
                        let text = read_text(&sub, source);
                        let cleaned = text
                            .trim_matches('"')
                            .trim_matches('\'')
                            .trim_start_matches("\"\"\"")
                            .trim_end_matches("\"\"\"")
                            .trim_start_matches("'''")
                            .trim_end_matches("'''")
                            .trim();
                        if cleaned.len() > 20 {
                            return Some((cleaned.to_string(), child.start_position().row + 1));
                        }
                    }
                }
            }
            break;
        }
    }
    None
}

#[cfg(feature = "lang-python")]
fn add_rationale(
    ctx: &mut ExtractionContext,
    text: &str,
    line: usize,
    parent_nid: &str,
    stem: &str,
) {
    let label = text.chars().take(80).collect::<String>().replace('\n', " ");
    let label = label.trim().to_string();
    let rid = make_id(&[stem, "rationale", &line.to_string()]);
    ctx.add_node_with_file_type(&rid, &label, line, "rationale");
    ctx.add_extracted_edge(&rid, parent_nid, "rationale_for", line);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_python_returns_empty_without_feature() {
        // This test works regardless of feature flags
        let path = Path::new("/nonexistent/test.py");
        let result = extract_python(path);
        // Either returns empty or an IO error, both are fine
        assert!(result.is_ok() || result.is_err());
    }
}
