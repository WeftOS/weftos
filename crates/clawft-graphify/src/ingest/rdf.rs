//! OWL/RDF ingestion (Turtle, JSON-LD) → graphify entities + relationships.
//!
//! # WEFT-358 / OG-2
//!
//! Parses RDF serializations via the Oxigraph stack (`oxrdfio` + `oxrdf`) and
//! maps triples into a [`KnowledgeGraph`].
//!
//! ## RDF triple → Entity / Relationship mapping
//!
//! | RDF pattern | Graphify mapping |
//! |-------------|------------------|
//! | `S rdf:type owl:Class` / `rdfs:Class` | Entity `S` with [`EntityType::Class`], `iri = S` |
//! | `S rdf:type owl:ObjectProperty` / `DatatypeProperty` / `rdf:Property` / `owl:AnnotationProperty` | Entity `S` with [`EntityType::Concept`], metadata `rdf_kind = "property"` |
//! | `S rdf:type T` (T not a meta-class above) | Ensure entities for `S` and `T`; edge `S --implements→ T` |
//! | `S rdfs:subClassOf O` | Edge `S --extends→ O` (VOWL export maps `Extends` → `rdfs:subClassOf`) |
//! | `S rdfs:subPropertyOf O` | Edge `S --extends→ O` |
//! | `S owl:equivalentClass O` / `owl:sameAs O` | Edge `S --related_to→ O` |
//! | `S rdfs:domain O` / `rdfs:range O` | Edge `S --related_to→ O` with metadata `owl_role` |
//! | `S rdfs:label "L"@lang` | Sets entity `label` (prefers `en`, else first label) |
//! | `S rdfs:comment "C"` | Entity metadata `description` |
//! | Other `S P O` with named `O` | Edge `S --Custom(localName(P))→ O` (or `RelatedTo` if empty) |
//! | Other `S P literal` | Entity metadata under local name of `P` |
//!
//! Blank nodes are included when they appear as subjects/objects of object
//! properties; IDs use the blank-node label string as the entity name.
//!
//! Ontology resources use [`DomainTag::Custom`]`(ONTOLOGY_DOMAIN)` = `0x22`.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use oxrdf::{NamedOrBlankNode, Term};
use oxrdfio::{JsonLdProfileSet, RdfFormat, RdfParser};

use crate::GraphifyError;
use crate::entity::{DomainTag, EntityId, EntityType, FileType};
use crate::model::{Entity, KnowledgeGraph};
use crate::relationship::{Confidence, RelationType, Relationship};

/// Domain byte for ontology / OWL-RDF resources (`DomainTag::Custom`).
pub const ONTOLOGY_DOMAIN: u8 = 0x22;

/// Supported RDF serialization formats for ontology ingest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdfIngestFormat {
    /// W3C Turtle.
    Turtle,
    /// W3C JSON-LD 1.1 (default profile set).
    JsonLd,
    /// N-Triples (useful for fixtures / intermediate dumps).
    NTriples,
}

impl RdfIngestFormat {
    /// Infer format from a file extension (case-insensitive).
    ///
    /// Recognized: `.ttl`/`.turtle`, `.jsonld`/`.json-ld`/`.json`, `.nt`.
    pub fn from_path(path: &Path) -> Option<Self> {
        let ext = path.extension()?.to_str()?.to_ascii_lowercase();
        match ext.as_str() {
            "ttl" | "turtle" => Some(Self::Turtle),
            "jsonld" | "json-ld" | "json" => Some(Self::JsonLd),
            "nt" => Some(Self::NTriples),
            _ => None,
        }
    }

    fn to_oxrdf(self) -> RdfFormat {
        match self {
            Self::Turtle => RdfFormat::Turtle,
            Self::JsonLd => RdfFormat::JsonLd {
                profile: JsonLdProfileSet::empty(),
            },
            Self::NTriples => RdfFormat::NTriples,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Turtle => "turtle",
            Self::JsonLd => "json-ld",
            Self::NTriples => "n-triples",
        }
    }
}

/// Options for RDF → graphify mapping.
#[derive(Debug, Clone)]
pub struct RdfIngestOptions {
    /// Logical source label stored on entities / relationships (`source_file`).
    pub source: String,
    /// Optional base IRI for resolving relative IRIs in Turtle.
    pub base_iri: Option<String>,
    /// When true, emit edges for arbitrary object-property triples (default true).
    pub include_object_properties: bool,
    /// When true, store non-label/comment literals as entity metadata (default true).
    pub include_literal_metadata: bool,
}

impl Default for RdfIngestOptions {
    fn default() -> Self {
        Self {
            source: "rdf".into(),
            base_iri: None,
            include_object_properties: true,
            include_literal_metadata: true,
        }
    }
}

impl RdfIngestOptions {
    /// Build options with a source path / label.
    pub fn with_source(source: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            ..Default::default()
        }
    }
}

/// Statistics from an RDF ingest run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RdfIngestStats {
    pub triples: usize,
    pub entities: usize,
    pub relationships: usize,
    pub format: String,
}

/// Parse RDF bytes into a knowledge graph.
pub fn parse_rdf(
    data: impl AsRef<[u8]>,
    format: RdfIngestFormat,
    options: &RdfIngestOptions,
) -> Result<(KnowledgeGraph, RdfIngestStats), GraphifyError> {
    let quads = parse_quads(data.as_ref(), format, options.base_iri.as_deref())?;
    let (kg, mut stats) = map_quads_to_graph(&quads, options);
    stats.format = format.as_str().into();
    Ok((kg, stats))
}

/// Parse a UTF-8 RDF string into a knowledge graph.
pub fn parse_rdf_str(
    text: &str,
    format: RdfIngestFormat,
    options: &RdfIngestOptions,
) -> Result<(KnowledgeGraph, RdfIngestStats), GraphifyError> {
    parse_rdf(text.as_bytes(), format, options)
}

/// Read an RDF file from disk (format inferred from extension unless forced).
pub fn parse_rdf_file(
    path: &Path,
    format: Option<RdfIngestFormat>,
    mut options: RdfIngestOptions,
) -> Result<(KnowledgeGraph, RdfIngestStats), GraphifyError> {
    let format = format
        .or_else(|| RdfIngestFormat::from_path(path))
        .ok_or_else(|| {
            GraphifyError::IngestError(format!(
                "cannot infer RDF format from path: {}",
                path.display()
            ))
        })?;
    if options.source == "rdf" {
        options.source = path.to_string_lossy().into_owned();
    }
    let data = std::fs::read(path)
        .map_err(|e| GraphifyError::IngestError(format!("read {}: {e}", path.display())))?;
    parse_rdf(&data, format, &options)
}

/// Turtle convenience entry point.
pub fn parse_turtle(
    text: &str,
    options: &RdfIngestOptions,
) -> Result<(KnowledgeGraph, RdfIngestStats), GraphifyError> {
    parse_rdf_str(text, RdfIngestFormat::Turtle, options)
}

/// JSON-LD convenience entry point.
pub fn parse_json_ld(
    text: &str,
    options: &RdfIngestOptions,
) -> Result<(KnowledgeGraph, RdfIngestStats), GraphifyError> {
    parse_rdf_str(text, RdfIngestFormat::JsonLd, options)
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct SimpleTriple {
    subject: ResourceKey,
    predicate: String,
    object: ObjectValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ResourceKey {
    Iri(String),
    Blank(String),
}

impl ResourceKey {
    fn iri_string(&self) -> String {
        match self {
            Self::Iri(s) => s.clone(),
            Self::Blank(s) => format!("_:{s}"),
        }
    }

    fn display_name(&self) -> String {
        match self {
            Self::Iri(s) => local_name(s),
            Self::Blank(s) => format!("_:{s}"),
        }
    }
}

#[derive(Debug, Clone)]
enum ObjectValue {
    Resource(ResourceKey),
    Literal { value: String, lang: Option<String> },
}

fn parse_quads(
    data: &[u8],
    format: RdfIngestFormat,
    base_iri: Option<&str>,
) -> Result<Vec<SimpleTriple>, GraphifyError> {
    let mut parser = RdfParser::from_format(format.to_oxrdf());
    if let Some(base) = base_iri {
        parser = parser
            .with_base_iri(base)
            .map_err(|e| GraphifyError::IngestError(format!("invalid base IRI '{base}': {e}")))?;
    }

    let mut triples = Vec::new();
    for quad in parser.for_reader(std::io::Cursor::new(data)) {
        let quad = quad.map_err(|e| GraphifyError::IngestError(format!("RDF parse: {e}")))?;
        let subject = match subject_key(&quad.subject) {
            Some(s) => s,
            None => continue,
        };
        let predicate = quad.predicate.as_str().to_string();
        let object = match term_value(&quad.object) {
            Some(o) => o,
            None => continue,
        };
        triples.push(SimpleTriple {
            subject,
            predicate,
            object,
        });
    }
    Ok(triples)
}

fn subject_key(subject: &NamedOrBlankNode) -> Option<ResourceKey> {
    match subject {
        NamedOrBlankNode::NamedNode(n) => Some(ResourceKey::Iri(n.as_str().to_string())),
        NamedOrBlankNode::BlankNode(b) => Some(ResourceKey::Blank(b.as_str().to_string())),
    }
}

fn term_value(term: &Term) -> Option<ObjectValue> {
    match term {
        Term::NamedNode(n) => Some(ObjectValue::Resource(ResourceKey::Iri(
            n.as_str().to_string(),
        ))),
        Term::BlankNode(b) => Some(ObjectValue::Resource(ResourceKey::Blank(
            b.as_str().to_string(),
        ))),
        Term::Literal(lit) => Some(ObjectValue::Literal {
            value: lit.value().to_string(),
            lang: lit.language().map(|l| l.to_string()),
        }),
        // Skip triple terms / other RDF-star if present.
        #[allow(unreachable_patterns)]
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Mapping
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ResourceKind {
    Class,
    Property,
    Individual,
    Unknown,
}

struct PendingEntity {
    kind: ResourceKind,
    label: Option<String>,
    label_lang: Option<String>,
    description: Option<String>,
    types: Vec<ResourceKey>,
    metadata: serde_json::Map<String, serde_json::Value>,
}

fn map_quads_to_graph(
    triples: &[SimpleTriple],
    options: &RdfIngestOptions,
) -> (KnowledgeGraph, RdfIngestStats) {
    let mut pending: HashMap<ResourceKey, PendingEntity> = HashMap::new();
    let mut edges: Vec<(ResourceKey, ResourceKey, RelationType, serde_json::Value)> = Vec::new();

    let ensure = |map: &mut HashMap<ResourceKey, PendingEntity>, key: &ResourceKey| {
        if !map.contains_key(key) {
            map.insert(
                key.clone(),
                PendingEntity {
                    kind: ResourceKind::Unknown,
                    label: None,
                    label_lang: None,
                    description: None,
                    types: Vec::new(),
                    metadata: serde_json::Map::new(),
                },
            );
        }
    };

    for t in triples {
        ensure(&mut pending, &t.subject);

        // rdf:type
        if t.predicate == RDF_TYPE {
            if let ObjectValue::Resource(obj) = &t.object {
                ensure(&mut pending, obj);
                let subj = pending.get_mut(&t.subject).expect("ensured");
                if is_class_meta(obj) {
                    subj.kind = ResourceKind::Class;
                } else if is_property_meta(obj) {
                    subj.kind = ResourceKind::Property;
                } else {
                    if subj.kind == ResourceKind::Unknown {
                        subj.kind = ResourceKind::Individual;
                    }
                    subj.types.push(obj.clone());
                    edges.push((
                        t.subject.clone(),
                        obj.clone(),
                        RelationType::Implements,
                        serde_json::json!({"owl_role": "rdf:type"}),
                    ));
                }
            }
            continue;
        }

        // rdfs:subClassOf / rdfs:subPropertyOf → Extends
        if t.predicate == RDFS_SUB_CLASS_OF || t.predicate == RDFS_SUB_PROPERTY_OF {
            if let ObjectValue::Resource(obj) = &t.object {
                ensure(&mut pending, obj);
                if t.predicate == RDFS_SUB_CLASS_OF {
                    if let Some(e) = pending.get_mut(&t.subject) {
                        e.kind = ResourceKind::Class;
                    }
                    if let Some(e) = pending.get_mut(obj)
                        && e.kind == ResourceKind::Unknown
                    {
                        e.kind = ResourceKind::Class;
                    }
                } else if let Some(e) = pending.get_mut(&t.subject) {
                    e.kind = ResourceKind::Property;
                }
                edges.push((
                    t.subject.clone(),
                    obj.clone(),
                    RelationType::Extends,
                    serde_json::json!({
                        "owl_role": if t.predicate == RDFS_SUB_CLASS_OF {
                            "rdfs:subClassOf"
                        } else {
                            "rdfs:subPropertyOf"
                        }
                    }),
                ));
            }
            continue;
        }

        // owl:equivalentClass / owl:sameAs
        if t.predicate == OWL_EQUIVALENT_CLASS || t.predicate == OWL_SAME_AS {
            if let ObjectValue::Resource(obj) = &t.object {
                ensure(&mut pending, obj);
                edges.push((
                    t.subject.clone(),
                    obj.clone(),
                    RelationType::RelatedTo,
                    serde_json::json!({
                        "owl_role": if t.predicate == OWL_EQUIVALENT_CLASS {
                            "owl:equivalentClass"
                        } else {
                            "owl:sameAs"
                        }
                    }),
                ));
            }
            continue;
        }

        // rdfs:domain / rdfs:range
        if t.predicate == RDFS_DOMAIN || t.predicate == RDFS_RANGE {
            if let ObjectValue::Resource(obj) = &t.object {
                ensure(&mut pending, obj);
                if let Some(e) = pending.get_mut(&t.subject) {
                    e.kind = ResourceKind::Property;
                }
                if let Some(e) = pending.get_mut(obj)
                    && e.kind == ResourceKind::Unknown
                {
                    e.kind = ResourceKind::Class;
                }
                let role = if t.predicate == RDFS_DOMAIN {
                    "rdfs:domain"
                } else {
                    "rdfs:range"
                };
                edges.push((
                    t.subject.clone(),
                    obj.clone(),
                    RelationType::RelatedTo,
                    serde_json::json!({"owl_role": role}),
                ));
            }
            continue;
        }

        // rdfs:label
        if t.predicate == RDFS_LABEL {
            if let ObjectValue::Literal { value, lang } = &t.object {
                let e = pending.get_mut(&t.subject).expect("ensured");
                let prefer = match (&e.label_lang, lang) {
                    (None, _) => true,
                    (Some(_), Some(l)) if l == "en" && e.label_lang.as_deref() != Some("en") => {
                        true
                    }
                    _ => false,
                };
                if prefer {
                    e.label = Some(value.clone());
                    e.label_lang = lang.clone();
                }
            }
            continue;
        }

        // rdfs:comment
        if t.predicate == RDFS_COMMENT {
            if let ObjectValue::Literal { value, .. } = &t.object {
                let e = pending.get_mut(&t.subject).expect("ensured");
                if e.description.is_none() {
                    e.description = Some(value.clone());
                }
            }
            continue;
        }

        // Generic object properties / literals
        match &t.object {
            ObjectValue::Resource(obj) if options.include_object_properties => {
                ensure(&mut pending, obj);
                let local = local_name(&t.predicate);
                let rel = if local.is_empty() {
                    RelationType::RelatedTo
                } else {
                    RelationType::Custom(local)
                };
                edges.push((
                    t.subject.clone(),
                    obj.clone(),
                    rel,
                    serde_json::json!({"predicate_iri": t.predicate}),
                ));
            }
            ObjectValue::Literal { value, lang } if options.include_literal_metadata => {
                let e = pending.get_mut(&t.subject).expect("ensured");
                let key = local_name(&t.predicate);
                if key.is_empty() {
                    continue;
                }
                let v = if let Some(l) = lang {
                    serde_json::json!({"value": value, "lang": l})
                } else {
                    serde_json::Value::String(value.clone())
                };
                e.metadata.insert(key, v);
            }
            _ => {}
        }
    }

    // Build KnowledgeGraph
    let mut kg = KnowledgeGraph::new();
    let mut id_by_key: HashMap<ResourceKey, EntityId> = HashMap::new();
    let domain = DomainTag::Custom(ONTOLOGY_DOMAIN);

    for (key, pe) in &pending {
        let entity_type = match pe.kind {
            ResourceKind::Class => EntityType::Class,
            ResourceKind::Property => EntityType::Concept,
            ResourceKind::Individual => EntityType::Concept,
            ResourceKind::Unknown => EntityType::Concept,
        };
        let name = key.iri_string();
        let id = EntityId::new(&domain, &entity_type, &name, &options.source);
        let label = pe.label.clone().unwrap_or_else(|| key.display_name());

        let mut metadata = pe.metadata.clone();
        if let Some(desc) = &pe.description {
            metadata.insert(
                "description".into(),
                serde_json::Value::String(desc.clone()),
            );
        }
        metadata.insert(
            "rdf_kind".into(),
            serde_json::Value::String(
                match pe.kind {
                    ResourceKind::Class => "class",
                    ResourceKind::Property => "property",
                    ResourceKind::Individual => "individual",
                    ResourceKind::Unknown => "resource",
                }
                .into(),
            ),
        );
        if !pe.types.is_empty() {
            metadata.insert(
                "rdf_types".into(),
                serde_json::Value::Array(
                    pe.types
                        .iter()
                        .map(|t| serde_json::Value::String(t.iri_string()))
                        .collect(),
                ),
            );
        }

        let iri = match key {
            ResourceKey::Iri(s) => Some(s.clone()),
            ResourceKey::Blank(_) => None,
        };

        kg.add_entity(Entity {
            id: id.clone(),
            entity_type,
            label,
            iri,
            source_file: Some(options.source.clone()),
            source_location: None,
            file_type: FileType::Document,
            metadata: serde_json::Value::Object(metadata),
            legacy_id: Some(name),
        });
        id_by_key.insert(key.clone(), id);
    }

    let mut seen_edges: HashSet<(EntityId, EntityId, String)> = HashSet::new();
    let mut rel_count = 0usize;
    for (src, tgt, rel_type, meta) in edges {
        let Some(source) = id_by_key.get(&src) else {
            continue;
        };
        let Some(target) = id_by_key.get(&tgt) else {
            continue;
        };
        let type_key = rel_type.to_string();
        if !seen_edges.insert((source.clone(), target.clone(), type_key)) {
            continue;
        }
        if kg
            .add_relationship(Relationship {
                source: source.clone(),
                target: target.clone(),
                relation_type: rel_type,
                confidence: Confidence::Extracted,
                weight: Confidence::Extracted.to_weight(),
                source_file: Some(options.source.clone()),
                source_location: None,
                metadata: meta,
                embedding: None,
            })
            .is_some()
        {
            rel_count += 1;
        }
    }

    let stats = RdfIngestStats {
        triples: triples.len(),
        entities: kg.entity_count(),
        relationships: rel_count,
        format: String::new(),
    };
    (kg, stats)
}

// ---------------------------------------------------------------------------
// Vocabulary helpers
// ---------------------------------------------------------------------------

const RDF_TYPE: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#type";
const RDFS_SUB_CLASS_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subClassOf";
const RDFS_SUB_PROPERTY_OF: &str = "http://www.w3.org/2000/01/rdf-schema#subPropertyOf";
const RDFS_LABEL: &str = "http://www.w3.org/2000/01/rdf-schema#label";
const RDFS_COMMENT: &str = "http://www.w3.org/2000/01/rdf-schema#comment";
const RDFS_DOMAIN: &str = "http://www.w3.org/2000/01/rdf-schema#domain";
const RDFS_RANGE: &str = "http://www.w3.org/2000/01/rdf-schema#range";
const OWL_EQUIVALENT_CLASS: &str = "http://www.w3.org/2002/07/owl#equivalentClass";
const OWL_SAME_AS: &str = "http://www.w3.org/2002/07/owl#sameAs";

fn is_class_meta(key: &ResourceKey) -> bool {
    match key {
        ResourceKey::Iri(s) => {
            s == "http://www.w3.org/2002/07/owl#Class"
                || s == "http://www.w3.org/2000/01/rdf-schema#Class"
        }
        ResourceKey::Blank(_) => false,
    }
}

fn is_property_meta(key: &ResourceKey) -> bool {
    match key {
        ResourceKey::Iri(s) => {
            matches!(
                s.as_str(),
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#Property"
                    | "http://www.w3.org/2002/07/owl#ObjectProperty"
                    | "http://www.w3.org/2002/07/owl#DatatypeProperty"
                    | "http://www.w3.org/2002/07/owl#AnnotationProperty"
                    | "http://www.w3.org/2002/07/owl#FunctionalProperty"
                    | "http://www.w3.org/2002/07/owl#InverseFunctionalProperty"
            )
        }
        ResourceKey::Blank(_) => false,
    }
}

/// Extract a human-friendly local name from an IRI or CURIE-like string.
pub fn local_name(iri: &str) -> String {
    if let Some(i) = iri.rfind('#') {
        let rest = &iri[i + 1..];
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    if let Some(i) = iri.rfind('/') {
        let rest = &iri[i + 1..];
        if !rest.is_empty() {
            return rest.to_string();
        }
    }
    iri.to_string()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::export::vowl::to_vowl_json;
    use crate::topology::TopologySchema;

    const TINY_TURTLE: &str = r##"
@prefix rdf:  <http://www.w3.org/1999/02/22-rdf-syntax-ns#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix owl:  <http://www.w3.org/2002/07/owl#> .
@prefix ex:   <http://example.org/onto#> .

ex:Animal a owl:Class ;
    rdfs:label "Animal"@en ;
    rdfs:comment "A living organism" .

ex:Dog a owl:Class ;
    rdfs:label "Dog"@en ;
    rdfs:subClassOf ex:Animal .

ex:hasOwner a owl:ObjectProperty ;
    rdfs:label "has owner"@en ;
    rdfs:domain ex:Dog ;
    rdfs:range ex:Person .

ex:Person a owl:Class ;
    rdfs:label "Person"@en .

ex:fido a ex:Dog ;
    rdfs:label "Fido"@en ;
    ex:hasOwner ex:alice .

ex:alice a ex:Person ;
    rdfs:label "Alice"@en .
"##;

    const TINY_JSON_LD: &str = r##"{
  "@context": {
    "rdf": "http://www.w3.org/1999/02/22-rdf-syntax-ns#",
    "rdfs": "http://www.w3.org/2000/01/rdf-schema#",
    "owl": "http://www.w3.org/2002/07/owl#",
    "ex": "http://example.org/onto#",
    "label": "rdfs:label",
    "comment": "rdfs:comment",
    "subClassOf": { "@id": "rdfs:subClassOf", "@type": "@id" },
    "domain": { "@id": "rdfs:domain", "@type": "@id" },
    "range": { "@id": "rdfs:range", "@type": "@id" },
    "hasOwner": { "@id": "ex:hasOwner", "@type": "@id" }
  },
  "@graph": [
    {
      "@id": "ex:Animal",
      "@type": "owl:Class",
      "label": "Animal",
      "comment": "A living organism"
    },
    {
      "@id": "ex:Dog",
      "@type": "owl:Class",
      "label": "Dog",
      "subClassOf": "ex:Animal"
    },
    {
      "@id": "ex:Person",
      "@type": "owl:Class",
      "label": "Person"
    },
    {
      "@id": "ex:hasOwner",
      "@type": "owl:ObjectProperty",
      "label": "has owner",
      "domain": "ex:Dog",
      "range": "ex:Person"
    },
    {
      "@id": "ex:fido",
      "@type": "ex:Dog",
      "label": "Fido",
      "hasOwner": "ex:alice"
    },
    {
      "@id": "ex:alice",
      "@type": "ex:Person",
      "label": "Alice"
    }
  ]
}"##;

    fn opts() -> RdfIngestOptions {
        RdfIngestOptions::with_source("fixture.ttl")
    }

    fn find_by_iri<'a>(kg: &'a KnowledgeGraph, iri: &str) -> Option<&'a Entity> {
        kg.entities().find(|e| e.iri.as_deref() == Some(iri))
    }

    fn has_edge(kg: &KnowledgeGraph, src_iri: &str, tgt_iri: &str, rel: RelationType) -> bool {
        let Some(src) = find_by_iri(kg, src_iri) else {
            return false;
        };
        let Some(tgt) = find_by_iri(kg, tgt_iri) else {
            return false;
        };
        kg.edges()
            .any(|(s, t, r)| s.id == src.id && t.id == tgt.id && r.relation_type == rel)
    }

    #[test]
    fn parse_turtle_builds_classes_and_subclass() {
        let (kg, stats) = parse_turtle(TINY_TURTLE, &opts()).expect("turtle parse");
        assert!(stats.triples > 0, "expected triples");
        assert!(
            stats.entities >= 5,
            "Animal, Dog, Person, hasOwner, fido, alice"
        );

        let dog = find_by_iri(&kg, "http://example.org/onto#Dog").expect("Dog class");
        assert_eq!(dog.entity_type, EntityType::Class);
        assert_eq!(dog.label, "Dog");
        assert_eq!(
            dog.metadata.get("description").and_then(|v| v.as_str()),
            None
        );

        let animal = find_by_iri(&kg, "http://example.org/onto#Animal").expect("Animal");
        assert_eq!(animal.label, "Animal");
        assert_eq!(
            animal.metadata.get("description").and_then(|v| v.as_str()),
            Some("A living organism")
        );

        assert!(
            has_edge(
                &kg,
                "http://example.org/onto#Dog",
                "http://example.org/onto#Animal",
                RelationType::Extends
            ),
            "Dog subClassOf Animal → Extends"
        );
    }

    #[test]
    fn parse_turtle_individuals_and_object_props() {
        let (kg, _) = parse_turtle(TINY_TURTLE, &opts()).unwrap();

        let fido = find_by_iri(&kg, "http://example.org/onto#fido").expect("fido");
        assert_eq!(fido.entity_type, EntityType::Concept);
        assert_eq!(fido.label, "Fido");
        assert_eq!(
            fido.metadata.get("rdf_kind").and_then(|v| v.as_str()),
            Some("individual")
        );

        assert!(has_edge(
            &kg,
            "http://example.org/onto#fido",
            "http://example.org/onto#Dog",
            RelationType::Implements
        ));
        assert!(has_edge(
            &kg,
            "http://example.org/onto#fido",
            "http://example.org/onto#alice",
            RelationType::Custom("hasOwner".into())
        ));

        let prop = find_by_iri(&kg, "http://example.org/onto#hasOwner").expect("hasOwner");
        assert_eq!(prop.entity_type, EntityType::Concept);
        assert_eq!(
            prop.metadata.get("rdf_kind").and_then(|v| v.as_str()),
            Some("property")
        );
    }

    #[test]
    fn parse_json_ld_matches_turtle_structure() {
        let (ttl_kg, _) = parse_turtle(TINY_TURTLE, &opts()).unwrap();
        let (ld_kg, stats) = parse_json_ld(
            TINY_JSON_LD,
            &RdfIngestOptions::with_source("fixture.jsonld"),
        )
        .expect("json-ld parse");

        assert_eq!(stats.format, "json-ld");
        assert!(find_by_iri(&ld_kg, "http://example.org/onto#Dog").is_some());
        assert!(has_edge(
            &ld_kg,
            "http://example.org/onto#Dog",
            "http://example.org/onto#Animal",
            RelationType::Extends
        ));
        assert!(has_edge(
            &ld_kg,
            "http://example.org/onto#fido",
            "http://example.org/onto#alice",
            RelationType::Custom("hasOwner".into())
        ));

        // Class counts should match between serializations for the fixture.
        let ttl_classes = ttl_kg
            .entities()
            .filter(|e| e.entity_type == EntityType::Class)
            .count();
        let ld_classes = ld_kg
            .entities()
            .filter(|e| e.entity_type == EntityType::Class)
            .count();
        assert_eq!(ttl_classes, ld_classes);
        assert!(ttl_classes >= 3);
    }

    #[test]
    fn format_from_path() {
        assert_eq!(
            RdfIngestFormat::from_path(Path::new("onto.ttl")),
            Some(RdfIngestFormat::Turtle)
        );
        assert_eq!(
            RdfIngestFormat::from_path(Path::new("onto.JSONLD")),
            Some(RdfIngestFormat::JsonLd)
        );
        assert_eq!(
            RdfIngestFormat::from_path(Path::new("data.nt")),
            Some(RdfIngestFormat::NTriples)
        );
        assert_eq!(RdfIngestFormat::from_path(Path::new("x.py")), None);
    }

    #[test]
    fn local_name_helpers() {
        assert_eq!(local_name("http://example.org/onto#Dog"), "Dog");
        assert_eq!(local_name("http://example.org/onto/Dog"), "Dog");
        assert_eq!(local_name("Dog"), "Dog");
    }

    #[test]
    fn invalid_turtle_errors() {
        let err = parse_turtle("@prefix broken", &opts()).unwrap_err();
        assert!(
            err.to_string().contains("RDF parse") || err.to_string().contains("ingest"),
            "got: {err}"
        );
    }

    #[test]
    fn roundtrip_rdf_to_vowl_ontology_assertions() {
        let (kg, _) = parse_turtle(TINY_TURTLE, &opts()).unwrap();
        let schema = TopologySchema::from_yaml(
            r##"
name: ontology
label: "Example Ontology"
version: "1.0.0"
iri: "http://example.org/onto#"
nodes:
  class:
    geometry: force
  concept:
    geometry: force
  "*":
    geometry: force
edges:
  - type: extends
    from: class
    to: class
  - type: implements
    from: concept
    to: class
  - type: hasOwner
    from: concept
    to: concept
"##,
        )
        .unwrap();

        let vowl = to_vowl_json(&kg, &schema).expect("vowl export");

        assert!(vowl["header"].is_object());
        let classes = vowl["class"].as_array().expect("class array");
        assert!(
            classes.len() >= 5,
            "expected ontology classes + individuals in VOWL, got {}",
            classes.len()
        );
        assert_eq!(vowl["metrics"]["classCount"], classes.len());

        // Labels present for known resources.
        let attrs = vowl["classAttribute"].as_array().unwrap();
        let labels: Vec<String> = attrs
            .iter()
            .filter_map(|a| a["label"]["en"].as_str().map(|s| s.to_string()))
            .collect();
        for expected in ["Animal", "Dog", "Person", "Fido", "Alice"] {
            assert!(
                labels.iter().any(|l| l == expected),
                "missing VOWL label {expected}, have {labels:?}"
            );
        }

        // IRIs preserved on class attributes where set.
        let iris: Vec<&str> = attrs.iter().filter_map(|a| a["iri"].as_str()).collect();
        assert!(iris.contains(&"http://example.org/onto#Dog"));
        assert!(iris.contains(&"http://example.org/onto#Animal"));

        // subClassOf mapping → Extends → rdfs:subClassOf in VOWL property types.
        let props = vowl["property"].as_array().unwrap();
        let has_subclass = props.iter().any(|p| p["type"] == "rdfs:subClassOf");
        assert!(
            has_subclass,
            "expected at least one rdfs:subClassOf property from Extends edges"
        );
    }

    #[test]
    fn parse_rdf_file_from_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mini.ttl");
        std::fs::write(
            &path,
            r#"
@prefix owl: <http://www.w3.org/2002/07/owl#> .
@prefix rdfs: <http://www.w3.org/2000/01/rdf-schema#> .
@prefix ex: <http://example.org#> .
ex:Thing a owl:Class ; rdfs:label "Thing" .
"#,
        )
        .unwrap();

        let (kg, stats) = parse_rdf_file(&path, None, RdfIngestOptions::default()).unwrap();
        assert_eq!(stats.format, "turtle");
        assert!(find_by_iri(&kg, "http://example.org#Thing").is_some());
    }
}
