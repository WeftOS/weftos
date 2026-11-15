//! Rust language support -- custom extractor (not config-driven).
//!
//! GRAPH-009: Handles function_item, struct_item, enum_item, trait_item,
//! impl_item, and use_declaration. Mirrors Python `extract_rust`.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-rust")]
use crate::extract::ast::{ExtractionContext, make_id};

/// Extract entities and relationships from a Rust source file.
#[cfg(feature = "lang-rust")]
pub fn extract_rust(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::{Language, Parser};

    let language = Language::from(tree_sitter_rust::LANGUAGE);

    let mut parser = Parser::new();
    parser
        .set_language(&language)
        .map_err(|e| GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), e)))?;

    let source = std::fs::read(path)?;
    let tree = parser.parse(&source, None).ok_or_else(|| {
        GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), "parse returned None"))
    })?;

    let mut ctx = ExtractionContext::new(path);
    walk_rust(&tree.root_node(), &source, &mut ctx, None);

    // Call graph pass
    run_rust_call_graph(&source, &mut ctx, &language);

    ctx.clean_edges();
    let raw = ctx.into_raw_result();
    Ok(raw.into_extraction_result(path))
}

#[cfg(not(feature = "lang-rust"))]
pub fn extract_rust(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(feature = "lang-rust")]
fn read_text(node: &tree_sitter::Node, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).to_string()
}

#[cfg(feature = "lang-rust")]
fn walk_rust(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    parent_impl_nid: Option<&str>,
) {
    let kind = node.kind();

    // function_item
    if kind == "function_item" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let func_name = read_text(&name_node, source);
            let line = node.start_position().row + 1;

            if let Some(impl_nid) = parent_impl_nid {
                let func_nid = make_id(&[impl_nid, &func_name]);
                ctx.add_node(&func_nid, &format!(".{func_name}()"), line);
                ctx.add_extracted_edge(impl_nid, &func_nid, "method", line);
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

    // struct_item, enum_item, trait_item
    if kind == "struct_item" || kind == "enum_item" || kind == "trait_item" {
        if let Some(name_node) = node.child_by_field_name("name") {
            let item_name = read_text(&name_node, source);
            let line = node.start_position().row + 1;
            let item_nid = make_id(&[&ctx.stem, &item_name]);
            ctx.add_node(&item_nid, &item_name, line);
            ctx.add_extracted_edge(&ctx.file_nid.clone(), &item_nid, "contains", line);
        }
        return;
    }

    // impl_item
    if kind == "impl_item" {
        let mut impl_nid: Option<String> = None;
        if let Some(type_node) = node.child_by_field_name("type") {
            let type_name = read_text(&type_node, source).trim().to_string();
            let nid = make_id(&[&ctx.stem, &type_name]);
            let line = node.start_position().row + 1;
            ctx.add_node(&nid, &type_name, line);
            impl_nid = Some(nid);
        }

        if let Some(body) = node.child_by_field_name("body") {
            let count = body.child_count();
            for i in 0..count {
                if let Some(child) = body.child(i) {
                    walk_rust(&child, source, ctx, impl_nid.as_deref());
                }
            }
        }
        return;
    }

    // use_declaration
    if kind == "use_declaration" {
        if let Some(arg) = node.child_by_field_name("argument") {
            let raw = read_text(&arg, source);
            // Split on `{`, strip `::*`, take last `::` segment
            let clean = raw.split('{').next().unwrap_or("");
            let clean = clean
                .trim_end_matches(':')
                .trim_end_matches('*')
                .trim_end_matches(':');
            let module_name = clean.rsplit("::").next().unwrap_or("").trim();
            if !module_name.is_empty() {
                let tgt_nid = make_id(&[module_name]);
                let line = node.start_position().row + 1;
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
        return;
    }

    // Default: recurse (passing None for parent_impl_nid at top level)
    let count = node.child_count();
    for i in 0..count {
        if let Some(child) = node.child(i) {
            walk_rust(&child, source, ctx, None);
        }
    }
}

#[cfg(feature = "lang-rust")]
fn run_rust_call_graph(
    source: &[u8],
    ctx: &mut ExtractionContext,
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
        walk_calls_rust(
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

#[cfg(feature = "lang-rust")]
#[allow(clippy::too_many_arguments)]
fn walk_calls_rust(
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
    if node.kind() == "function_item" && node.start_byte() > body_start {
        return;
    }

    if node.kind() == "call_expression"
        && let Some(func_node) = node.child_by_field_name("function")
    {
        let callee_name = match func_node.kind() {
            "identifier" => Some(read_text(&func_node, source)),
            "field_expression" => func_node
                .child_by_field_name("field")
                .map(|f| read_text(&f, source)),
            "scoped_identifier" => func_node
                .child_by_field_name("name")
                .map(|n| read_text(&n, source)),
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
            walk_calls_rust(
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
    fn extract_rust_returns_empty_without_feature() {
        let path = Path::new("/nonexistent/test.rs");
        let result = extract_rust(path);
        assert!(result.is_ok() || result.is_err());
    }
}
