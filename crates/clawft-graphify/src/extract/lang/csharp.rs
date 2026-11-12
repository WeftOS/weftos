//! C# language support -- custom extractor (same output shape as Go/Rust).
//!
//! Handles class/interface/struct/record/enum declarations (with base-list
//! edges), methods and constructors, `using` directives (namespaces are
//! walked through), and invocation / `new` call edges.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-csharp")]
use crate::extract::ast::ExtractionContext;
#[cfg(feature = "lang-csharp")]
use crate::extract::lang::common::{self, CallSpec, PendingInherit};

/// Extract entities and relationships from a C# source file.
#[cfg(feature = "lang-csharp")]
pub fn extract_csharp(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::Language;

    let language = Language::from(tree_sitter_c_sharp::LANGUAGE);
    let (source, tree) = common::parse(path, &language)?;

    let mut ctx = ExtractionContext::new(path);
    let mut inh = Vec::new();
    walk_csharp(&tree.root_node(), &source, &mut ctx, None, &mut inh);
    common::resolve_inherits(&mut ctx, inh);
    common::run_call_graph(&tree.root_node(), &source, &mut ctx, &CSHARP_CALLS);
    Ok(common::finish(path, ctx))
}

#[cfg(not(feature = "lang-csharp"))]
pub fn extract_csharp(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(feature = "lang-csharp")]
const CSHARP_CALLS: CallSpec = CallSpec {
    callee: csharp_callee,
    boundary_kinds: &[
        "method_declaration",
        "constructor_declaration",
        "local_function_statement",
    ],
};

/// Name of the member being called, looking through generics (`Run<T>()`).
#[cfg(feature = "lang-csharp")]
fn member_name(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "identifier" => Some(common::text(node, source)),
        "generic_name" => node.named_child(0).map(|n| common::text(&n, source)),
        "member_access_expression" | "member_binding_expression" => node
            .child_by_field_name("name")
            .and_then(|n| member_name(&n, source)),
        _ => None,
    }
}

#[cfg(feature = "lang-csharp")]
fn csharp_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    match node.kind() {
        "invocation_expression" => node
            .child_by_field_name("function")
            .and_then(|f| member_name(&f, source)),
        "object_creation_expression" => node
            .child_by_field_name("type")
            .map(|t| common::simple_name(&common::text(&t, source))),
        _ => None,
    }
}

#[cfg(feature = "lang-csharp")]
fn walk_csharp(
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
        | "struct_declaration"
        | "record_declaration"
        | "record_struct_declaration"
        | "enum_declaration" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = common::text(&name_node, source);
                let nid = common::add_type(ctx, class_nid, &name, line);
                for i in 0..node.child_count() {
                    let Some(child) = node.child(i) else { continue };
                    if child.kind() != "base_list" {
                        continue;
                    }
                    for j in 0..child.named_child_count() {
                        if let Some(base) = child.named_child(j)
                            && matches!(
                                base.kind(),
                                "identifier" | "generic_name" | "qualified_name"
                            )
                        {
                            let base = common::simple_name(&common::text(&base, source));
                            inh.push((nid.clone(), base, "inherits", line));
                        }
                    }
                }
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        walk_csharp(&child, source, ctx, Some(&nid), inh);
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
        "using_directive" => {
            let raw = common::text(node, source);
            let raw = raw
                .trim()
                .trim_start_matches("global")
                .trim()
                .trim_start_matches("using")
                .trim()
                .trim_start_matches("static")
                .trim()
                .trim_end_matches(';')
                .trim();
            // `using Alias = Ns.Type;` imports the right-hand side.
            let target = raw.rsplit_once('=').map_or(raw, |(_, rhs)| rhs).trim();
            common::add_import(ctx, &common::simple_name(target), line);
        }
        _ => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    walk_csharp(&child, source, ctx, class_nid, inh);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "lang-csharp"))]
mod tests {
    use super::*;
    use crate::extract::ast::{extract_ast, make_id};
    use crate::extract::lang::LanguageId;
    use crate::extract::lang::common::{has_edge, has_entity, legacy_of, write_fixture};

    const SRC: &str = r#"using System;
using System.Collections.Generic;
using static System.Math;
using Json = Newtonsoft.Json.JsonConvert;

namespace Demo.App
{
    public class Service : BaseService, IDisposable
    {
        public Service() { Init(); }

        public void Run()
        {
            var x = Compute(2);
            Log<int>(x);
            helper.Assist();
            var r = new Repo();
        }

        private int Compute(int v) => v * 2;

        private void Init() { }

        private void Log<T>(T v) { }

        public void Dispose() { }
    }

    public class BaseService { }
    public interface IDisposable { void Dispose(); }
    public struct Vec { public int X; public int Len() { return X; } }
    public enum Mode { A, B }
    public record Pair(int A, int B);
    public class Repo { }
}
"#;

    fn extract(dir: &tempfile::TempDir, name: &str, src: &str) -> ExtractionResult {
        let p = write_fixture(dir.path(), name, src);
        extract_ast(&p, LanguageId::CSharp).expect("extract")
    }

    #[test]
    fn types_and_methods_through_namespaces() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Service.cs", SRC);
        for label in [
            "Service.cs",
            "Service",
            "BaseService",
            "IDisposable",
            "Vec",
            "Mode",
            "Pair",
            "Repo",
            ".Run()",
            ".Compute()",
            ".Init()",
            ".Log()",
            ".Len()",
        ] {
            assert!(has_entity(&r, label), "missing {label}");
        }
        assert!(has_edge(&r, "Service.cs", "contains", "Service"));
        assert!(has_edge(&r, "Service", "method", ".Run()"));
        assert!(has_edge(&r, "Service", "method", ".Service()"));
        assert!(has_edge(&r, "Vec", "method", ".Len()"));
    }

    #[test]
    fn base_list_binds_local_and_phantoms_external() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Service.cs", SRC);
        assert!(has_edge(&r, "Service", "inherits", "BaseService"));
        assert_eq!(
            legacy_of(&r, "BaseService"),
            make_id(&["Service", "BaseService"])
        );
        assert!(has_edge(&r, "Service", "inherits", "IDisposable"));
    }

    #[test]
    fn usings_become_imports_from_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Service.cs", SRC);
        let file = crate::entity::EntityId::from_legacy_string("service");
        let targets: Vec<_> = r
            .relationships
            .iter()
            .filter(|rel| rel.source == file && rel.relation_type.to_string() == "imports_from")
            .map(|rel| rel.target.clone())
            .collect();
        for module in ["System", "Generic", "Math", "JsonConvert"] {
            let t = crate::entity::EntityId::from_legacy_string(&make_id(&[module]));
            assert!(targets.contains(&t), "no using edge to {module}");
        }
    }

    #[test]
    fn call_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Service.cs", SRC);
        assert!(has_edge(&r, ".Run()", "calls", ".Compute()"));
        // generic method call
        assert!(has_edge(&r, ".Run()", "calls", ".Log()"));
        // constructor body calling a method
        assert!(has_edge(&r, ".Service()", "calls", ".Init()"));
        // `new Repo()` resolves to the in-file type
        assert!(has_edge(&r, ".Run()", "calls", "Repo"));
    }

    #[test]
    fn malformed_source_does_not_panic() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "Bad.cs", "class { void ( namespace");
        assert!(has_entity(&r, "Bad.cs"));
    }
}
