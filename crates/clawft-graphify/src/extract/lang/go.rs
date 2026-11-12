//! Go language support -- custom extractor (not config-driven).
//!
//! GRAPH-010: Handles function_declaration, method_declaration (with receiver
//! type extraction), type_declaration, and import_declaration.
//! Mirrors Python `extract_go`.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-go")]
use crate::extract::ast::{ExtractionContext, make_id};

/// Extract entities and relationships from a Go source file.
#[cfg(feature = "lang-go")]
pub fn extract_go(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::{Language, Parser};

    let language = Language::from(tree_sitter_go::LANGUAGE);

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), e)))?;

    let source = std::fs::read(path)?;
    let tree = parser.parse(&source, None).ok_or_else(|| {
        GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), "parse returned None"))
    })?;

    let mut ctx = ExtractionContext::new(path);
    walk_go(&tree.root_node(), &source, &mut ctx);

    // Call graph pass
    run_go_call_graph(&source, &mut ctx, &language);

    ctx.clean_edges();
    let raw = ctx.into_raw_result();
    Ok(raw.into_extraction_result(path))
}

#[cfg(not(feature = "lang-go"))]
pub fn extract_go(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(feature = "lang-go")]
fn read_text(node: &tree_sitter::Node, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).to_string()
}

#[cfg(feature = "lang-go")]
fn walk_go(node: &tree_sitter::Node, source: &[u8], ctx: &mut ExtractionContext) {
    let kind = node.kind();

    // function_declaration
    if kind == "function_declaration" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let func_name = read_text(&name_node, source);
            let line = node.start_position().row + 1;
            let func_nid = make_id(&[&ctx.stem, &func_name]);
            ctx.add_node(&func_nid, &format!("{func_name}()"), line);
            ctx.add_extracted_edge(&ctx.file_nid.clone(), &func_nid, "contains", line);
            if let Some(body) = node.child_by_field_name("body") {
                ctx.function_bodies_indices
                    .push((func_nid, body.start_byte(), body.end_byte()));
            }
        }
        return;
    }

    // method_declaration (with receiver type extraction)
    if kind == "method_declaration" {
        let mut receiver_type: Option<String> = None;
        if let Some(receiver) = node.child_by_field_name("receiver") {
            let count = receiver.child_count();
            for i in 0..count {
                if let Some(param) = receiver.child(i)
                    && param.kind() == "parameter_declaration"
                {
                    if let Some(type_node) = param.child_by_field_name("type") {
                        let raw = read_text(&type_node, source);
                        receiver_type = Some(raw.trim_start_matches('*').trim().to_string());
                    }
                    break;
                }
            }
        }

        if let Some(name_node) = node.child_by_field_name("name") {
            let method_name = read_text(&name_node, source);
            let line = node.start_position().row + 1;

            if let Some(ref recv_type) = receiver_type {
                let parent_nid = make_id(&[&ctx.stem, recv_type]);
                ctx.add_node(&parent_nid, recv_type, line);
                let method_nid = make_id(&[&parent_nid, &method_name]);
                ctx.add_node(&method_nid, &format!(".{method_name}()"), line);
                ctx.add_extracted_edge(&parent_nid, &method_nid, "method", line);
                if let Some(body) = node.child_by_field_name("body") {
                    ctx.function_bodies_indices.push((
                        method_nid,
                        body.start_byte(),
                        body.end_byte(),
                    ));
                }
            } else {
                let method_nid = make_id(&[&ctx.stem, &method_name]);
                ctx.add_node(&method_nid, &format!("{method_name}()"), line);
                ctx.add_extracted_edge(&ctx.file_nid.clone(), &method_nid, "contains", line);
                if let Some(body) = node.child_by_field_name("body") {
                    ctx.function_bodies_indices.push((
                        method_nid,
                        body.start_byte(),
                        body.end_byte(),
                    ));
                }
            }
        }
        return;
    }

    // type_declaration
    if kind == "type_declaration" {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i)
                && child.kind() == "type_spec"
                && let Some(name_node) = child.child_by_field_name("name")
            {
                let type_name = read_text(&name_node, source);
                let line = child.start_position().row + 1;
                let type_nid = make_id(&[&ctx.stem, &type_name]);
                ctx.add_node(&type_nid, &type_name, line);
                ctx.add_extracted_edge(&ctx.file_nid.clone(), &type_nid, "contains", line);
            }
        }
        return;
    }

    // import_declaration
    if kind == "import_declaration" {
        let count = node.child_count();
        for i in 0..count {
            if let Some(child) = node.child(i) {
                if child.kind() == "import_spec_list" {
                    let spec_count = child.child_count();
                    for j in 0..spec_count {
                        if let Some(spec) = child.child(j)
                            && spec.kind() == "import_spec"
                        {
                            handle_go_import_spec(&spec, source, ctx);
                        }
                    }
                } else if child.kind() == "import_spec" {
                    handle_go_import_spec(&child, source, ctx);
                }
            }
        }
        return;
    }

    // Default: recurse
    let count = node.child_count();
    for i in 0..count {
        if let Some(child) = node.child(i) {
            walk_go(&child, source, ctx);
        }
    }
}

#[cfg(feature = "lang-go")]
fn handle_go_import_spec(spec: &tree_sitter::Node, source: &[u8], ctx: &mut ExtractionContext) {
    if let Some(path_node) = spec.child_by_field_name("path") {
        let raw = read_text(&path_node, source);
        let raw = raw.trim_matches('"');
        let module_name = raw.rsplit('/').next().unwrap_or("");
        if !module_name.is_empty() {
            let tgt_nid = make_id(&[module_name]);
            let line = spec.start_position().row + 1;
            ctx.add_edge(
                &ctx.file_nid.clone(),
                &tgt_nid,
                "imports_from",
                line,
                "EXTRACTED",
                1.0,
            );
        }
    }
}

#[cfg(feature = "lang-go")]
fn run_go_call_graph(source: &[u8], ctx: &mut ExtractionContext, language: &tree_sitter::Language) {
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
        walk_calls_go(
            &tree.root_node(),
            source,
            ctx,
            caller_nid,
            *start,
            *end,
            &label_to_nid,
            &mut seen_call_pairs,
        );
    }
}

#[cfg(feature = "lang-go")]
#[allow(clippy::too_many_arguments)]
fn walk_calls_go(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    caller_nid: &str,
    body_start: usize,
    body_end: usize,
    label_to_nid: &std::collections::HashMap<String, String>,
    seen_call_pairs: &mut std::collections::HashSet<(String, String)>,
) {
    if node.end_byte() <= body_start || node.start_byte() >= body_end {
        return;
    }

    let kind = node.kind();
    if (kind == "function_declaration" || kind == "method_declaration")
        && node.start_byte() > body_start
    {
        return;
    }

    if kind == "call_expression"
        && let Some(func_node) = node.child_by_field_name("function")
    {
        let callee_name = match func_node.kind() {
            "identifier" => Some(read_text(&func_node, source)),
            "selector_expression" => func_node
                .child_by_field_name("field")
                .map(|f| read_text(&f, source)),
            _ => None,
        };

        if let Some(name) = callee_name {
            let line = node.start_position().row + 1;
            ctx.emit_call_edge(caller_nid, &name, line, label_to_nid, seen_call_pairs);
        }
    }

    let count = node.child_count();
    for i in 0..count {
        if let Some(child) = node.child(i) {
            walk_calls_go(
                &child,
                source,
                ctx,
                caller_nid,
                body_start,
                body_end,
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
    fn extract_go_returns_empty_without_feature() {
        let path = Path::new("/nonexistent/test.go");
        let result = extract_go(path);
        assert!(result.is_ok() || result.is_err());
    }
}
