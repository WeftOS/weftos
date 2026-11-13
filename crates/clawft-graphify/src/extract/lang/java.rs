//! Java language support -- custom extractor (same output shape as Go/Rust).
//!
//! Handles class/interface/enum/record/annotation declarations (with
//! `extends`/`implements` edges), methods and constructors, `import`
//! declarations and method-invocation / `new` call edges.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-java")]
use crate::extract::ast::ExtractionContext;
#[cfg(feature = "lang-java")]
use crate::extract::lang::common::{self, CallSpec, PendingInherit};

/// Extract entities and relationships from a Java source file.
#[cfg(feature = "lang-java")]
pub fn extract_java(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::Language;

    let language = Language::from(tree_sitter_java::LANGUAGE);
    let (source, tree) = common::parse(path, &language)?;

    let mut ctx = ExtractionContext::new(path);
    let mut inh = Vec::new();
    walk_java(&tree.root_node(), &source, &mut ctx, None, &mut inh);
    common::resolve_inherits(&mut ctx, inh);
    common::run_call_graph(&tree.root_node(), &source, &mut ctx, &JAVA_CALLS);
    Ok(common::finish(path, ctx))
}

#[cfg(not(feature = "lang-java"))]
pub fn extract_java(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(feature = "lang-java")]
const JAVA_CALLS: CallSpec = CallSpec {
    callee: java_callee,
    boundary_kinds: &["method_declaration", "constructor_declaration"],
};

#[cfg(feature = "lang-java")]
fn java_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "method_invocation" => node
            .child_by_field_name("name")
            .map(|n| common::text(&n, source)),
        "object_creation_expression" => node
            .child_by_field_name("type")
            .map(|t| common::simple_name(&common::text(&t, source))),
        _ => None,
    }
}

/// Collect type names under an `extends`/`implements` clause, skipping
/// generic arguments (`Comparable<Foo>` names `Comparable`, not `Foo`).
#[cfg(feature = "lang-java")]
fn collect_types(node: &tree_sitter::Node, source: &[u8], out: &mut Vec<String>) {
    match node.kind() {
        "type_identifier" => out.push(common::text(node, source)),
        "scoped_type_identifier" => out.push(common::simple_name(&common::text(node, source))),
        "type_arguments" => {}
        _ => {
            for i in 0..node.child_count() {
                if let Some(c) = node.child(i) {
                    collect_types(&c, source, out);
                }
            }
        }
    }
}

#[cfg(feature = "lang-java")]
fn walk_java(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    class_nid: Option<&str>,
    inh: &mut Vec<PendingInherit>,
) {
    let kind = node.kind();
    let line = node.start_position().row + 1;

    match kind {
        "class_declaration"
        | "interface_declaration"
        | "enum_declaration"
        | "record_declaration"
        | "annotation_type_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = common::text(&name_node, source);
                let nid = common::add_type(ctx, class_nid, &name, line);
                for i in 0..node.child_count() {
                    let Some(child) = node.child(i) else { continue };
                    let relation = match child.kind() {
                        "superclass" | "extends_interfaces" => "inherits",
                        "super_interfaces" => "implements",
                        _ => continue,
                    };
                    let mut bases = Vec::new();
                    collect_types(&child, source, &mut bases);
                    for base in bases {
                        inh.push((nid.clone(), base, relation, line));
                    }
                }
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        walk_java(&child, source, ctx, Some(&nid), inh);
                    }
                }
            }
        }
        "method_declaration" | "constructor_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = common::text(&name_node, source);
                let nid = common::add_callable(ctx, class_nid, &name, line);
                if let Some(body) = node.child_by_field_name("body") {
                    common::push_body(ctx, &nid, &body);
                }
            }
        }
        "import_declaration" => {
            let raw = common::text(node, source);
            let raw = raw
                .trim()
                .trim_start_matches("import")
                .trim()
                .trim_start_matches("static")
                .trim()
                .trim_end_matches(';')
                .trim()
                .trim_end_matches(".*");
            common::add_import(ctx, raw.rsplit('.').next().unwrap_or(""), line);
        }
        _ => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    walk_java(&child, source, ctx, class_nid, inh);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "lang-java"))]
mod tests {
    use super::*;
    use crate::extract::ast::{extract_ast, make_id};
    use crate::extract::lang::LanguageId;
    use crate::extract::lang::common::{has_edge, has_entity, legacy_of, write_fixture};

    const SRC: &str = r#"package demo;

import java.util.List;
import static java.lang.Math.max;
import demo.util.*;

public class Greeter extends Base implements Runnable, Comparable<Other> {
    private final Helper helper = new Helper();

    public Greeter() { init(); }

    public void run() {
        String s = format("hi");
        helper.assist();
    }

    private void init() {}

    static String format(String s) { return s; }

    interface Nested { void go(); }
}

class Base {}
interface Marker extends Runnable {}
enum Color { RED; Color next() { return RED; } }
record Point(int x, int y) { int sum() { return x + y; } }
"#;

    fn extract(dir: &tempfile::TempDir, name: &str, src: &str) -> ExtractionResult {
        let p = write_fixture(dir.path(), name, src);
        extract_ast(&p, LanguageId::Java).expect("extract")
    }

    #[test]
    fn types_methods_and_file_node() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Greeter.java", SRC);
        for label in [
            "Greeter.java",
            "Greeter",
            "Base",
            "Marker",
            "Color",
            "Point",
            "Nested",
            ".run()",
            ".init()",
            ".format()",
            ".sum()",
            ".next()",
        ] {
            assert!(has_entity(&r, label), "missing {label}");
        }
        assert!(has_edge(&r, "Greeter.java", "contains", "Greeter"));
        assert!(has_edge(&r, "Greeter", "contains", "Nested"));
        assert!(has_edge(&r, "Greeter", "method", ".run()"));
        assert!(has_edge(&r, "Greeter", "method", ".init()"));
        assert!(has_edge(&r, "Point", "method", ".sum()"));
    }

    #[test]
    fn inheritance_binds_local_base_and_phantoms_external() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Greeter.java", SRC);
        // Base is declared later in the file: must bind to the real node.
        assert!(has_edge(&r, "Greeter", "inherits", "Base"));
        assert_eq!(
            legacy_of(&r, "Base"),
            make_id(&["Greeter", "Base"]),
            "Base must be the in-file node, not a phantom"
        );
        assert!(has_edge(&r, "Greeter", "implements", "Runnable"));
        // Generic argument is not a base.
        assert!(has_edge(&r, "Greeter", "implements", "Comparable"));
        assert!(!has_entity(&r, "Other"));
        assert!(has_edge(&r, "Marker", "inherits", "Runnable"));
    }

    #[test]
    fn imports_become_imports_from_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Greeter.java", SRC);
        let file = crate::entity::EntityId::from_legacy_string("greeter");
        let targets: Vec<_> = r
            .relationships
            .iter()
            .filter(|rel| rel.source == file && rel.relation_type.to_string() == "imports_from")
            .map(|rel| rel.target.clone())
            .collect();
        for module in ["List", "max", "util"] {
            let t = crate::entity::EntityId::from_legacy_string(&make_id(&[module]));
            assert!(targets.contains(&t), "no import edge to {module}");
        }
    }

    #[test]
    fn call_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Greeter.java", SRC);
        // plain method call inside a method, and one from a constructor body
        assert!(has_edge(&r, ".run()", "calls", ".format()"));
        assert!(has_edge(&r, ".Greeter()", "calls", ".init()"));
        // unresolved receiver calls (helper.assist()) produce no edge
        assert!(!r.relationships.iter().any(|rel| {
            rel.relation_type.to_string() == "calls"
                && rel.target == crate::entity::EntityId::from_legacy_string("assist")
        }));
    }

    #[test]
    fn import_target_matches_sibling_file_node() {
        // Cross-file resolution keys on the stem, so an import of `Base` must
        // point at the node id the extractor gives `Base.java`'s file node.
        let d = tempfile::tempdir().unwrap();
        let base = extract(&d, "Base.java", "public class Base {}\n");
        let user = extract(&d, "User.java", "import demo.Base;\nclass User {}\n");
        let base_file = base
            .entities
            .iter()
            .find(|e| e.label == "Base.java")
            .unwrap()
            .id
            .clone();
        assert!(user.relationships.iter().any(|rel| {
            rel.relation_type.to_string() == "imports_from" && rel.target == base_file
        }));
    }

    #[test]
    fn malformed_source_does_not_panic() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Bad.java", "class { void ( ");
        assert!(has_entity(&r, "Bad.java"));
    }
}
