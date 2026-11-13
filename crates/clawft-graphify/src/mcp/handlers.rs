//! Graphify MCP tool handlers (WEFT-369).
//!
//! Pure (mostly sync) operations used by the stdio MCP server. Kept free of
//! protocol concerns so unit tests can call them without a transport.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::GraphifyError;
use crate::analyze;
use crate::build::build_from_json;
use crate::entity::{DomainTag, EntityId, EntityType, FileType};
use crate::export::{self, ExportFormat};
use crate::extract::detect;
use crate::model::{Entity, ExtractionResult, KnowledgeGraph};
use crate::pipeline::{Pipeline, PipelineConfig};
use crate::relationship::{Confidence, RelationType, Relationship};

use super::tools::{TOOL_DIFF, TOOL_EXPORT, TOOL_INGEST, TOOL_QUERY};

// ---------------------------------------------------------------------------
// Config / result
// ---------------------------------------------------------------------------

/// Runtime configuration for the graphify MCP server and tools.
#[derive(Debug, Clone)]
pub struct McpConfig {
    /// Working directory for relative paths (default: cwd).
    pub work_dir: PathBuf,
    /// Default graph path relative to `work_dir`.
    pub default_graph: PathBuf,
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            work_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            default_graph: PathBuf::from("graphify-out/graph.json"),
        }
    }
}

impl McpConfig {
    /// Resolve a user path relative to `work_dir` when not absolute.
    pub fn resolve(&self, path: impl AsRef<Path>) -> PathBuf {
        let p = path.as_ref();
        if p.is_absolute() {
            p.to_path_buf()
        } else {
            self.work_dir.join(p)
        }
    }
}

/// Result of a tool call (text body + error flag).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallResult {
    pub text: String,
    pub is_error: bool,
}

impl CallResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: false,
        }
    }

    pub fn err(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            is_error: true,
        }
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

/// Route a tool name + JSON args to the matching handler.
pub fn dispatch_tool(config: &McpConfig, name: &str, args: &Value) -> CallResult {
    match name {
        TOOL_QUERY => {
            let question = args
                .get("question")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if question.trim().is_empty() {
                return CallResult::err("graphify_query requires 'question'");
            }
            let graph = args
                .get("graph")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| config.default_graph.clone());
            let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("bfs");
            let depth = args
                .get("depth")
                .and_then(|v| v.as_u64())
                .unwrap_or(3)
                .clamp(1, 6) as usize;
            let limit = args
                .get("limit")
                .and_then(|v| v.as_u64())
                .unwrap_or(10)
                .clamp(1, 100) as usize;
            match query_graph(config, &question, &graph, mode, depth, limit) {
                Ok(s) => CallResult::ok(s),
                Err(e) => CallResult::err(e.to_string()),
            }
        }
        TOOL_INGEST => {
            let target = match args.get("target").and_then(|v| v.as_str()) {
                Some(t) if !t.is_empty() => t,
                _ => return CallResult::err("graphify_ingest requires 'target'"),
            };
            let output = args
                .get("output")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            let contributor = args.get("contributor").and_then(|v| v.as_str());
            match ingest_target(config, target, output.as_deref(), contributor) {
                Ok(s) => CallResult::ok(s),
                Err(e) => CallResult::err(e.to_string()),
            }
        }
        TOOL_EXPORT => {
            let format = match args.get("format").and_then(|v| v.as_str()) {
                Some(f) if !f.is_empty() => f,
                _ => return CallResult::err("graphify_export requires 'format'"),
            };
            let graph = args
                .get("graph")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| config.default_graph.clone());
            let output = args
                .get("output")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            match export_graph(config, format, &graph, output.as_deref()) {
                Ok(s) => CallResult::ok(s),
                Err(e) => CallResult::err(e.to_string()),
            }
        }
        TOOL_DIFF => {
            let old = args
                .get("old")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| PathBuf::from("graphify-out/graph.json.bak"));
            let current = args
                .get("current")
                .and_then(|v| v.as_str())
                .map(PathBuf::from)
                .unwrap_or_else(|| config.default_graph.clone());
            match graph_diff_paths(config, &old, &current) {
                Ok(s) => CallResult::ok(s),
                Err(e) => CallResult::err(e.to_string()),
            }
        }
        other => CallResult::err(format!("unknown tool: {other}")),
    }
}

// ---------------------------------------------------------------------------
// Load helpers
// ---------------------------------------------------------------------------

fn load_graph_json(path: &Path) -> Result<KnowledgeGraph, GraphifyError> {
    if !path.exists() {
        return Err(GraphifyError::IngestError(format!(
            "graph file not found: {}",
            path.display()
        )));
    }
    let data = std::fs::read_to_string(path)
        .map_err(|e| GraphifyError::CacheError(format!("read {}: {e}", path.display())))?;
    let json_value: Value = serde_json::from_str(&data)?;
    let json_for_build = normalize_graph_json(json_value);
    build_from_json(&json_for_build)
}

/// Remap legacy `"links"` key to `"edges"` expected by `build_from_json`.
fn normalize_graph_json(mut value: Value) -> Value {
    if value.get("edges").is_none()
        && let Some(links) = value.get("links").cloned()
        && let Some(obj) = value.as_object_mut()
    {
        obj.insert("edges".into(), links);
    }
    value
}

// ---------------------------------------------------------------------------
// query
// ---------------------------------------------------------------------------

/// Keyword score + optional BFS/DFS neighbourhood expansion.
pub fn query_graph(
    config: &McpConfig,
    question: &str,
    graph_path: &Path,
    mode: &str,
    depth: usize,
    limit: usize,
) -> Result<String, GraphifyError> {
    let path = config.resolve(graph_path);
    let kg = load_graph_json(&path)?;
    if kg.node_count() == 0 {
        return Ok("No nodes found in graph.".into());
    }

    let terms: Vec<String> = question
        .split_whitespace()
        .filter(|t| t.len() > 2)
        .map(|t| t.to_lowercase())
        .collect();
    if terms.is_empty() {
        return Ok("Query too short (all terms <= 2 chars).".into());
    }

    let mut scored: Vec<(EntityId, f64, String, String)> = Vec::new();
    for id in kg.entity_ids() {
        if let Some(entity) = kg.entity(id) {
            let label = entity.label.to_lowercase();
            let source = entity.source_file.as_deref().unwrap_or("").to_lowercase();
            let score: f64 = terms
                .iter()
                .map(|t| {
                    (if label.contains(t.as_str()) { 1.0 } else { 0.0 })
                        + (if source.contains(t.as_str()) {
                            0.5
                        } else {
                            0.0
                        })
                })
                .sum();
            if score > 0.0 {
                scored.push((
                    id.clone(),
                    score,
                    entity.label.clone(),
                    entity.source_file.clone().unwrap_or_default(),
                ));
            }
        }
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));

    if scored.is_empty() {
        return Ok("No matching nodes found.".into());
    }

    // WEFT-376: graph-aware re-rank of keyword hits (degree, community, hops, relations).
    let communities = crate::cluster::cluster(&kg);
    let vector_hits: Vec<crate::graph_rerank::VectorHit> = scored
        .iter()
        .map(|(id, score, label, src)| {
            crate::graph_rerank::VectorHit::with_metadata(
                id.to_hex(),
                *score,
                serde_json::json!({ "label": label, "source_file": src }),
            )
        })
        .collect();
    let reranked = crate::graph_rerank::graph_rerank_top_k(
        &vector_hits,
        &kg,
        Some(&communities),
        &crate::graph_rerank::GraphRerankConfig::default(),
        limit,
    );

    let mut out = String::new();
    out.push_str(&format!(
        "Query: {question}\nGraph: {}\nMode: {mode}, depth: {depth}\n\nMatches (graph re-rank):\n",
        path.display()
    ));
    for (i, hit) in reranked.iter().enumerate() {
        let label = hit
            .metadata
            .as_ref()
            .and_then(|m| m.get("label"))
            .and_then(|v| v.as_str())
            .unwrap_or(&hit.id);
        let src = hit
            .metadata
            .as_ref()
            .and_then(|m| m.get("source_file"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let hop = hit
            .hop_distance
            .map(|h| h.to_string())
            .unwrap_or_else(|| "∞".into());
        out.push_str(&format!(
            "  {}. [{:.3}] {label} (src={src}, deg={}, hop={hop})\n",
            i + 1,
            hit.score,
            hit.degree,
        ));
    }

    let mode_l = mode.to_lowercase();
    if mode_l == "none" {
        return Ok(out);
    }

    // Expand from top-3 seeds (post re-rank order).
    let seeds: Vec<EntityId> = reranked
        .iter()
        .take(3)
        .filter_map(|h| EntityId::from_hex(&h.id))
        .collect();
    let mut seen: HashSet<EntityId> = HashSet::new();
    let mut neighbourhood: Vec<String> = Vec::new();

    for seed in &seeds {
        let visited = if mode_l == "dfs" {
            traverse_dfs(&kg, seed, depth)
        } else {
            traverse_bfs(&kg, seed, depth)
        };
        for id in visited {
            if seen.insert(id.clone())
                && let Some(e) = kg.entity(&id)
            {
                neighbourhood.push(format!(
                    "{} ({})",
                    e.label,
                    e.source_file.as_deref().unwrap_or("-")
                ));
            }
        }
    }

    out.push_str(&format!(
        "\nNeighbourhood ({} nodes via {mode_l}, depth {depth}):\n",
        neighbourhood.len()
    ));
    for line in neighbourhood.iter().take(40) {
        out.push_str(&format!("  - {line}\n"));
    }
    if neighbourhood.len() > 40 {
        out.push_str(&format!("  … {} more\n", neighbourhood.len() - 40));
    }

    Ok(out)
}

fn traverse_bfs(kg: &KnowledgeGraph, start: &EntityId, max_depth: usize) -> Vec<EntityId> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let mut q: VecDeque<(EntityId, usize)> = VecDeque::new();
    q.push_back((start.clone(), 0));
    seen.insert(start.clone());
    while let Some((id, d)) = q.pop_front() {
        out.push(id.clone());
        if d >= max_depth {
            continue;
        }
        for n in kg.neighbors(&id) {
            if seen.insert(n.id.clone()) {
                q.push_back((n.id.clone(), d + 1));
            }
        }
    }
    out
}

fn traverse_dfs(kg: &KnowledgeGraph, start: &EntityId, max_depth: usize) -> Vec<EntityId> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    fn walk(
        kg: &KnowledgeGraph,
        id: &EntityId,
        depth: usize,
        max_depth: usize,
        seen: &mut HashSet<EntityId>,
        out: &mut Vec<EntityId>,
    ) {
        if !seen.insert(id.clone()) {
            return;
        }
        out.push(id.clone());
        if depth >= max_depth {
            return;
        }
        for n in kg.neighbors(id) {
            walk(kg, &n.id, depth + 1, max_depth, seen, out);
        }
    }
    walk(kg, start, 0, max_depth, &mut seen, &mut out);
    out
}

// ---------------------------------------------------------------------------
// ingest
// ---------------------------------------------------------------------------

/// Ingest a local directory or URL into the knowledge graph.
pub fn ingest_target(
    config: &McpConfig,
    target: &str,
    output: Option<&Path>,
    contributor: Option<&str>,
) -> Result<String, GraphifyError> {
    if target.starts_with("http://") || target.starts_with("https://") {
        return ingest_url(config, target, output, contributor);
    }
    ingest_local(config, Path::new(target), output)
}

fn ingest_url(
    config: &McpConfig,
    url: &str,
    output: Option<&Path>,
    contributor: Option<&str>,
) -> Result<String, GraphifyError> {
    #[cfg(feature = "http-client")]
    {
        use crate::ingest::{self, ReqwestHttpClient};
        let out = output
            .map(|p| config.resolve(p))
            .unwrap_or_else(|| config.work_dir.join("graphify-out").join("memory"));
        std::fs::create_dir_all(&out)
            .map_err(|e| GraphifyError::IngestError(format!("create output dir: {e}")))?;
        let client = ReqwestHttpClient::new()?;
        let result = ingest::ingest(url, &out, &client, contributor)?;
        Ok(format!(
            "Ingested URL ({:?}) → {}",
            result.url_type,
            result.path.display()
        ))
    }
    #[cfg(not(feature = "http-client"))]
    {
        let _ = (config, url, output, contributor);
        Err(GraphifyError::IngestError(
            "URL ingest requires the `http-client` feature on clawft-graphify".into(),
        ))
    }
}

fn ingest_local(
    config: &McpConfig,
    target: &Path,
    output: Option<&Path>,
) -> Result<String, GraphifyError> {
    let root = config.resolve(target);
    if !root.exists() {
        return Err(GraphifyError::IngestError(format!(
            "path does not exist: {}",
            root.display()
        )));
    }
    if !root.is_dir() {
        return Err(GraphifyError::IngestError(
            "single-file ingest is not supported; pass a directory (or use rebuild)".into(),
        ));
    }

    let detection = detect::detect(&root)?;
    let extractions = build_extractions_from_detection(&detection);
    let pipeline_detection = crate::model::DetectionResult {
        total_files: detection.total_files,
        total_words: detection.total_words,
        warning: detection.warning.clone(),
    };

    let pipeline = Pipeline::new(PipelineConfig::default());
    let result = pipeline.run_from_extractions(extractions, pipeline_detection)?;

    let out_dir = output
        .map(|p| config.resolve(p))
        .unwrap_or_else(|| root.join("graphify-out"));
    std::fs::create_dir_all(&out_dir)
        .map_err(|e| GraphifyError::ExportError(format!("create {}: {e}", out_dir.display())))?;

    let mut graph = result.graph;
    if let Some(ref analysis) = result.analysis {
        graph.communities = Some(analysis.communities.clone());
        graph.community_summaries = Some(analysis.community_summaries.clone());
    }

    let json_path = out_dir.join("graph.json");
    export::export(&graph, ExportFormat::Json, &json_path)?;

    let mut msg = format!(
        "Ingested {} → {}\n  files: {}\n  nodes: {}\n  edges: {}",
        root.display(),
        json_path.display(),
        result.stats.files_processed,
        graph.node_count(),
        graph.edge_count(),
    );
    if let Some(ref analysis) = result.analysis {
        msg.push_str(&format!("\n  communities: {}", analysis.communities.len()));
    }
    Ok(msg)
}

/// File-level extractions without tree-sitter (same strategy as weaver graphify).
fn build_extractions_from_detection(detection: &detect::DetectionResult) -> Vec<ExtractionResult> {
    let mut extractions: Vec<ExtractionResult> = Vec::new();
    let mut dir_entities: HashMap<String, Vec<(EntityId, String)>> = HashMap::new();

    let type_map: &[(&str, FileType, EntityType)] = &[
        ("code", FileType::Code, EntityType::Module),
        (
            "document",
            FileType::Document,
            EntityType::Custom("document".into()),
        ),
        ("paper", FileType::Paper, EntityType::Custom("paper".into())),
        ("image", FileType::Image, EntityType::Custom("image".into())),
    ];

    for &(key, file_type, ref entity_type) in type_map {
        let Some(files) = detection.files.get(key) else {
            continue;
        };
        for file_path in files {
            let label = Path::new(file_path)
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or(file_path)
                .to_string();

            let domain = match file_type {
                FileType::Code => DomainTag::Code,
                _ => DomainTag::Forensic,
            };
            let id = EntityId::new(&domain, entity_type, &label, file_path);
            let dir = Path::new(file_path)
                .parent()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            dir_entities
                .entry(dir)
                .or_default()
                .push((id.clone(), label.clone()));

            extractions.push(ExtractionResult {
                source_file: file_path.clone(),
                entities: vec![Entity {
                    id,
                    entity_type: entity_type.clone(),
                    label,
                    source_file: Some(file_path.clone()),
                    source_location: None,
                    file_type,
                    metadata: json!({}),
                    iri: None,
                    legacy_id: Some(file_path.clone()),
                }],
                relationships: vec![],
                hyperedges: vec![],
                input_tokens: 0,
                output_tokens: 0,
                errors: vec![],
            });
        }
    }

    // Co-location star edges within moderate-sized directories.
    for entities in dir_entities.values() {
        if entities.len() < 2 || entities.len() > 50 {
            continue;
        }
        let (hub_id, _) = &entities[0];
        for (other_id, _) in entities.iter().skip(1) {
            // Attach relationship onto the hub's extraction if present.
            if let Some(ext) = extractions
                .iter_mut()
                .find(|e| e.entities.iter().any(|ent| ent.id == *hub_id))
            {
                ext.relationships.push(Relationship {
                    source: hub_id.clone(),
                    target: other_id.clone(),
                    relation_type: RelationType::RelatedTo,
                    confidence: Confidence::Inferred,
                    weight: 0.5,
                    source_file: None,
                    source_location: None,
                    metadata: json!({}),
                    embedding: None,
                });
            }
        }
    }

    extractions
}

// ---------------------------------------------------------------------------
// export
// ---------------------------------------------------------------------------

/// Export graph JSON to a format.
pub fn export_graph(
    config: &McpConfig,
    format: &str,
    graph_path: &Path,
    output: Option<&Path>,
) -> Result<String, GraphifyError> {
    let path = config.resolve(graph_path);
    let kg = load_graph_json(&path)?;
    let format_lower = format.to_lowercase();
    let export_format = ExportFormat::from_str_loose(&format_lower).ok_or_else(|| {
        GraphifyError::ExportError(format!(
            "unknown format '{format}'. Supported: json, graphml, cypher, html, obsidian, wiki, svg, vowl"
        ))
    })?;

    let default_out = match export_format {
        ExportFormat::Json => PathBuf::from("graphify-out/graph.json"),
        ExportFormat::Obsidian => PathBuf::from("graphify-out/obsidian"),
        ExportFormat::Wiki => PathBuf::from("graphify-out/wiki"),
        ExportFormat::Html => PathBuf::from("graphify-out/graph.html"),
        ExportFormat::GraphMl => PathBuf::from("graphify-out/graph.graphml"),
        ExportFormat::Cypher => PathBuf::from("graphify-out/graph.cypher"),
        ExportFormat::Svg => PathBuf::from("graphify-out/graph.svg"),
        ExportFormat::Vowl => PathBuf::from("graphify-out/graph.vowl.json"),
    };
    let out = config.resolve(output.unwrap_or(&default_out));
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            GraphifyError::ExportError(format!("create parent {}: {e}", parent.display()))
        })?;
    }
    export::export(&kg, export_format, &out)?;
    Ok(format!(
        "Exported {} as {format_lower} → {}",
        path.display(),
        out.display()
    ))
}

// ---------------------------------------------------------------------------
// diff
// ---------------------------------------------------------------------------

/// Diff two graph JSON paths.
pub fn graph_diff_paths(
    config: &McpConfig,
    old_path: &Path,
    current_path: &Path,
) -> Result<String, GraphifyError> {
    let current = config.resolve(current_path);
    if !current.exists() {
        return Err(GraphifyError::IngestError(format!(
            "current graph not found: {}",
            current.display()
        )));
    }
    let old = config.resolve(old_path);
    if !old.exists() {
        return Ok(format!(
            "No previous graph at {} — first build (current: {} nodes).",
            old.display(),
            load_graph_json(&current)?.node_count()
        ));
    }

    let old_kg = load_graph_json(&old)?;
    let new_kg = load_graph_json(&current)?;
    let diff = analyze::graph_diff(&old_kg, &new_kg);

    Ok(format!(
        "Graph diff\n  old:     {}\n  current: {}\n  summary: {}\n  +nodes: {}\n  -nodes: {}\n  +edges: {}\n  -edges: {}",
        old.display(),
        current.display(),
        diff.summary,
        diff.new_nodes.len(),
        diff.removed_nodes.len(),
        diff.new_edges.len(),
        diff.removed_edges.len(),
    ))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entity::EntityId;
    use crate::export::json;
    use tempfile::TempDir;

    fn sample_graph_json() -> Value {
        json!({
            "nodes": [
                {
                    "id": "mod_a",
                    "label": "AuthModule",
                    "source_file": "src/auth.rs",
                    "file_type": "code",
                    "entity_type": "module"
                },
                {
                    "id": "fn_login",
                    "label": "login",
                    "source_file": "src/auth.rs",
                    "file_type": "code",
                    "entity_type": "function"
                },
                {
                    "id": "mod_b",
                    "label": "Billing",
                    "source_file": "src/billing.rs",
                    "file_type": "code",
                    "entity_type": "module"
                }
            ],
            "edges": [
                {
                    "source": "mod_a",
                    "target": "fn_login",
                    "relation": "contains",
                    "confidence": "EXTRACTED",
                    "source_file": "src/auth.rs"
                },
                {
                    "source": "fn_login",
                    "target": "mod_b",
                    "relation": "calls",
                    "confidence": "INFERRED",
                    "source_file": "src/auth.rs"
                }
            ]
        })
    }

    fn write_sample(dir: &Path) -> PathBuf {
        let path = dir.join("graph.json");
        let kg = build_from_json(&sample_graph_json()).expect("sample graph");
        json::to_json(&kg, &path).expect("write graph");
        path
    }

    #[test]
    fn query_finds_auth_module() {
        let tmp = TempDir::new().unwrap();
        let graph = write_sample(tmp.path());
        let cfg = McpConfig {
            work_dir: tmp.path().to_path_buf(),
            default_graph: PathBuf::from("graph.json"),
        };
        let out = query_graph(&cfg, "auth login", &graph, "bfs", 2, 10).unwrap();
        assert!(out.contains("AuthModule") || out.contains("login"), "{out}");
        assert!(out.contains("Neighbourhood"), "{out}");
    }

    #[test]
    fn query_none_mode_skips_neighbourhood() {
        let tmp = TempDir::new().unwrap();
        let graph = write_sample(tmp.path());
        let cfg = McpConfig {
            work_dir: tmp.path().to_path_buf(),
            default_graph: graph.clone(),
        };
        let out = query_graph(&cfg, "Billing", &graph, "none", 3, 5).unwrap();
        assert!(out.contains("Billing"), "{out}");
        assert!(!out.contains("Neighbourhood"), "{out}");
    }

    #[test]
    fn dispatch_unknown_tool() {
        let cfg = McpConfig::default();
        let r = dispatch_tool(&cfg, "nope", &json!({}));
        assert!(r.is_error);
        assert!(r.text.contains("unknown"));
    }

    #[test]
    fn dispatch_query_missing_question() {
        let cfg = McpConfig::default();
        let r = dispatch_tool(&cfg, TOOL_QUERY, &json!({}));
        assert!(r.is_error);
    }

    #[test]
    fn ingest_local_directory() {
        let tmp = TempDir::new().unwrap();
        // Minimal project tree
        let src = tmp.path().join("proj").join("src");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("main.rs"), "fn main() {}").unwrap();
        std::fs::write(src.join("lib.rs"), "pub fn x() {}").unwrap();

        let cfg = McpConfig {
            work_dir: tmp.path().to_path_buf(),
            default_graph: PathBuf::from("graphify-out/graph.json"),
        };
        let out = ingest_target(&cfg, "proj", Some(Path::new("proj/graphify-out")), None)
            .expect("ingest");
        assert!(out.contains("nodes:"), "{out}");
        assert!(tmp.path().join("proj/graphify-out/graph.json").exists());
    }

    #[test]
    fn export_json_roundtrip() {
        let tmp = TempDir::new().unwrap();
        let graph = write_sample(tmp.path());
        let cfg = McpConfig {
            work_dir: tmp.path().to_path_buf(),
            default_graph: graph.clone(),
        };
        let dest = tmp.path().join("out.json");
        let msg = export_graph(&cfg, "json", &graph, Some(&dest)).unwrap();
        assert!(msg.contains("Exported"), "{msg}");
        assert!(dest.exists());
    }

    #[test]
    fn diff_identical_graphs() {
        let tmp = TempDir::new().unwrap();
        let a = write_sample(tmp.path());
        let b = tmp.path().join("graph2.json");
        std::fs::copy(&a, &b).unwrap();
        let cfg = McpConfig {
            work_dir: tmp.path().to_path_buf(),
            default_graph: a.clone(),
        };
        let msg = graph_diff_paths(&cfg, &a, &b).unwrap();
        assert!(
            msg.contains("no changes") || msg.contains("+nodes: 0"),
            "{msg}"
        );
    }

    #[test]
    fn diff_missing_old_is_first_build() {
        let tmp = TempDir::new().unwrap();
        let current = write_sample(tmp.path());
        let cfg = McpConfig {
            work_dir: tmp.path().to_path_buf(),
            default_graph: current.clone(),
        };
        let msg = graph_diff_paths(&cfg, Path::new("missing.bak"), &current).unwrap();
        assert!(msg.contains("first build"), "{msg}");
    }

    #[test]
    #[allow(unused_imports)]
    fn entity_id_import_used_in_helpers() {
        // Keep EntityId linked for traversal helpers under rustc unused lints.
        let _ = EntityId::from_legacy_string("x");
    }
}
