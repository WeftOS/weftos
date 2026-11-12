//! End-to-end tests for the `vision_extract` path (WEFT-381).
//!
//! Gated behind the `vision-extract` feature (see Cargo.toml
//! `[[test]] required-features`). Uses a mock vision callback — no real
//! LLM or network. Fixture image lives under `tests/fixtures/`.
//!
//! Run:
//! ```text
//! scripts/build.sh test clawft-graphify --features vision-extract
//! # or
//! cargo test -p clawft-graphify --features vision-extract --test vision_extract_e2e
//! ```

#![cfg(feature = "vision-extract")]

use std::path::{Path, PathBuf};

use clawft_graphify::entity::{EntityType, FileType};
use clawft_graphify::relationship::{Confidence, RelationType};
use clawft_graphify::vision_extract::{IMAGE_EXTENSIONS, extract_vision, is_image};

fn fixtures_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> PathBuf {
    fixtures_dir().join(name)
}

fn mock_architecture_response() -> String {
    // Prefer the on-disk mock fixture so the e2e path exercises file I/O
    // for both the image and the canned vision JSON.
    let path = fixture("architecture_diagram.mock.json");
    std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("missing mock response fixture {}: {e}", path.display()))
}

/// Full path: read PNG fixture → mock vision_complete → parse entities/edges.
#[tokio::test]
async fn e2e_vision_extract_architecture_diagram_with_mock_vision() {
    let image = fixture("architecture_diagram.png");
    assert!(
        image.is_file(),
        "fixture missing: {} (WEFT-381 requires tests/fixtures/)",
        image.display()
    );
    assert!(
        is_image(&image),
        "architecture_diagram.png must be classified as an image"
    );

    let image_bytes = std::fs::read(&image).expect("read fixture");
    assert!(!image_bytes.is_empty(), "fixture must be non-empty");
    // PNG magic
    assert_eq!(&image_bytes[0..8], b"\x89PNG\r\n\x1a\n");

    let expected_len = image_bytes.len();
    let mock = mock_architecture_response();

    let vision_complete = move |prompt: String, bytes: Vec<u8>, media_type: String| {
        let mock = mock.clone();
        async move {
            assert!(
                prompt.contains("Entities") || prompt.contains("entities"),
                "vision prompt should ask for entities"
            );
            assert_eq!(bytes.len(), expected_len, "image bytes must match fixture");
            assert_eq!(media_type, "image/png");
            Ok::<String, String>(mock)
        }
    };

    let result = extract_vision(&image, vision_complete)
        .await
        .expect("vision extract with mock must succeed");

    assert_eq!(result.entities.len(), 3, "entities: {:?}", result.entities);
    assert_eq!(
        result.relationships.len(),
        2,
        "relationships: {:?}",
        result.relationships
    );
    assert!(result.errors.is_empty());
    assert!(result.source_file.contains("architecture_diagram.png"));

    // All entities from images carry FileType::Image + source provenance.
    for e in &result.entities {
        assert_eq!(e.file_type, FileType::Image);
        assert_eq!(e.source_file.as_deref(), Some(result.source_file.as_str()));
    }

    let labels: Vec<&str> = result.entities.iter().map(|e| e.label.as_str()).collect();
    assert!(labels.contains(&"API Gateway"));
    assert!(labels.contains(&"Auth Service"));
    assert!(labels.contains(&"User DB"));

    let gateway = result
        .entities
        .iter()
        .find(|e| e.label == "API Gateway")
        .expect("API Gateway entity");
    assert_eq!(gateway.entity_type, EntityType::Service);
    assert_eq!(
        gateway.metadata.get("description").and_then(|v| v.as_str()),
        Some("External entry point")
    );

    // Edges resolve by name → EntityId and preserve confidence.
    let extracted = result
        .relationships
        .iter()
        .find(|r| r.confidence == Confidence::Extracted)
        .expect("EXTRACTED edge");
    assert_eq!(extracted.relation_type, RelationType::RelatedTo);
    assert!((extracted.weight - 1.0).abs() < f32::EPSILON);

    let inferred = result
        .relationships
        .iter()
        .find(|r| r.confidence == Confidence::Inferred)
        .expect("INFERRED edge");
    assert!((inferred.weight - 0.7).abs() < f32::EPSILON);
}

/// Markdown-fenced vision responses still parse (LLM often wraps JSON).
#[tokio::test]
async fn e2e_vision_extract_strips_markdown_fences() {
    let image = fixture("architecture_diagram.png");
    let fenced = r#"Here is the extraction:
```json
{"entities":[{"name":"BoxA","type":"concept","description":"left"},{"name":"BoxB","type":"concept","description":"right"}],"relationships":[{"source":"BoxA","target":"BoxB","relation":"related_to","confidence":"INFERRED"}]}
```
"#;

    let vision_complete = move |_prompt: String, _bytes: Vec<u8>, _media: String| async move {
        Ok::<String, String>(fenced.to_string())
    };

    let result = extract_vision(&image, vision_complete)
        .await
        .expect("fenced JSON must parse");
    assert_eq!(result.entities.len(), 2);
    assert_eq!(result.relationships.len(), 1);
}

/// Missing image path surfaces ExtractionFailed (not a panic).
#[tokio::test]
async fn e2e_vision_extract_missing_file_errors() {
    let missing = fixtures_dir().join("does-not-exist.png");
    let vision_complete =
        |_p: String, _b: Vec<u8>, _m: String| async { Ok::<String, String>("{}".into()) };

    let err = extract_vision(&missing, vision_complete)
        .await
        .expect_err("missing file must error");
    let msg = err.to_string();
    assert!(
        msg.contains("failed to read image")
            || msg.to_lowercase().contains("not found")
            || msg.contains("No such file"),
        "unexpected error: {msg}"
    );
}

/// Vision callback failure maps to LlmError.
#[tokio::test]
async fn e2e_vision_extract_llm_error_propagates() {
    let image = fixture("architecture_diagram.png");
    let vision_complete = |_p: String, _b: Vec<u8>, _m: String| async {
        Err::<String, String>("mock vision API down".into())
    };

    let err = extract_vision(&image, vision_complete)
        .await
        .expect_err("LLM failure must error");
    let msg = err.to_string();
    assert!(
        msg.contains("mock vision API down") || msg.to_lowercase().contains("llm"),
        "unexpected error: {msg}"
    );
}

/// Dangling relationship endpoints are dropped (name not in entity set).
#[tokio::test]
async fn e2e_vision_extract_skips_unknown_relationship_endpoints() {
    let image = fixture("architecture_diagram.png");
    let response = r#"{
      "entities": [{"name": "OnlyOne", "type": "concept", "description": "solo"}],
      "relationships": [
        {"source": "OnlyOne", "target": "Ghost", "relation": "related_to", "confidence": "INFERRED"},
        {"source": "Ghost", "target": "OnlyOne", "relation": "related_to", "confidence": "INFERRED"}
      ]
    }"#;

    let vision_complete = move |_p: String, _b: Vec<u8>, _m: String| {
        let response = response.to_string();
        async move { Ok::<String, String>(response) }
    };

    let result = extract_vision(&image, vision_complete)
        .await
        .expect("partial graph ok");
    assert_eq!(result.entities.len(), 1);
    assert!(
        result.relationships.is_empty(),
        "dangling edges must be dropped, got {:?}",
        result.relationships
    );
}

#[test]
fn fixture_image_extensions_recognized() {
    assert!(is_image(Path::new("x.png")));
    assert!(is_image(Path::new("x.SVG")));
    assert!(IMAGE_EXTENSIONS.contains(&"png"));
    assert!(!is_image(Path::new("notes.md")));
}
