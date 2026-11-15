//! C++ language support -- custom extractor (same output shape as Go/Rust).
//!
//! Superset of the C walker: classes/structs with base-class edges, inline
//! and out-of-line (`Foo::bar`) methods, namespaces and templates (walked
//! through), `#include` directives, and call / `new` edges.

use std::path::Path;

use crate::GraphifyError;
use crate::model::ExtractionResult;

#[cfg(feature = "lang-cpp")]
use crate::extract::ast::ExtractionContext;
#[cfg(feature = "lang-cpp")]
use crate::extract::lang::common;

/// Extract entities and relationships from a C++ source or header file.
#[cfg(feature = "lang-cpp")]
pub fn extract_cpp(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    use tree_sitter::Language;

    let language = Language::from(tree_sitter_cpp::LANGUAGE);
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

#[cfg(not(feature = "lang-cpp"))]
pub fn extract_cpp(path: &Path) -> Result<ExtractionResult, GraphifyError> {
    Ok(ExtractionResult::empty(path.to_path_buf()))
}

#[cfg(all(test, feature = "lang-cpp"))]
mod tests {
    use super::*;
    use crate::extract::ast::{extract_ast, make_id};
    use crate::extract::lang::LanguageId;
    use crate::extract::lang::common::{has_edge, has_entity, legacy_of, write_fixture};

    const SRC: &str = r#"#include <vector>
#include "shape.hpp"

namespace geo {

class Circle : public Shape, private Named {
public:
    Circle(double r) : r_(r) { reset(); }
    double area() const { return scale(r_) * r_; }
    void reset();
private:
    double r_;
};

template <typename T>
T scale(T v) { return v * 2; }

}  // namespace geo

void geo::Circle::reset() { r_ = 0; }

class Shape {};
struct Named { int id; };

int main() {
    geo::Circle c(1.0);
    Circle *p = new Circle(2.0);
    c.reset();
    return 0;
}
"#;

    fn extract(dir: &tempfile::TempDir, name: &str, src: &str) -> ExtractionResult {
        let p = write_fixture(dir.path(), name, src);
        extract_ast(&p, LanguageId::Cpp).expect("extract")
    }

    #[test]
    fn classes_methods_and_functions() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "circle.cpp", SRC);
        for label in [
            "circle.cpp",
            "Circle",
            "Shape",
            "Named",
            ".Circle()",
            ".area()",
            ".reset()",
            "scale()",
            "main()",
        ] {
            assert!(has_entity(&r, label), "missing {label}");
        }
        assert!(has_edge(&r, "Circle", "method", ".area()"));
        assert!(has_edge(&r, "Circle", "method", ".Circle()"));
        assert!(has_edge(&r, "circle.cpp", "contains", "main()"));
        // `void Circle::reset() {}` outside the class attaches to the class
        // node declared in the same file (no phantom duplicate).
        assert!(has_edge(&r, "Circle", "method", ".reset()"));
        assert_eq!(r.entities.iter().filter(|e| e.label == "Circle").count(), 1);
    }

    #[test]
    fn base_classes_bind_to_later_declarations() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "circle.cpp", SRC);
        assert!(has_edge(&r, "Circle", "inherits", "Shape"));
        assert!(has_edge(&r, "Circle", "inherits", "Named"));
        assert_eq!(legacy_of(&r, "Shape"), make_id(&["circle", "Shape"]));
    }

    #[test]
    fn includes_become_imports_from_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "circle.cpp", SRC);
        let file = crate::entity::EntityId::from_legacy_string("circle");
        let targets: Vec<_> = r
            .relationships
            .iter()
            .filter(|rel| rel.source == file && rel.relation_type.to_string() == "imports_from")
            .map(|rel| rel.target.clone())
            .collect();
        for module in ["vector", "shape"] {
            let t = crate::entity::EntityId::from_legacy_string(&make_id(&[module]));
            assert!(targets.contains(&t), "no include edge to {module}");
        }
    }

    #[test]
    fn call_edges() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "circle.cpp", SRC);
        // method calling a template function, ctor calling a method,
        // and main calling a method through an object.
        assert!(has_edge(&r, ".area()", "calls", "scale()"));
        assert!(has_edge(&r, ".Circle()", "calls", ".reset()"));
        assert!(has_edge(&r, "main()", "calls", ".reset()"));
    }

    #[test]
    fn malformed_source_does_not_panic() {
        let d = tempfile::tempdir().unwrap();
        let r = extract(&d, "bad.cpp", "class { void (:: template<");
        assert!(has_entity(&r, "bad.cpp"));
    }
}
