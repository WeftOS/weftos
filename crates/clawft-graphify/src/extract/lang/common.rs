//! Helpers shared by the Java, C, C++, Ruby and C# extractors.
//!
//! Every function here is only reachable from a `lang-*` feature, so with a
//! subset of features some of them are unused; hence the `dead_code` allow.
#![allow(dead_code)]

use std::path::Path;

use tree_sitter::{Language, Node, Parser, Tree};

use crate::GraphifyError;
use crate::extract::ast::{ExtractionContext, make_id};
use crate::model::ExtractionResult;

/// A pending `class -> base` edge: (class nid, base name, relation, line).
/// Resolved after the walk so a base declared later in the file still binds
/// to its real node instead of a phantom.
pub(crate) type PendingInherit = (String, String, &'static str, usize);

/// Source text of a node.
pub(crate) fn text(node: &Node, source: &[u8]) -> String {
    String::from_utf8_lossy(&source[node.start_byte()..node.end_byte()]).to_string()
}

/// Read and parse `path` with `language`.
pub(crate) fn parse(path: &Path, language: &Language) -> Result<(Vec<u8>, Tree), GraphifyError> {
    let mut parser = Parser::new();
    parser
        .set_language(language)
        .map_err(|e| GraphifyError::ExtractionFailed(format!("{}: {}", path.display(), e)))?;
    let source = std::fs::read(path)?;
    let tree = parser.parse(&source, None).ok_or_else(|| {
        GraphifyError::ExtractionFailed(format!("{}: parse returned None", path.display()))
    })?;
    Ok((source, tree))
}

/// Last segment of a possibly qualified / generic type name:
/// `a.b.Base<T>` and `ns::Base` both give `Base`.
pub(crate) fn simple_name(raw: &str) -> String {
    let base = raw.split('<').next().unwrap_or("");
    base.rsplit(['.', ':'])
        .find(|s| !s.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string()
}

/// Record a method body so the call-graph pass can walk it.
pub(crate) fn push_body(ctx: &mut ExtractionContext, nid: &str, body: &Node) {
    ctx.function_bodies_indices
        .push((nid.to_string(), body.start_byte(), body.end_byte()));
}

/// Add a class-like node plus the `contains` edge from its parent (the
/// enclosing class, or the file node). Returns the node id.
pub(crate) fn add_type(
    ctx: &mut ExtractionContext,
    parent_nid: Option<&str>,
    name: &str,
    line: usize,
) -> String {
    let nid = make_id(&[parent_nid.unwrap_or(&ctx.stem.clone()), name]);
    ctx.add_node(&nid, name, line);
    let parent = parent_nid.map_or_else(|| ctx.file_nid.clone(), str::to_string);
    ctx.add_extracted_edge(&parent, &nid, "contains", line);
    nid
}

/// Add a method node (`.name()`) under `class_nid`, or a free function
/// (`name()`) under the file when there is no enclosing class. Returns the
/// node id.
pub(crate) fn add_callable(
    ctx: &mut ExtractionContext,
    class_nid: Option<&str>,
    name: &str,
    line: usize,
) -> String {
    match class_nid {
        Some(class) => {
            let nid = make_id(&[class, name]);
            ctx.add_node(&nid, &format!(".{name}()"), line);
            ctx.add_extracted_edge(class, &nid, "method", line);
            nid
        }
        None => {
            let nid = make_id(&[&ctx.stem.clone(), name]);
            ctx.add_node(&nid, &format!("{name}()"), line);
            ctx.add_extracted_edge(&ctx.file_nid.clone(), &nid, "contains", line);
            nid
        }
    }
}

/// Add a file-level `imports_from` edge to the module with the given stem.
pub(crate) fn add_import(ctx: &mut ExtractionContext, module: &str, line: usize) {
    if module.is_empty() {
        return;
    }
    let tgt = make_id(&[module]);
    if tgt.is_empty() {
        return;
    }
    ctx.add_edge(
        &ctx.file_nid.clone(),
        &tgt,
        "imports_from",
        line,
        "EXTRACTED",
        1.0,
    );
}

/// Bind pending inheritance edges. A base declared in this file binds to its
/// node; otherwise a phantom node keyed by the bare name is used (same as the
/// Python extractor), so a later cross-file pass can match it.
pub(crate) fn resolve_inherits(ctx: &mut ExtractionContext, pending: Vec<PendingInherit>) {
    for (class_nid, base, relation, line) in pending {
        if base.is_empty() {
            continue;
        }
        let mut base_nid = make_id(&[&ctx.stem.clone(), &base]);
        if !ctx.seen_ids.contains(&base_nid) {
            // Nested declarations carry their parent in the id; match by label.
            if let Some(n) = ctx
                .nodes
                .iter()
                .find(|n| n.label == base && !n.source_file.is_empty())
            {
                base_nid = n.id.clone();
            }
        }
        if !ctx.seen_ids.contains(&base_nid) {
            base_nid = make_id(&[&base]);
            if !ctx.seen_ids.contains(&base_nid) {
                ctx.add_phantom_node(&base_nid, &base);
            }
        }
        if base_nid != class_nid {
            ctx.add_extracted_edge(&class_nid, &base_nid, relation, line);
        }
    }
}

/// Which AST nodes are calls, how to name the callee, and where a nested
/// callable starts (so its calls are attributed to it, not the outer body).
pub(crate) struct CallSpec {
    pub callee: fn(&Node, &[u8]) -> Option<String>,
    pub boundary_kinds: &'static [&'static str],
}

/// Pass 2: walk each recorded body and emit INFERRED `calls` edges to
/// callables/types defined in the same file.
pub(crate) fn run_call_graph(
    root: &Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    spec: &CallSpec,
) {
    let label_to_nid = ctx.build_label_map();
    let mut seen = std::collections::HashSet::new();
    let bodies = ctx.function_bodies_indices.clone();
    for (caller, start, end) in &bodies {
        walk_calls(
            root,
            source,
            ctx,
            spec,
            caller,
            *start,
            *end,
            &label_to_nid,
            &mut seen,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn walk_calls(
    node: &Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    spec: &CallSpec,
    caller: &str,
    body_start: usize,
    body_end: usize,
    label_to_nid: &std::collections::HashMap<String, String>,
    seen: &mut std::collections::HashSet<(String, String)>,
) {
    if node.end_byte() <= body_start || node.start_byte() >= body_end {
        return;
    }
    if node.start_byte() > body_start && spec.boundary_kinds.contains(&node.kind()) {
        return;
    }
    if let Some(name) = (spec.callee)(node, source) {
        let line = node.start_position().row + 1;
        ctx.emit_call_edge(caller, &name, line, label_to_nid, seen);
    }
    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            walk_calls(
                &child,
                source,
                ctx,
                spec,
                caller,
                body_start,
                body_end,
                label_to_nid,
                seen,
            );
        }
    }
}

/// Final step shared by every extractor.
pub(crate) fn finish(path: &Path, mut ctx: ExtractionContext) -> ExtractionResult {
    ctx.clean_edges();
    ctx.into_raw_result().into_extraction_result(path)
}

// ── C / C++ shared walker ───────────────────────────────────────────────────

/// Resolve the name a declarator declares, with its C++ scope when written
/// out of line (`Foo::bar` gives `(Some("Foo"), "bar")`).
pub(crate) fn declared_name(node: &Node, source: &[u8]) -> Option<(Option<String>, String)> {
    let mut cur = *node;
    let mut scope: Option<String> = None;
    for _ in 0..32 {
        match cur.kind() {
            "identifier"
            | "field_identifier"
            | "type_identifier"
            | "destructor_name"
            | "operator_name"
            | "namespace_identifier" => {
                return Some((scope, text(&cur, source)));
            }
            "qualified_identifier" => {
                if let Some(s) = cur.child_by_field_name("scope") {
                    scope = Some(simple_name(&text(&s, source)));
                }
                cur = cur.child_by_field_name("name")?;
            }
            _ => {
                cur = cur
                    .child_by_field_name("declarator")
                    .or_else(|| cur.named_child(0))?;
            }
        }
    }
    None
}

/// Module stem named by a `#include`: `<sys/foo.h>` and `"foo.h"` both give `foo`.
pub(crate) fn include_module(node: &Node, source: &[u8]) -> Option<String> {
    let path = node.child_by_field_name("path")?;
    let raw = text(&path, source);
    let raw = raw.trim_matches(|c| c == '"' || c == '<' || c == '>');
    let file = raw.rsplit('/').next().unwrap_or("");
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    Some(stem.to_string())
}

/// Callee name for C and C++ call expressions.
pub(crate) fn c_family_callee(node: &Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "call_expression" => {
            let f = node.child_by_field_name("function")?;
            match f.kind() {
                "identifier" => Some(text(&f, source)),
                "field_expression" => f.child_by_field_name("field").map(|n| text(&n, source)),
                "qualified_identifier" => f.child_by_field_name("name").map(|n| text(&n, source)),
                "template_function" => f.child_by_field_name("name").map(|n| text(&n, source)),
                _ => None,
            }
        }
        "new_expression" => node
            .child_by_field_name("type")
            .map(|t| simple_name(&text(&t, source))),
        _ => None,
    }
}

const TYPE_SPECIFIERS: &[&str] = &[
    "struct_specifier",
    "union_specifier",
    "enum_specifier",
    "class_specifier",
];

/// Walk a C or C++ translation unit.
pub(crate) fn walk_c_family(
    node: &Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    class_nid: Option<&str>,
    inh: &mut Vec<PendingInherit>,
) {
    let kind = node.kind();
    let line = node.start_position().row + 1;

    if kind == "function_definition" {
        let Some((scope, name)) = node
            .child_by_field_name("declarator")
            .and_then(|d| declared_name(&d, source))
        else {
            return;
        };
        // `void Foo::bar() {}` outside the class attaches to Foo.
        let owner = match (&scope, class_nid) {
            (Some(s), _) => {
                let nid = make_id(&[&ctx.stem.clone(), s]);
                ctx.add_node(&nid, s, line);
                Some(nid)
            }
            (None, Some(c)) => Some(c.to_string()),
            (None, None) => None,
        };
        let nid = add_callable(ctx, owner.as_deref(), &name, line);
        if let Some(body) = node.child_by_field_name("body") {
            push_body(ctx, &nid, &body);
        }
        return;
    }

    // Forward declarations and `struct foo *p` uses have no body.
    if TYPE_SPECIFIERS.contains(&kind)
        && let Some(body) = node.child_by_field_name("body")
        && let Some(name_node) = node.child_by_field_name("name")
    {
        let name = simple_name(&text(&name_node, source));
        let nid = add_type(ctx, class_nid, &name, line);
        for i in 0..node.child_count() {
            if let Some(child) = node.child(i)
                && child.kind() == "base_class_clause"
            {
                for j in 0..child.named_child_count() {
                    if let Some(base) = child.named_child(j)
                        && matches!(
                            base.kind(),
                            "type_identifier" | "qualified_identifier" | "template_type"
                        )
                    {
                        let base = simple_name(&text(&base, source));
                        inh.push((nid.clone(), base, "inherits", line));
                    }
                }
            }
        }
        for i in 0..body.child_count() {
            if let Some(child) = body.child(i) {
                walk_c_family(&child, source, ctx, Some(&nid), inh);
            }
        }
        return;
    }

    // `typedef struct { ... } Name;`: the aggregate is anonymous, the typedef
    // name is the type's name.
    if kind == "type_definition"
        && let Some(ty) = node.child_by_field_name("type")
        && TYPE_SPECIFIERS.contains(&ty.kind())
        && ty.child_by_field_name("name").is_none()
        && ty.child_by_field_name("body").is_some()
        && let Some((_, name)) = node
            .child_by_field_name("declarator")
            .and_then(|d| declared_name(&d, source))
    {
        add_type(ctx, class_nid, &name, line);
        return;
    }

    if kind == "preproc_include" {
        if let Some(module) = include_module(node, source) {
            add_import(ctx, &module, line);
        }
        return;
    }

    for i in 0..node.child_count() {
        if let Some(child) = node.child(i) {
            walk_c_family(&child, source, ctx, class_nid, inh);
        }
    }
}

/// Call-graph spec shared by C and C++.
pub(crate) const C_FAMILY_CALLS: CallSpec = CallSpec {
    callee: c_family_callee,
    boundary_kinds: &["function_definition"],
};

/// Test helper: write `source` to `dir/name` and return the path.
#[cfg(test)]
pub(crate) fn write_fixture(dir: &Path, name: &str, source: &str) -> std::path::PathBuf {
    let p = dir.join(name);
    std::fs::write(&p, source).expect("write fixture");
    p
}

/// Test helper: does `result` hold an entity with this label?
#[cfg(test)]
pub(crate) fn has_entity(result: &ExtractionResult, label: &str) -> bool {
    result.entities.iter().any(|e| e.label == label)
}

/// Test helper: legacy id of the entity with this label.
#[cfg(test)]
pub(crate) fn legacy_of(result: &ExtractionResult, label: &str) -> String {
    result
        .entities
        .iter()
        .find(|e| e.label == label)
        .and_then(|e| e.legacy_id.clone())
        .unwrap_or_else(|| panic!("no entity labelled {label}"))
}

/// Test helper: is there an edge `src -[relation]-> tgt` (by entity label)?
#[cfg(test)]
pub(crate) fn has_edge(result: &ExtractionResult, src: &str, relation: &str, tgt: &str) -> bool {
    use crate::entity::EntityId;
    let s = EntityId::from_legacy_string(&legacy_of(result, src));
    let t = EntityId::from_legacy_string(&legacy_of(result, tgt));
    result
        .relationships
        .iter()
        .any(|r| r.source == s && r.target == t && r.relation_type.to_string() == relation)
}
