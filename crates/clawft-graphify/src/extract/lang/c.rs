//! C language support -- custom extractor (same output shape as Go/Rust).
//!
//! Handles function definitions, struct/union/enum definitions (including
//! `typedef struct { .. } Name;`), `#include` directives and call edges. The
//! walker is shared with C++ (`common::walk_c_family`).

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-c")]
use crate::extract::ast::ExtractionContext;
#[cfg(feature = "lang-c")]
use crate::extract::lang::common;

/// Extract entities and relationships from a C source or header file.
#[cfg(feature = "lang-c")]
pub fn extract_c(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::Language;

    let language = Language::from(tree_sitter_c::LANGUAGE);
    let (source, tree) = common::parse(path, &language)?;

    let mut ctx = ExtractionContext::new(path);
    let mut inh = Vec::new();
    common::walk_c_family(&tree.root_node(), &source, &mut ctx, None, &mut inh);
    common::resolve_inherits(&mut ctx, inh);
    common::run_call_graph(
        &tree.root_node(),
        &source,
        &mut ctx,
        &common::C_FAMILY_CALLS,
    );
    Ok(common::finish(path, ctx))
}

#[cfg(not(feature = "lang-c"))]
pub fn extract_c(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(all(test, feature = "lang-c"))]
mod tests {
    use super::*;
    use crate::extract::ast::{extract_ast, make_id};
    use crate::extract::lang::LanguageId;
    use crate::extract::lang::common::{has_edge, has_entity, write_fixture};

    const SRC: &str = r#"#include <stdio.h>
#include "util/helpers.h"

struct Point { int x; int y; };
union Value { int i; float f; };
enum Color { RED, GREEN };
typedef struct { int len; } Buffer;

static int square(int v) { return v * v; }

int *make(void) { return 0; }

int main(int argc, char **argv) {
    int a = square(3);
    printf("%d\n", a);
    return make() != 0;
}
"#;

    fn extract(dir: &tempfile::TempDir, name: &str, src: &str) -> ExtractionResult {
        let p = write_fixture(dir.path(), name, src);
        extract_ast(&p, LanguageId::C).expect("extract")
    }

    #[test]
    fn functions_and_aggregates() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "prog.c", SRC);
        for label in [
            "prog.c", "Point", "Value", "Color", "Buffer", "square()", "make()", "main()",
        ] {
            assert!(has_entity(&r, label), "missing {label}");
        }
        assert!(has_edge(&r, "prog.c", "contains", "main()"));
        assert!(has_edge(&r, "prog.c", "contains", "Point"));
        assert!(has_edge(&r, "prog.c", "contains", "Buffer"));
    }

    #[test]
    fn includes_become_imports_from_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "prog.c", SRC);
        let file = crate::entity::EntityId::from_legacy_string("prog");
        let targets: Vec<_> = r
            .relationships
            .iter()
            .filter(|rel| rel.source == file && rel.relation_type.to_string() == "imports_from")
            .map(|rel| rel.target.clone())
            .collect();
        for module in ["stdio", "helpers"] {
            let t = crate::entity::EntityId::from_legacy_string(&make_id(&[module]));
            assert!(targets.contains(&t), "no include edge to {module}");
        }
    }

    #[test]
    fn call_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "prog.c", SRC);
        assert!(has_edge(&r, "main()", "calls", "square()"));
        assert!(has_edge(&r, "main()", "calls", "make()"));
        // printf is not defined in this file: no edge, and no dangling node.
        assert!(!has_entity(&r, "printf()"));
    }

    #[test]
    fn header_with_prototypes_only_yields_types() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(
            &d,
            "api.h",
            "struct Node;\nint f(int);\nstruct Node { int v; };\n",
        );
        assert!(has_entity(&r, "Node"));
        assert!(!has_entity(&r, "f()"));
    }

    #[test]
    fn malformed_source_does_not_panic() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "bad.c", "int main( { struct ");
        assert!(has_entity(&r, "bad.c"));
    }
}
