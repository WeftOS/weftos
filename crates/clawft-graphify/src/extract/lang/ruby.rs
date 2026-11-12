//! Ruby language support -- custom extractor (same output shape as Go/Rust).
//!
//! Handles `class` / `module` (with superclass edges), instance and
//! singleton (`def self.x`) methods, `require` / `require_relative` / `load`
//! and call edges for calls with an explicit receiver or argument list.
//! A bare identifier with no parentheses (`helper`) parses as a variable
//! reference in Ruby's grammar, so it is not treated as a call.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-ruby")]
use crate::extract::ast::ExtractionContext;
#[cfg(feature = "lang-ruby")]
use crate::extract::lang::common::{self, CallSpec, PendingInherit};

/// Extract entities and relationships from a Ruby source file.
#[cfg(feature = "lang-ruby")]
pub fn extract_ruby(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::Language;

    let language = Language::from(tree_sitter_ruby::LANGUAGE);
    let (source, tree) = common::parse(path, &language)?;

    let mut ctx = ExtractionContext::new(path);
    let mut inh = Vec::new();
    walk_ruby(&tree.root_node(), &source, &mut ctx, None, &mut inh);
    common::resolve_inherits(&mut ctx, inh);
    common::run_call_graph(&tree.root_node(), &source, &mut ctx, &RUBY_CALLS);
    Ok(common::finish(path, ctx))
}

#[cfg(not(feature = "lang-ruby"))]
pub fn extract_ruby(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(feature = "lang-ruby")]
const RUBY_CALLS: CallSpec = CallSpec {
    callee: ruby_callee,
    boundary_kinds: &["method", "singleton_method"],
};

#[cfg(feature = "lang-ruby")]
fn ruby_callee(node: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    if node.kind() != "call" {
        return None;
    }
    node.child_by_field_name("method")
        .map(|n| common::text(&n, source))
}

/// First string literal argument of a call, without its quotes.
#[cfg(feature = "lang-ruby")]
fn first_string_arg(call: &tree_sitter::Node, source: &[u8]) -> Option<String> {
    let args = call.child_by_field_name("arguments")?;
    for i in 0..args.named_child_count() {
        let arg = args.named_child(i)?;
        if arg.kind() == "string" {
            let raw = common::text(&arg, source);
            return Some(raw.trim_matches(|c| c == '"' || c == '\'').to_string());
        }
    }
    None
}

#[cfg(feature = "lang-ruby")]
fn walk_ruby(
    node: &tree_sitter::Node,
    source: &[u8],
    ctx: &mut ExtractionContext,
    scope_nid: Option<&str>,
    inh: &mut Vec<PendingInherit>,
) {
    let kind = node.kind();
    let line = node.start_position().row + 1;

    match kind {
        "class" | "module" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = common::simple_name(&common::text(&name_node, source));
                let nid = common::add_type(ctx, scope_nid, &name, line);
                if let Some(sup) = node.child_by_field_name("superclass") {
                    let base = common::simple_name(&common::text(&sup, source).replace('<', ""));
                    inh.push((nid.clone(), base, "inherits", line));
                }
                for i in 0..node.child_count() {
                    if let Some(child) = node.child(i) {
                        walk_ruby(&child, source, ctx, Some(&nid), inh);
                    }
                }
            }
        }
        "method" | "singleton_method" => {
            if let Some(name_node) = node.child_by_field_name("name") {
                let name = common::text(&name_node, source);
                let nid = common::add_callable(ctx, scope_nid, &name, line);
                // Whole node: also covers endless `def f = expr` methods.
                common::push_body(ctx, &nid, node);
            }
        }
        "call" => {
            let method = node
                .child_by_field_name("method")
                .map(|m| common::text(&m, source));
            if node.child_by_field_name("receiver").is_none()
                && matches!(
                    method.as_deref(),
                    Some("require" | "require_relative" | "load")
                )
            {
                if let Some(arg) = first_string_arg(node, source) {
                    let file = arg.rsplit('/').next().unwrap_or("");
                    let module = file.strip_suffix(".rb").unwrap_or(file);
                    common::add_import(ctx, module, line);
                }
                return;
            }
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    walk_ruby(&child, source, ctx, scope_nid, inh);
                }
            }
        }
        _ => {
            for i in 0..node.child_count() {
                if let Some(child) = node.child(i) {
                    walk_ruby(&child, source, ctx, scope_nid, inh);
                }
            }
        }
    }
}

#[cfg(all(test, feature = "lang-ruby"))]
mod tests {
    use super::*;
    use crate::extract::ast::{extract_ast, make_id};
    use crate::extract::lang::LanguageId;
    use crate::extract::lang::common::{has_edge, has_entity, legacy_of, write_fixture};

    const SRC: &str = r#"require 'json'
require_relative 'lib/models'
load "tasks.rb"

module Billing
  class Invoice < Record
    def initialize(items)
      @items = items
      validate(items)
    end

    def total
      @items.sum { |i| price_of(i) }
    end

    def self.build(items)
      new(items)
    end

    def validate(items)
      raise ArgumentError if items.empty?
    end

    def price_of(item)
      item.price
    end
  end

  class Record; end
end

def standalone(x)
  Billing::Invoice.build(x)
end
"#;

    fn extract(dir: &tempfile::TempDir, name: &str, src: &str) -> ExtractionResult {
        let p = write_fixture(dir.path(), name, src);
        extract_ast(&p, LanguageId::Ruby).expect("extract")
    }

    #[test]
    fn modules_classes_and_methods() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "billing.rb", SRC);
        for label in [
            "billing.rb",
            "Billing",
            "Invoice",
            "Record",
            ".initialize()",
            ".total()",
            ".build()",
            ".validate()",
            "standalone()",
        ] {
            assert!(has_entity(&r, label), "missing {label}");
        }
        assert!(has_edge(&r, "billing.rb", "contains", "Billing"));
        assert!(has_edge(&r, "Billing", "contains", "Invoice"));
        assert!(has_edge(&r, "Invoice", "method", ".total()"));
        assert!(has_edge(&r, "Invoice", "method", ".build()"));
        assert!(has_edge(&r, "billing.rb", "contains", "standalone()"));
    }

    #[test]
    fn superclass_binds_to_in_file_node() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "billing.rb", SRC);
        assert!(has_edge(&r, "Invoice", "inherits", "Record"));
        // Record is nested in Billing, declared after Invoice.
        assert_eq!(
            legacy_of(&r, "Record"),
            make_id(&["billing_billing", "Record"])
        );
    }

    #[test]
    fn requires_become_imports_from_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "billing.rb", SRC);
        let file = crate::entity::EntityId::from_legacy_string("billing");
        let targets: Vec<_> = r
            .relationships
            .iter()
            .filter(|rel| rel.source == file && rel.relation_type.to_string() == "imports_from")
            .map(|rel| rel.target.clone())
            .collect();
        for module in ["json", "models", "tasks"] {
            let t = crate::entity::EntityId::from_legacy_string(&make_id(&[module]));
            assert!(targets.contains(&t), "no require edge to {module}");
        }
    }

    #[test]
    fn call_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "billing.rb", SRC);
        assert!(has_edge(&r, ".initialize()", "calls", ".validate()"));
        assert!(has_edge(&r, ".total()", "calls", ".price_of()"));
        assert!(has_edge(&r, "standalone()", "calls", ".build()"));
    }

    #[test]
    fn malformed_source_does_not_panic() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "bad.rb", "class < def ( end end");
        assert!(has_entity(&r, "bad.rb"));
    }
}
