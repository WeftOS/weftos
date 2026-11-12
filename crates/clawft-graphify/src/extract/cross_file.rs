//! Cross-file import resolution.
//!
//! GRAPH-011: Turns file-level imports into class-level INFERRED edges.
//! Currently Python-only. Extensible for other languages.
//!
//! Two-pass algorithm:
//!   Pass 1: Build `stem -> {ClassName: EntityId}` global map from all results
//!   Pass 2: For each Python file with `from .module import Name`, resolve Name
//!           in global map, emit INFERRED "uses" edges from local classes

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::entity::EntityId;
use crate::model::ExtractionResult;
use crate::relationship::{Confidence, RelationType, Relationship};

use super::ast::make_id;

/// Resolve cross-file imports and produce additional "uses" relationships.
///
/// Given per-file extraction results and their paths, builds a global entity
/// map and resolves `from .module import Name` statements into class-level
/// INFERRED edges.
pub fn resolve_cross_file_imports(
    per_file: &[ExtractionResult],
    paths: &[PathBuf],
) -> Vec<Relationship> {
    if per_file.is_empty() || paths.is_empty() {
        return Vec::new();
    }

    // Pass 1: build global map: stem -> {label -> legacy_id}
    let mut stem_to_entities: HashMap<String, HashMap<String, String>> = HashMap::new();

    for result in per_file {
        for entity in &result.entities {
            let src = match &entity.source_file {
                Some(s) if !s.is_empty() => s,
                _ => continue,
            };
            let stem = Path::new(src.as_str())
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            let label = &entity.label;

            // Only index real classes/functions (not file nodes, not method stubs)
            if !label.is_empty()
                && !label.ends_with(')')
                && !label.ends_with(".py")
                && !label.starts_with('_')
                && let Some(ref legacy) = entity.legacy_id
            {
                stem_to_entities
                    .entry(stem)
                    .or_default()
                    .insert(label.clone(), legacy.clone());
            }
        }
    }

    // Pass 2: resolve imports
    // Currently Python-only -- uses heuristic parsing of import edges
    let mut new_edges: Vec<Relationship> = Vec::new();

    for (result, path) in per_file.iter().zip(paths.iter()) {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        if ext != "py" {
            continue;
        }

        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("")
            .to_string();
        let str_path = path.to_string_lossy().to_string();
        let file_nid = make_id(&[&stem]);

        // Find all classes/entities defined in this file (the importers)
        let local_classes: Vec<String> = result
            .entities
            .iter()
            .filter(|e| {
                e.source_file.as_deref() == Some(str_path.as_str())
                    && !e.label.ends_with(')')
                    && !e.label.ends_with(".py")
                    && e.legacy_id.as_deref() != Some(&file_nid)
            })
            .filter_map(|e| e.legacy_id.clone())
            .collect();

        if local_classes.is_empty() {
            continue;
        }

        // Find imports_from edges to resolve
        for rel in &result.relationships {
            if rel.relation_type != RelationType::ImportsFrom {
                continue;
            }

            // The target is identified by the relationship target EntityId.
            let target_hex = rel.target.to_hex();

            // Check if the target stem has known entities.
            for (target_stem, entities) in &stem_to_entities {
                // Relationship targets are hashed `EntityId`s; hash the stem's
                // legacy id the same way before comparing (the raw stem string
                // never equals a hex digest, which silently dropped every edge).
                let target_stem_id = make_id(&[target_stem]);
                if EntityId::from_legacy_string(&target_stem_id).to_hex() != target_hex {
                    continue;
                }

                for entity_legacy_id in entities.values() {
                    for local_class_nid in &local_classes {
                        new_edges.push(Relationship {
                            source: EntityId::from_legacy_string(local_class_nid),
                            target: EntityId::from_legacy_string(entity_legacy_id),
                            relation_type: RelationType::RelatedTo,
                            confidence: Confidence::Inferred,
                            weight: 0.8,
                            source_file: Some(str_path.clone()),
                            source_location: rel.source_location.clone(),
                            metadata: serde_json::json!({}),
                            embedding: None,
                        });
                    }
                }
            }
        }
    }

    new_edges
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::EntityType;
    use crate::entity::FileType;
    use crate::model::Entity;

    fn make_entity(legacy: &str, label: &str, source_file: &str) -> Entity {
        Entity {
            id: EntityId::from_legacy_string(legacy),
            entity_type: EntityType::Class,
            label: label.to_string(),
            source_file: Some(source_file.to_string()),
            source_location: Some("L1".to_string()),
            file_type: FileType::Code,
            metadata: serde_json::json!({}),
            legacy_id: Some(legacy.to_string()),
            iri: None,
        }
    }

    fn make_rel(source: &str, target: &str, rel_type: RelationType) -> Relationship {
        Relationship {
            source: EntityId::from_legacy_string(source),
            target: EntityId::from_legacy_string(target),
            relation_type: rel_type,
            confidence: Confidence::Extracted,
            weight: 1.0,
            source_file: Some("auth.py".to_string()),
            source_location: Some("L1".to_string()),
            metadata: serde_json::json!({}),
            embedding: None,
        }
    }

    #[test]
    fn empty_inputs_return_empty() {
        let result = resolve_cross_file_imports(&[], &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn cross_file_python_resolution() {
        // models.py defines Response
        let models_result = ExtractionResult {
            source_file: "models.py".to_string(),
            entities: vec![
                make_entity("models", "models.py", "models.py"),
                make_entity("models_response", "Response", "models.py"),
            ],
            relationships: vec![],
            hyperedges: vec![],
            input_tokens: 0,
            output_tokens: 0,
            errors: vec![],
        };

        // auth.py defines DigestAuth and imports from models
        let auth_result = ExtractionResult {
            source_file: "auth.py".to_string(),
            entities: vec![
                make_entity("auth", "auth.py", "auth.py"),
                make_entity("auth_digestauth", "DigestAuth", "auth.py"),
            ],
            relationships: vec![make_rel("auth", "models", RelationType::ImportsFrom)],
            hyperedges: vec![],
            input_tokens: 0,
            output_tokens: 0,
            errors: vec![],
        };

        let per_file = vec![models_result, auth_result];
        let paths = vec![PathBuf::from("models.py"), PathBuf::from("auth.py")];

        let edges = resolve_cross_file_imports(&per_file, &paths);
        // Should produce a related-to edge: DigestAuth -> Response
        assert!(!edges.is_empty());
        assert!(
            edges
                .iter()
                .all(|e| e.relation_type == RelationType::RelatedTo)
        );
        assert!(edges.iter().all(|e| e.confidence == Confidence::Inferred));
    }

    #[test]
    fn non_python_files_skipped() {
        let js_result = ExtractionResult {
            source_file: "app.js".to_string(),
            entities: vec![make_entity("app", "app.js", "app.js")],
            relationships: vec![],
            hyperedges: vec![],
            input_tokens: 0,
            output_tokens: 0,
            errors: vec![],
        };

        let edges = resolve_cross_file_imports(&[js_result], &[PathBuf::from("app.js")]);
        assert!(edges.is_empty());
    }
}
