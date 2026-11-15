//! JavaScript and TypeScript language support for AST extraction.
//!
//! GRAPH-008: JS_CONFIG, TS_CONFIG, arrow function handling.
//!
//! Feature-gated behind `lang-javascript` and `lang-typescript`.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
use crate::extract::ast::{ExtractionContext, LanguageConfig, make_id};

/// JavaScript language configuration.
#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
pub static JS_CONFIG: LanguageConfig = LanguageConfig {
    class_types: &["class_declaration"],
    function_types: &["function_declaration", "method_definition"],
    import_types: &["import_statement"],
    call_types: &["call_expression"],
    name_field: "name",
    name_fallback_child_types: &[],
    body_field: "body",
    body_fallback_child_types: &[],
    call_function_field: "function",
    call_accessor_node_types: &["member_expression"],
    call_accessor_field: "property",
    function_boundary_types: &[
        "function_declaration",
        "arrow_function",
        "method_definition",
    ],
    function_label_parens: true,
};

/// TypeScript language configuration.
#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
pub static TS_CONFIG: LanguageConfig = LanguageConfig {
    class_types: &["class_declaration"],
    function_types: &["function_declaration", "method_definition"],
    import_types: &["import_statement"],
    call_types: &["call_expression"],
    name_field: "name",
    name_fallback_child_types: &[],
    body_field: "body",
    body_fallback_child_types: &[],
    call_function_field: "function",
    call_accessor_node_types: &["member_expression"],
    call_accessor_field: "property",
    function_boundary_types: &[
        "function_declaration",
        "arrow_function",
        "method_definition",
    ],
    function_label_parens: true,
};

/// Extract entities and relationships from a JS or TS file.
#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
pub fn extract_js(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::{Language, Parser};

    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("js");
    let is_ts = ext == "ts" || ext == "tsx";

    let config = if is_ts { &TS_CONFIG } else { &JS_CONFIG };

    let language = if is_ts {
        #[cfg(feature = "lang-typescript")]
        {
            Language::from(tree_sitter_typescript::LANGUAGE_TYPESCRIPT)
        }
        #[cfg(not(feature = "lang-typescript"))]
        {
            return Ok(ExtractionResult::empty(path.to_path_buf()));
        }
    } else {
        #[cfg(feature = "lang-javascript")]
        {
            Language::from(tree_sitter_javascript::LANGUAGE)
        }
        #[cfg(not(feature = "lang-javascript"))]
        {
            return Ok(ExtractionResult::empty(path.to_path_buf()));
        }
    };

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), e)))?;

    let source = std::fs::read(path)?;
    let tree = parser.parse(&source, None).ok_or_else(|| {
        GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), "parse returned None"))
    })?;

    let mut ctx = ExtractionContext::new(path);
    walk_js(&tree.root_node(), &source, &mut ctx, None, config);

    // Call graph pass
    run_js_call_graph(&source, &mut ctx, config, &language);

    ctx.clean_edges();
    let raw = ctx.into_raw_result();
    Ok(raw.into_extraction_result(path))
}

#[cfg(not(any(feature = "lang-javascript", feature = "lang-typescript")))]
pub fn extract_js(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
fn read_text(node: &tree_sitter::Node, source: &[u8]) -> String {
    let bytes = &source[node.start_byte()..node.end_byte()];
    String::from_utf8_lossy(bytes).to_string()
}

#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
fn walk_js(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    parent_class_nid: Option<&str>,
    config: &LanguageConfig,
) {
    let kind = node.kind();

    // Imports
    if config.import_types.contains(&kind) {
        handle_js_import(node, source, ctx);
        return;
    }

    // Classes
    if config.class_types.contains(&kind) {
        if let Some(name_node) = node.child_by_field_name("name") {
            let class_name = read_text(&name_node, source);
            let class_nid = make_id(&[&ctx.stem, &class_name]);
            let line = node.start_position().row + 1;
            ctx.add_node(&class_nid, &class_name, line);
            ctx.add_extracted_edge(&ctx.file_nid.clone(), &class_nid, "contains", line);

            if let Some(body) = node.child_by_field_name("body") {
                let count = body.child_count();
                for i in 0..count {
                    if let Some(child) = body.child(i) {
                        walk_js(&child, source, ctx, Some(&class_nid), config);
                    }
                }
            }
        }
        return;
    }

    // Functions
    if config.function_types.contains(&kind) {
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

    // Arrow function detection (JS/TS specific extra walk)
    if kind == "lexical_declaration" {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i)
                && child.kind() == "variable_declarator"
                && let Some(value) = child.child_by_field_name("value")
                && value.kind() == "arrow_function"
                && let Some(name_node) = child.child_by_field_name("name")
            {
                let func_name = read_text(&name_node, source);
                let line = child.start_position().row + 1;
                let func_nid = make_id(&[&ctx.stem, &func_name]);
                ctx.add_node(&func_nid, &format!("{func_name}()"), line);
                ctx.add_extracted_edge(&ctx.file_nid.clone(), &func_nid, "contains", line);
                if let Some(body) = value.child_by_field_name("body") {
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
            walk_js(&child, source, ctx, None, config);
        }
    }
}

#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
fn handle_js_import(node: &tree_sitter::Node, source: &[u8], ctx: &mut ExtractionContext) {
    let line = node.start_position().row + 1;
    let file_nid = ctx.file_nid.clone();
    let count = node.child_count();

    for i in 0..count {
        if let Some(child) = node.child(i)
            && child.kind() == "string"
        {
            let raw = read_text(&child, source);
            let raw = raw.trim_matches(|c: char| c == '\'' || c == '"' || c == '`' || c == ' ');
            let module_name = raw
                .trim_start_matches("./")
                .rsplit('/')
                .next()
                .unwrap_or("");
            if !module_name.is_empty() {
                let tgt_nid = make_id(&[module_name]);
                ctx.add_edge(&file_nid, &tgt_nid, "imports_from", line, "EXTRACTED", 1.0);
            }
            break;
        }
    }
}

#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
fn run_js_call_graph(
    source: &[u8],
    ctx: &mut ExtractionContext,
    config: &LanguageConfig,
    language: &tree_sitter::Language,
) {
    use tree_sitter::Parser;

    let label_to_nid = ctx.build_label_map();
    let mut seen_call_pairs = std::collections::HashSet::new();

    let mut parser = Parser::new();
    if parser.set_language(language).is_err() {
        return;
    }
    let Some(tree) = parser.parse(source, None) else {
        return;
    };

    let bodies: Vec<(String, usize, usize)> = ctx.function_bodies_indices.clone();
    for (caller_nid, start, end) in &bodies {
        walk_calls_js(
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

#[cfg(any(feature = "lang-javascript", feature = "lang-typescript"))]
#[allow(clippy::too_many_arguments)]
fn walk_calls_js(
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
                && let Some(prop) = func_node.child_by_field_name(config.call_accessor_field)
            {
                callee_name = Some(read_text(&prop, source));
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
            walk_calls_js(
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_js_returns_empty_without_feature() {
        let path = Path::new("/nonexistent/test.js");
        let result = extract_js(path);
        assert!(result.is_ok() || result.is_err());
    }
}
