//! Vision-based extraction from images and diagrams via Claude Vision API.
//!
//! Feature-gated behind `vision-extract`. Sends images to Claude with a
//! structured prompt asking for entity and relationship extraction from
//! diagrams, screenshots, and other visual content.

#[cfg(feature = "vision-extract")]
use std::path::Path;

#[cfg(feature = "vision-extract")]
use crate::GraphifyError;
#[cfg(feature = "vision-extract")]
use crate::entity::{DomainTag, EntityId, EntityType, FileType};
#[cfg(feature = "vision-extract")]
use crate::model::{Entity, ExtractionResult};
#[cfg(feature = "vision-extract")]
use crate::relationship::{Confidence, Relationship};

/// The structured prompt sent alongside the image.
#[cfg(feature = "vision-extract")]
const VISION_PROMPT: &str = r#"Analyze this image (diagram, screenshot, or document) and extract:

1. **Entities**: Named components, systems, people, labels, or concepts visible in the image.
2. **Relationships**: Connections, arrows, or implied relationships between entities.

If this is an architecture diagram, extract all boxes/components and their connections.
If this is a screenshot, extract UI elements and their hierarchy.
If this is a document, OCR the text and extract key concepts.

Return ONLY valid JSON in this exact format (no markdown fences):
{
  "entities": [
    {"name": "EntityName", "type": "concept", "description": "Brief description"}
  ],
  "relationships": [
    {"source": "EntityA", "target": "EntityB", "relation": "related_to", "confidence": "INFERRED"}
  ]
}
"#;

/// Supported image extensions for vision extraction.
#[cfg(feature = "vision-extract")]
pub const IMAGE_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "gif", "svg", "bmp", "tiff"];

/// Check if a file path has a supported image extension.
#[cfg(feature = "vision-extract")]
pub fn is_image(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| IMAGE_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Extract entities and relationships from an image or diagram.
///
/// The `vision_complete` callback takes (prompt, image_bytes, media_type) and
/// returns the LLM's text response. This keeps the extraction logic decoupled
/// from the specific vision API transport.
///
/// # Arguments
/// * `image_path` - Path to the image file.
/// * `vision_complete` - Async function that sends image + prompt to the LLM.
///
/// # Errors
/// Returns `GraphifyError::LlmError` if the vision call fails.
#[cfg(feature = "vision-extract")]
pub async fn extract_vision<F, Fut>(
    image_path: &Path,
    vision_complete: F,
) -> Result<ExtractionResult, GraphifyError>
where
    F: FnOnce(String, Vec<u8>, String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    let image_bytes = std::fs::read(image_path).map_err(|e| {
        GraphifyError::ExtractionFailed(format!(
            "failed to read image {}: {e}",
            image_path.display()
        ))
    })?;

    let media_type = match image_path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("gif") => "image/gif",
        Some("svg") => "image/svg+xml",
        _ => "application/octet-stream",
    };

    let source_file = image_path.display().to_string();

    let response = vision_complete(
        VISION_PROMPT.to_string(),
        image_bytes,
        media_type.to_string(),
    )
    .await
    .map_err(GraphifyError::LlmError)?;

    // Re-use the semantic extraction JSON parser.
    let json_str = extract_json_from_response(&response);

    let parsed: VisionOutput = serde_json::from_str(json_str)
        .map_err(|e| GraphifyError::LlmError(format!("failed to parse vision response: {e}")))?;

    let mut name_to_id = std::collections::HashMap::new();
    let mut entities = Vec::new();

    for ve in &parsed.entities {
        let entity_type = parse_entity_type(&ve.entity_type);
        let id = EntityId::new(&DomainTag::Code, &entity_type, &ve.name, &source_file);
        name_to_id.insert(ve.name.clone(), id.clone());

        let mut metadata = serde_json::Map::new();
        if !ve.description.is_empty() {
            metadata.insert(
                "description".into(),
                serde_json::Value::String(ve.description.clone()),
            );
        }

        entities.push(Entity {
            id,
            entity_type,
            label: ve.name.clone(),
            source_file: Some(source_file.clone()),
            source_location: None,
            file_type: FileType::Image,
            metadata: serde_json::Value::Object(metadata),
            legacy_id: None,
            iri: None,
        });
    }

    let mut relationships = Vec::new();
    for vr in &parsed.relationships {
        let Some(src) = name_to_id.get(&vr.source) else {
            continue;
        };
        let Some(tgt) = name_to_id.get(&vr.target) else {
            continue;
        };
        let confidence = crate::relationship::Confidence::from_str_loose(&vr.confidence)
            .unwrap_or(Confidence::Inferred);
        relationships.push(Relationship {
            source: src.clone(),
            target: tgt.clone(),
            relation_type: crate::relationship::RelationType::RelatedTo,
            confidence,
            weight: confidence.to_weight(),
            source_file: Some(source_file.clone()),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
    }

    Ok(ExtractionResult {
        source_file,
        entities,
        relationships,
        hyperedges: Vec::new(),
        input_tokens: 0,
        output_tokens: 0,
        errors: Vec::new(),
    })
}

#[cfg(feature = "vision-extract")]
fn extract_json_from_response(response: &str) -> &str {
    let trimmed = response.trim();
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
    {
        return &trimmed[start..=end];
    }
    trimmed
}

#[cfg(feature = "vision-extract")]
fn parse_entity_type(s: &str) -> EntityType {
    match s.to_lowercase().as_str() {
        "module" => EntityType::Module,
        "class" => EntityType::Class,
        "function" => EntityType::Function,
        "service" => EntityType::Service,
        "concept" => EntityType::Concept,
        "person" => EntityType::Person,
        "location" => EntityType::Location,
        "organization" => EntityType::Organization,
        other => EntityType::Custom(other.to_string()),
    }
}

#[cfg(feature = "vision-extract")]
#[derive(Debug, serde::Deserialize)]
struct VisionOutput {
    #[serde(default)]
    entities: Vec<VisionEntity>,
    #[serde(default)]
    relationships: Vec<VisionRelationship>,
}

#[cfg(feature = "vision-extract")]
#[derive(Debug, serde::Deserialize)]
struct VisionEntity {
    name: String,
    #[serde(rename = "type", default)]
    entity_type: String,
    #[serde(default)]
    description: String,
}

#[cfg(feature = "vision-extract")]
#[derive(Debug, serde::Deserialize)]
struct VisionRelationship {
    source: String,
    target: String,
    #[serde(default = "default_confidence")]
    confidence: String,
}

#[cfg(feature = "vision-extract")]
fn default_confidence() -> String {
    "INFERRED".to_string()
}

#[cfg(all(test, feature = "vision-extract"))]
mod tests {
    use super::*;
    use crate::entity::{EntityType, FileType};
    use crate::relationship::Confidence;

    #[test]
    fn is_image_extensions() {
        assert!(is_image(Path::new("diagram.png")));
        assert!(is_image(Path::new("photo.JPEG")));
        assert!(is_image(Path::new("arch.webp")));
        assert!(is_image(Path::new("flow.SVG")));
        assert!(!is_image(Path::new("code.rs")));
        assert!(!is_image(Path::new("notes")));
    }

    #[test]
    fn extract_json_from_markdown_fences() {
        let input = r#"```json
{"entities": [], "relationships": []}
```"#;
        let result = extract_json_from_response(input);
        assert!(result.starts_with('{'));
        assert!(result.ends_with('}'));
    }

    #[test]
    fn parse_entity_types() {
        assert_eq!(parse_entity_type("service"), EntityType::Service);
        assert_eq!(parse_entity_type("CONCEPT"), EntityType::Concept);
        assert_eq!(
            parse_entity_type("widget"),
            EntityType::Custom("widget".into())
        );
    }

    #[tokio::test]
    async fn extract_vision_parses_mock_response() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.png");
        // Minimal valid 1×1 PNG (IHDR + empty IDAT is enough for read path).
        let png: &[u8] = &[
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x02, 0x00, 0x00,
            0x00, 0x90, 0x77, 0x53, 0xDE, 0x00, 0x00, 0x00, 0x0C, 0x49, 0x44, 0x41, 0x54, 0x08,
            0xD7, 0x63, 0xF8, 0xCF, 0xC0, 0x00, 0x00, 0x03, 0x01, 0x01, 0x00, 0x18, 0xDD, 0x8D,
            0xB4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4E, 0x44, 0xAE, 0x42, 0x60, 0x82,
        ];
        std::fs::write(&path, png).unwrap();

        let fake_vision = |_prompt: String, bytes: Vec<u8>, media: String| async move {
            assert!(!bytes.is_empty());
            assert_eq!(media, "image/png");
            Ok::<_, String>(
                r#"{"entities":[{"name":"Frontend","type":"service","description":"UI"},{"name":"Backend","type":"service","description":"API"}],"relationships":[{"source":"Frontend","target":"Backend","relation":"related_to","confidence":"EXTRACTED"}]}"#
                    .to_string(),
            )
        };

        let result = extract_vision(&path, fake_vision).await.unwrap();
        assert_eq!(result.entities.len(), 2);
        assert_eq!(result.entities[0].label, "Frontend");
        assert_eq!(result.entities[0].file_type, FileType::Image);
        assert_eq!(result.relationships.len(), 1);
        assert_eq!(result.relationships[0].confidence, Confidence::Extracted);
    }

    #[tokio::test]
    async fn extract_vision_rejects_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.png");
        std::fs::write(&path, b"not-a-real-png-but-readable").unwrap();

        let fake_vision = |_p: String, _b: Vec<u8>, _m: String| async {
            Ok::<_, String>("this is not json".into())
        };

        let err = extract_vision(&path, fake_vision).await.unwrap_err();
        assert!(
            err.to_string().contains("failed to parse vision response")
                || err.to_string().to_lowercase().contains("llm"),
            "got: {err}"
        );
    }
}
