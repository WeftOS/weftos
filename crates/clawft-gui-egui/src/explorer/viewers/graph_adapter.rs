//! Graph adapter seam for `ui://graph` (WEFT-281).
//!
//! The read-only [`super::graph::GraphViewer`] and the Phase 3+ editable
//! patch UI share this pure model so a future `egui_node_graph` backend
//! can plug in without changing Explorer dispatch or substrate shape.
//!
//! ## Migration path (`egui_node_graph`)
//!
//! 1. Keep JSON `{ nodes, edges }` as the substrate / workshop contract.
//! 2. Implement [`GraphPatchEditor`] for an `egui_node_graph` wrapper
//!    (typed ports, drag-to-connect) behind [`GraphEditBackend::EguiNodeGraph`].
//! 3. Flip the backend at the paint call site — no shape change.
//!
//! We deliberately do **not** depend on `egui_node_graph` today: it pins
//! egui, is editor-grade overkill for Explorer, and would bloat the
//! VSCode panel wasm budget (same rationale as the GraphViewer module
//! docs). The adapter trait is the seam; CustomPainter is the default.

use serde_json::{Value, json};
use std::collections::HashMap;

// ─── backend enum ───────────────────────────────────────────────────

/// Which paint/interact stack drives the editable patch UI.
///
/// Only [`GraphEditBackend::CustomPainter`] is linked. The
/// `EguiNodeGraph` variant is reserved so call sites can switch once
/// the crate is adopted without reshaping the session API.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GraphEditBackend {
    /// In-tree egui painter (zero extra deps). Default for WEFT-281.
    #[default]
    CustomPainter,
    /// Future: wrap `egui_node_graph` once egui pin + wasm budget allow.
    EguiNodeGraph,
}

// ─── model ──────────────────────────────────────────────────────────

/// One node in a `ui://graph` value.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphNode {
    pub id: String,
    pub label: String,
    pub kind: Option<String>,
    pub pos: Option<(f32, f32)>,
}

/// One directed edge in a `ui://graph` value.
#[derive(Clone, Debug, PartialEq)]
pub struct GraphEdge {
    pub source: String,
    pub target: String,
    pub kind: Option<String>,
}

/// Editable graph document: nodes + edges + selection.
///
/// Local-only until graph-edit substrate verbs land; [`GraphDocument::to_value`]
/// is the write-back payload those verbs will accept.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GraphDocument {
    pub nodes: Vec<GraphNode>,
    pub edges: Vec<GraphEdge>,
    /// Selected node id, if any.
    pub selected: Option<String>,
    /// Monotonic counter for synthetic ids (`n{next_id}`).
    pub next_id: u64,
    /// True when local edits diverge from the last ingested `Value`.
    pub dirty: bool,
}

// ─── trait (migration seam) ─────────────────────────────────────────

/// Operations the editable patch UI (and a future egui_node_graph
/// adapter) must support. Pure — no egui types — so unit tests cover
/// the full edit surface without a frame.
pub trait GraphPatchEditor {
    fn from_value(value: &Value) -> Self
    where
        Self: Sized;

    fn to_value(&self) -> Value;

    fn select_node(&mut self, id: Option<String>);

    fn selected(&self) -> Option<&str>;

    fn set_node_label(&mut self, id: &str, label: String) -> bool;

    fn set_node_kind(&mut self, id: &str, kind: Option<String>) -> bool;

    fn set_node_pos(&mut self, id: &str, pos: (f32, f32)) -> bool;

    /// Insert a node; returns its id.
    fn add_node(&mut self, label: String, kind: Option<String>) -> String;

    fn remove_node(&mut self, id: &str) -> bool;

    fn add_edge(&mut self, source: &str, target: &str, kind: Option<String>) -> bool;

    fn remove_edge(&mut self, source: &str, target: &str) -> bool;
}

impl GraphPatchEditor for GraphDocument {
    fn from_value(value: &Value) -> Self {
        Self::parse(value)
    }

    fn to_value(&self) -> Value {
        self.serialize()
    }

    fn select_node(&mut self, id: Option<String>) {
        self.selected = id.filter(|i| self.nodes.iter().any(|n| n.id == *i));
    }

    fn selected(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    fn set_node_label(&mut self, id: &str, label: String) -> bool {
        let Some(node) = self.nodes.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        if node.label != label {
            node.label = label;
            self.dirty = true;
        }
        true
    }

    fn set_node_kind(&mut self, id: &str, kind: Option<String>) -> bool {
        let Some(node) = self.nodes.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        if node.kind != kind {
            node.kind = kind;
            self.dirty = true;
        }
        true
    }

    fn set_node_pos(&mut self, id: &str, pos: (f32, f32)) -> bool {
        let Some(node) = self.nodes.iter_mut().find(|n| n.id == id) else {
            return false;
        };
        if node.pos != Some(pos) {
            node.pos = Some(pos);
            self.dirty = true;
        }
        true
    }

    fn add_node(&mut self, label: String, kind: Option<String>) -> String {
        let id = loop {
            let candidate = format!("n{}", self.next_id);
            self.next_id = self.next_id.saturating_add(1);
            if !self.nodes.iter().any(|n| n.id == candidate) {
                break candidate;
            }
        };
        let n = self.nodes.len() as f32;
        // Place new nodes on a loose diagonal so they don't stack.
        let pos = Some((n * 40.0, n * 28.0));
        self.nodes.push(GraphNode {
            id: id.clone(),
            label,
            kind,
            pos,
        });
        self.selected = Some(id.clone());
        self.dirty = true;
        id
    }

    fn remove_node(&mut self, id: &str) -> bool {
        let before = self.nodes.len();
        self.nodes.retain(|n| n.id != id);
        if self.nodes.len() == before {
            return false;
        }
        self.edges.retain(|e| e.source != id && e.target != id);
        if self.selected.as_deref() == Some(id) {
            self.selected = None;
        }
        self.dirty = true;
        true
    }

    fn add_edge(&mut self, source: &str, target: &str, kind: Option<String>) -> bool {
        if source == target {
            return false;
        }
        if !self.nodes.iter().any(|n| n.id == source) {
            return false;
        }
        if !self.nodes.iter().any(|n| n.id == target) {
            return false;
        }
        if self
            .edges
            .iter()
            .any(|e| e.source == source && e.target == target)
        {
            return false;
        }
        self.edges.push(GraphEdge {
            source: source.to_owned(),
            target: target.to_owned(),
            kind,
        });
        self.dirty = true;
        true
    }

    fn remove_edge(&mut self, source: &str, target: &str) -> bool {
        let before = self.edges.len();
        self.edges
            .retain(|e| !(e.source == source && e.target == target));
        if self.edges.len() == before {
            return false;
        }
        self.dirty = true;
        true
    }
}

// ─── parse / serialize ──────────────────────────────────────────────

impl GraphDocument {
    /// Parse `{ nodes, edges }` with the same shape tolerance as GraphViewer.
    pub fn parse(value: &Value) -> Self {
        let nodes_raw = value
            .as_object()
            .and_then(|o| o.get("nodes"))
            .and_then(Value::as_array);
        let edges_raw = value
            .as_object()
            .and_then(|o| o.get("edges"))
            .and_then(Value::as_array);

        let nodes: Vec<GraphNode> = nodes_raw
            .map(|a| a.iter().filter_map(GraphNode::from_value).collect())
            .unwrap_or_default();
        let edges: Vec<GraphEdge> = edges_raw
            .map(|a| a.iter().filter_map(GraphEdge::from_value).collect())
            .unwrap_or_default();

        let mut max_n = 0u64;
        for n in &nodes {
            if let Some(rest) = n.id.strip_prefix('n')
                && let Ok(v) = rest.parse::<u64>()
            {
                max_n = max_n.max(v + 1);
            }
        }

        Self {
            nodes,
            edges,
            selected: None,
            next_id: max_n,
            dirty: false,
        }
    }

    /// Serialize to the canonical object form used by substrate / workshop.
    ///
    /// Always emits typed objects (not bare ids) so round-trips preserve
    /// label/kind/pos — the form vector-synth and patch UIs need.
    pub fn serialize(&self) -> Value {
        let nodes: Vec<Value> = self
            .nodes
            .iter()
            .map(|n| {
                let mut obj = serde_json::Map::new();
                obj.insert("id".into(), Value::String(n.id.clone()));
                obj.insert("label".into(), Value::String(n.label.clone()));
                if let Some(k) = &n.kind {
                    obj.insert("kind".into(), Value::String(k.clone()));
                }
                if let Some((x, y)) = n.pos {
                    obj.insert("pos".into(), json!([x, y]));
                }
                Value::Object(obj)
            })
            .collect();
        let edges: Vec<Value> = self
            .edges
            .iter()
            .map(|e| {
                let mut obj = serde_json::Map::new();
                obj.insert("source".into(), Value::String(e.source.clone()));
                obj.insert("target".into(), Value::String(e.target.clone()));
                if let Some(k) = &e.kind {
                    obj.insert("kind".into(), Value::String(k.clone()));
                }
                Value::Object(obj)
            })
            .collect();
        json!({ "nodes": nodes, "edges": edges })
    }

    /// Fingerprint of an inbound value for dirty-session re-sync.
    pub fn source_fingerprint(value: &Value) -> u64 {
        // Cheap stable hash of the compact JSON — enough to detect
        // substrate republish without storing the whole value.
        let s = value.to_string();
        s.bytes().fold(0xcbf29ce484222325u64, |acc, b| {
            acc.wrapping_mul(0x100000001b3).wrapping_add(b as u64)
        })
    }

    /// Look up a node by id.
    pub fn node(&self, id: &str) -> Option<&GraphNode> {
        self.nodes.iter().find(|n| n.id == id)
    }

    /// Positions map for layout consumers (id → explicit pos when set).
    pub fn explicit_positions(&self) -> HashMap<String, (f32, f32)> {
        self.nodes
            .iter()
            .filter_map(|n| n.pos.map(|p| (n.id.clone(), p)))
            .collect()
    }
}

impl GraphNode {
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::String(s) => Some(Self {
                id: s.clone(),
                label: s.clone(),
                kind: None,
                pos: None,
            }),
            Value::Number(n) => Some(Self {
                id: n.to_string(),
                label: n.to_string(),
                kind: None,
                pos: None,
            }),
            Value::Object(obj) => {
                let id = obj
                    .get("id")
                    .and_then(|x| match x {
                        Value::String(s) => Some(s.clone()),
                        Value::Number(n) => Some(n.to_string()),
                        _ => None,
                    })
                    .or_else(|| obj.get("label").and_then(Value::as_str).map(str::to_owned))?;
                let label = obj
                    .get("label")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| id.clone());
                let kind = obj.get("kind").and_then(Value::as_str).map(str::to_owned);
                let pos = obj.get("pos").and_then(|p| p.as_array()).and_then(|a| {
                    if a.len() != 2 {
                        return None;
                    }
                    let x = a[0].as_f64()? as f32;
                    let y = a[1].as_f64()? as f32;
                    Some((x, y))
                });
                Some(Self {
                    id,
                    label,
                    kind,
                    pos,
                })
            }
            _ => None,
        }
    }
}

impl GraphEdge {
    pub fn from_value(v: &Value) -> Option<Self> {
        match v {
            Value::Array(a) if a.len() == 2 => {
                let source = endpoint_id(&a[0])?;
                let target = endpoint_id(&a[1])?;
                Some(Self {
                    source,
                    target,
                    kind: None,
                })
            }
            Value::Object(obj) => {
                let source = obj
                    .get("source")
                    .or_else(|| obj.get("from"))
                    .and_then(endpoint_id)?;
                let target = obj
                    .get("target")
                    .or_else(|| obj.get("to"))
                    .and_then(endpoint_id)?;
                let kind = obj.get("kind").and_then(Value::as_str).map(str::to_owned);
                Some(Self {
                    source,
                    target,
                    kind,
                })
            }
            _ => None,
        }
    }
}

/// Extract a stable id from an endpoint (same rules as GraphViewer).
pub fn endpoint_id(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Array(a) => a.first().and_then(endpoint_id),
        _ => None,
    }
}

// ─── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sample() -> Value {
        json!({
            "nodes": [
                { "id": "n1", "label": "Osc", "kind": "osc", "pos": [0.0, 0.0] },
                { "id": "n2", "label": "Vcf", "kind": "vcf", "pos": [1.0, 0.0] }
            ],
            "edges": [
                { "source": "n1", "target": "n2", "kind": "audio" }
            ]
        })
    }

    #[test]
    fn parse_typed_graph() {
        let doc = GraphDocument::parse(&sample());
        assert_eq!(doc.nodes.len(), 2);
        assert_eq!(doc.edges.len(), 1);
        assert_eq!(doc.nodes[0].label, "Osc");
        assert_eq!(doc.edges[0].kind.as_deref(), Some("audio"));
        assert!(!doc.dirty);
        assert!(doc.next_id >= 3);
    }

    #[test]
    fn parse_bare_ids() {
        let v = json!({
            "nodes": ["a", "b"],
            "edges": [["a", "b"]]
        });
        let doc = GraphDocument::parse(&v);
        assert_eq!(doc.nodes.len(), 2);
        assert_eq!(doc.edges[0].source, "a");
        assert_eq!(doc.edges[0].target, "b");
    }

    #[test]
    fn round_trip_preserves_fields() {
        let doc = GraphDocument::parse(&sample());
        let out = doc.to_value();
        let again = GraphDocument::parse(&out);
        assert_eq!(again.nodes, doc.nodes);
        assert_eq!(again.edges, doc.edges);
    }

    #[test]
    fn select_and_edit_label() {
        let mut doc = GraphDocument::parse(&sample());
        doc.select_node(Some("n1".into()));
        assert_eq!(doc.selected(), Some("n1"));
        assert!(doc.set_node_label("n1", "LFO".into()));
        assert_eq!(doc.node("n1").unwrap().label, "LFO");
        assert!(doc.dirty);
    }

    #[test]
    fn select_unknown_clears() {
        let mut doc = GraphDocument::parse(&sample());
        doc.select_node(Some("nope".into()));
        assert!(doc.selected().is_none());
    }

    #[test]
    fn add_remove_node() {
        let mut doc = GraphDocument::parse(&sample());
        let id = doc.add_node("New".into(), Some("util".into()));
        assert!(doc.node(&id).is_some());
        assert_eq!(doc.selected(), Some(id.as_str()));
        assert!(doc.remove_node(&id));
        assert!(doc.node(&id).is_none());
        assert!(doc.selected().is_none());
    }

    #[test]
    fn remove_node_drops_incident_edges() {
        let mut doc = GraphDocument::parse(&sample());
        assert!(doc.remove_node("n1"));
        assert!(doc.edges.is_empty());
    }

    #[test]
    fn add_edge_connect_and_dedupe() {
        let mut doc = GraphDocument::parse(&sample());
        // reverse edge is fine
        assert!(doc.add_edge("n2", "n1", Some("cv".into())));
        assert_eq!(doc.edges.len(), 2);
        // duplicate rejected
        assert!(!doc.add_edge("n2", "n1", None));
        // self-loop rejected
        assert!(!doc.add_edge("n1", "n1", None));
        // missing endpoint rejected
        assert!(!doc.add_edge("n1", "missing", None));
    }

    #[test]
    fn remove_edge() {
        let mut doc = GraphDocument::parse(&sample());
        assert!(doc.remove_edge("n1", "n2"));
        assert!(doc.edges.is_empty());
        assert!(!doc.remove_edge("n1", "n2"));
    }

    #[test]
    fn set_pos_and_kind() {
        let mut doc = GraphDocument::parse(&sample());
        assert!(doc.set_node_pos("n1", (10.0, 20.0)));
        assert_eq!(doc.node("n1").unwrap().pos, Some((10.0, 20.0)));
        assert!(doc.set_node_kind("n1", Some("lfo".into())));
        assert_eq!(doc.node("n1").unwrap().kind.as_deref(), Some("lfo"));
        assert!(doc.set_node_kind("n1", None));
        assert!(doc.node("n1").unwrap().kind.is_none());
    }

    #[test]
    fn fingerprint_stable_and_sensitive() {
        let a = sample();
        let b = sample();
        assert_eq!(
            GraphDocument::source_fingerprint(&a),
            GraphDocument::source_fingerprint(&b)
        );
        let mut c = sample();
        c["nodes"][0]["label"] = json!("changed");
        assert_ne!(
            GraphDocument::source_fingerprint(&a),
            GraphDocument::source_fingerprint(&c)
        );
    }

    #[test]
    fn backend_default_is_custom_painter() {
        assert_eq!(GraphEditBackend::default(), GraphEditBackend::CustomPainter);
    }

    #[test]
    fn vector_synth_cable_shape() {
        let v = json!({
            "nodes": [0, 1],
            "edges": [{ "from": [0, 0], "to": [1, 2] }]
        });
        let doc = GraphDocument::parse(&v);
        assert_eq!(doc.nodes.len(), 2);
        assert_eq!(doc.edges[0].source, "0");
        assert_eq!(doc.edges[0].target, "1");
    }
}
