//! LLM-based semantic extraction for documents, PDFs, and non-code files.
//!
//! Feature-gated behind `semantic-extract`. Takes a caller-supplied
//! `FnOnce(String) -> Future<Output = String>` callback that runs the
//! prompt against whatever LLM the host wires in (`clawft-llm`, a stub
//! for tests, a different provider, …). Keeping the provider out of the
//! crate's dep graph is intentional — see `.planning/graphify-rs/phase45-notes.md`
//! §3 (callback-based LLM invocation) and WEFT-383.

#[cfg(feature = "semantic-extract")]
use crate::GraphifyError;
#[cfg(feature = "semantic-extract")]
use crate::entity::{DomainTag, EntityId, EntityType, FileType};
#[cfg(feature = "semantic-extract")]
use crate::model::{Entity, ExtractionResult};
#[cfg(feature = "semantic-extract")]
use crate::relationship::{Confidence, RelationType, Relationship};

/// The structured prompt sent to the LLM for entity/relationship extraction.
#[cfg(feature = "semantic-extract")]
const EXTRACTION_PROMPT: &str = r#"You are a knowledge graph extraction engine. Analyze the following text and extract:

1. **Entities**: Named concepts, people, systems, components, locations, events, or other significant things.
2. **Relationships**: Directed connections between entities.

Return ONLY valid JSON in this exact format (no markdown fences):
{
  "entities": [
    {"name": "EntityName", "type": "concept", "description": "Brief description"}
  ],
  "relationships": [
    {"source": "EntityA", "target": "EntityB", "relation": "related_to", "confidence": "INFERRED"}
  ]
}

Entity types: module, class, function, service, concept, person, event, evidence,
location, organization, document, hypothesis, custom.

Relation types: contains, imports, calls, depends_on, related_to, witnessed_by,
found_at, contradicts, corroborates, precedes, documented_in, owned_by, custom.

Confidence: EXTRACTED (certain), INFERRED (likely), AMBIGUOUS (uncertain).

TEXT:
"#;

/// Result of parsing the LLM's JSON response.
#[cfg(feature = "semantic-extract")]
#[derive(Debug, serde::Deserialize)]
struct LlmExtractionOutput {
    #[serde(default)]
    entities: Vec<LlmEntity>,
    #[serde(default)]
    relationships: Vec<LlmRelationship>,
}

#[cfg(feature = "semantic-extract")]
#[derive(Debug, serde::Deserialize)]
struct LlmEntity {
    name: String,
    #[serde(rename = "type", default)]
    entity_type: String,
    #[serde(default)]
    description: String,
    #[serde(default)]
    iri: Option<String>,
}

#[cfg(feature = "semantic-extract")]
#[derive(Debug, serde::Deserialize)]
struct LlmRelationship {
    source: String,
    target: String,
    #[serde(default = "default_relation")]
    relation: String,
    #[serde(default = "default_confidence")]
    confidence: String,
}

#[cfg(feature = "semantic-extract")]
fn default_relation() -> String {
    "related_to".to_string()
}
#[cfg(feature = "semantic-extract")]
fn default_confidence() -> String {
    "INFERRED".to_string()
}

/// Extract entities and relationships from unstructured text using an LLM.
///
/// The `llm_complete` callback takes a prompt string and returns the LLM's
/// text response. This keeps the extraction logic decoupled from any specific
/// LLM client implementation.
///
/// # Arguments
/// * `text` - The unstructured text to analyze.
/// * `source_file` - Path to the source file (for provenance tracking).
/// * `llm_complete` - Async function that sends a prompt to the LLM and returns
///   the response text.
///
/// # Errors
/// Returns `GraphifyError::LlmError` if the LLM call fails or returns
/// unparseable JSON.
#[cfg(feature = "semantic-extract")]
pub async fn extract_semantic<F, Fut>(
    text: &str,
    source_file: &str,
    llm_complete: F,
) -> Result<ExtractionResult, GraphifyError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<String, String>>,
{
    // Truncate very long texts to stay within context windows.
    let truncated = if text.len() > 100_000 {
        &text[..100_000]
    } else {
        text
    };

    let prompt = format!("{EXTRACTION_PROMPT}{truncated}");

    let response = llm_complete(prompt)
        .await
        .map_err(GraphifyError::LlmError)?;

    // Try to parse JSON from the response, handling markdown fences.
    let json_str = extract_json_from_response(&response);

    let parsed: LlmExtractionOutput = serde_json::from_str(json_str).map_err(|e| {
        GraphifyError::LlmError(format!(
            "failed to parse LLM response as JSON: {e}\nResponse: {response}"
        ))
    })?;

    // Build a name -> EntityId lookup for relationship resolution.
    let mut name_to_id = std::collections::HashMap::new();
    let mut entities = Vec::with_capacity(parsed.entities.len());

    for llm_entity in &parsed.entities {
        let entity_type = parse_entity_type(&llm_entity.entity_type);
        let domain = domain_for_entity_type(&entity_type);
        let id = EntityId::new(&domain, &entity_type, &llm_entity.name, source_file);

        name_to_id.insert(llm_entity.name.clone(), id.clone());

        let mut metadata = serde_json::Map::new();
        if !llm_entity.description.is_empty() {
            metadata.insert(
                "description".to_string(),
                serde_json::Value::String(llm_entity.description.clone()),
            );
        }

        entities.push(Entity {
            id,
            entity_type,
            label: llm_entity.name.clone(),
            source_file: Some(source_file.to_string()),
            source_location: None,
            file_type: FileType::Document,
            metadata: serde_json::Value::Object(metadata),
            legacy_id: None,
            iri: llm_entity.iri.clone(),
        });
    }

    let mut relationships = Vec::with_capacity(parsed.relationships.len());

    for llm_rel in &parsed.relationships {
        let Some(source_id) = name_to_id.get(&llm_rel.source) else {
            continue;
        };
        let Some(target_id) = name_to_id.get(&llm_rel.target) else {
            continue;
        };

        let relation_type = parse_relation_type(&llm_rel.relation);
        let confidence =
            Confidence::from_str_loose(&llm_rel.confidence).unwrap_or(Confidence::Inferred);

        relationships.push(Relationship {
            source: source_id.clone(),
            target: target_id.clone(),
            relation_type,
            confidence,
            weight: confidence.to_weight(),
            source_file: Some(source_file.to_string()),
            source_location: None,
            metadata: serde_json::json!({}),
            embedding: None,
        });
    }

    Ok(ExtractionResult {
        source_file: source_file.to_string(),
        entities,
        relationships,
        hyperedges: Vec::new(),
        input_tokens: 0, // Caller should fill in from LLM usage data.
        output_tokens: 0,
        errors: Vec::new(),
    })
}

/// Strip markdown code fences from LLM response to get raw JSON.
#[cfg(feature = "semantic-extract")]
fn extract_json_from_response(response: &str) -> &str {
    let trimmed = response.trim();
    // Handle ```json ... ``` fences.
    if let Some(start) = trimmed.find('{')
        && let Some(end) = trimmed.rfind('}')
    {
        return &trimmed[start..=end];
    }
    trimmed
}

/// Map an LLM-provided entity type string to our `EntityType` enum.
#[cfg(feature = "semantic-extract")]
fn parse_entity_type(s: &str) -> EntityType {
    match s.to_lowercase().as_str() {
        "module" => EntityType::Module,
        "class" => EntityType::Class,
        "function" => EntityType::Function,
        "import" => EntityType::Import,
        "service" => EntityType::Service,
        "concept" => EntityType::Concept,
        "person" => EntityType::Person,
        "event" => EntityType::Event,
        "evidence" => EntityType::Evidence,
        "location" => EntityType::Location,
        "organization" => EntityType::Organization,
        "document" => EntityType::Document,
        "hypothesis" => EntityType::Hypothesis,
        "file" => EntityType::File,
        other => EntityType::Custom(other.to_string()),
    }
}

/// Determine the domain tag for an entity type.
#[cfg(feature = "semantic-extract")]
fn domain_for_entity_type(et: &EntityType) -> DomainTag {
    match et {
        EntityType::Person
        | EntityType::Event
        | EntityType::Evidence
        | EntityType::Location
        | EntityType::Organization
        | EntityType::Hypothesis
        | EntityType::Document
        | EntityType::PhysicalObject
        | EntityType::DigitalArtifact
        | EntityType::FinancialRecord
        | EntityType::Communication
        | EntityType::Timeline => DomainTag::Forensic,
        _ => DomainTag::Code,
    }
}

/// Map an LLM-provided relation string to our `RelationType` enum.
#[cfg(feature = "semantic-extract")]
fn parse_relation_type(s: &str) -> RelationType {
    match s.to_lowercase().replace('-', "_").as_str() {
        "contains" => RelationType::Contains,
        "imports" => RelationType::Imports,
        "imports_from" => RelationType::ImportsFrom,
        "calls" => RelationType::Calls,
        "depends_on" => RelationType::DependsOn,
        "implements" => RelationType::Implements,
        "extends" => RelationType::Extends,
        "related_to" => RelationType::RelatedTo,
        "witnessed_by" => RelationType::WitnessedBy,
        "found_at" => RelationType::FoundAt,
        "contradicts" => RelationType::Contradicts,
        "corroborates" => RelationType::Corroborates,
        "precedes" => RelationType::Precedes,
        "documented_in" => RelationType::DocumentedIn,
        "owned_by" => RelationType::OwnedBy,
        other => RelationType::Custom(other.to_string()),
    }
}

#[cfg(all(test, feature = "semantic-extract"))]
mod tests {
    use super::*;

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
        assert_eq!(parse_entity_type("concept"), EntityType::Concept);
        assert_eq!(parse_entity_type("PERSON"), EntityType::Person);
        assert_eq!(
            parse_entity_type("widget"),
            EntityType::Custom("widget".into())
        );
    }

    #[test]
    fn parse_relation_types() {
        assert_eq!(parse_relation_type("contains"), RelationType::Contains);
        assert_eq!(parse_relation_type("depends-on"), RelationType::DependsOn);
        assert_eq!(
            parse_relation_type("novel_relation"),
            RelationType::Custom("novel_relation".into())
        );
    }

    #[tokio::test]
    async fn extract_semantic_parses_llm_response() {
        let fake_llm = |_prompt: String| async {
            Ok::<_, String>(
                r#"{"entities":[{"name":"Auth","type":"module","description":"Authentication module"}],"relationships":[]}"#.to_string()
            )
        };

        let result = extract_semantic("test text", "test.md", fake_llm)
            .await
            .unwrap();
        assert_eq!(result.entities.len(), 1);
        assert_eq!(result.entities[0].label, "Auth");
    }
}
