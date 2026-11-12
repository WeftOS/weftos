//! `LineageViewer` — renders a `Lineage` provenance document as identity
//! chrome + a node-link graph (sources → via_actor → target).
//!
//! WEFT-275 / EXPLORER-MANAGEMENT-SURFACE affordance #14. Pairs with the
//! `Lineage` ObjectType (priority 13). Graph painting is delegated to
//! [`super::graph::GraphViewer`] via a synthetic `{nodes, edges}` value
//! so we keep a single read-only graph primitive (`ui://graph`).

use super::SubstrateViewer;
use crate::ontology::types::lineage::{
    Lineage, LINEAGE_PRIORITY, resolve_target_path, source_paths,
};
use crate::ontology::ObjectType;
use serde_json::{json, Value};

pub struct LineageViewer;

impl SubstrateViewer for LineageViewer {
    fn matches(value: &Value) -> u32 {
        // Mirror the ObjectType classifier so type badge and viewer fire
        // under the same conditions.
        Lineage::matches(value).min(LINEAGE_PRIORITY)
    }

    fn paint(ui: &mut egui::Ui, path: &str, value: &Value) {
        let Some(obj) = value.as_object() else {
            return;
        };

        ui.label(
            egui::RichText::new(format!("lineage · {path}"))
                .color(crate::theming::palette::TEXT_SUBTLE)
                .small(),
        );
        ui.add_space(4.0);

        let sources = source_paths(value);
        let via_actor = obj.get("via_actor").and_then(Value::as_str);
        let derivation = obj.get("derivation").and_then(Value::as_str);
        let target = resolve_target_path(value, path);
        let ts = obj.get("ts").map(ts_to_string);

        // Identity board.
        egui::Grid::new(("lineage_kvs", path))
            .num_columns(2)
            .spacing([12.0, 2.0])
            .show(ui, |ui| {
                if let Some(actor) = via_actor {
                    ui.label(egui::RichText::new("via_actor").weak());
                    ui.label(
                        egui::RichText::new(actor)
                            .monospace()
                            .color(crate::theming::palette::ACCENT_SAND),
                    );
                    ui.end_row();
                }
                if let Some(d) = derivation {
                    ui.label(egui::RichText::new("derivation").weak());
                    ui.label(egui::RichText::new(d).monospace());
                    ui.end_row();
                }
                if let Some(t) = ts.as_ref() {
                    ui.label(egui::RichText::new("ts").weak());
                    ui.label(egui::RichText::new(t).monospace());
                    ui.end_row();
                }
                ui.label(egui::RichText::new("sources").weak());
                ui.label(
                    egui::RichText::new(format!("{}", sources.len()))
                        .monospace()
                        .weak(),
                );
                ui.end_row();
            });

        ui.add_space(6.0);

        // Navigable path list: sources + target.
        if !sources.is_empty() {
            ui.label(
                egui::RichText::new("source paths")
                    .small()
                    .color(crate::theming::palette::TEXT_LABEL_FAINT),
            );
            for src in &sources {
                paint_path_link(ui, src, "Navigate to source path");
            }
        }
        if let Some(ref tgt) = target {
            ui.label(
                egui::RichText::new("target path")
                    .small()
                    .color(crate::theming::palette::TEXT_LABEL_FAINT),
            );
            paint_path_link(ui, tgt, "Navigate to derived target");
        }

        ui.add_space(8.0);
        ui.separator();
        ui.add_space(4.0);

        // Build synthetic ui://graph and hand off to GraphViewer.
        let graph_value = build_lineage_graph(&sources, via_actor, target.as_deref());
        if graph_value
            .get("nodes")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false)
        {
            super::graph::GraphViewer::paint(ui, path, &graph_value);
        } else {
            ui.label(
                egui::RichText::new("lineage: no sources or target to graph")
                    .color(crate::theming::palette::TEXT_SUBTLE)
                    .italics(),
            );
        }
    }
}

fn paint_path_link(ui: &mut egui::Ui, path: &str, hover: &str) {
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new("→")
                .small()
                .color(crate::theming::palette::TEXT_LABEL_FAINT),
        );
        let link = ui
            .link(
                egui::RichText::new(path)
                    .monospace()
                    .small()
                    .color(crate::theming::palette::ACCENT_STEEL),
            )
            .on_hover_text(hover);
        if link.clicked() {
            crate::explorer::request_navigation(ui.ctx(), path.to_string());
        }
    });
}

/// Materialise a GraphViewer-compatible value from lineage fields.
fn build_lineage_graph(
    sources: &[String],
    via_actor: Option<&str>,
    target: Option<&str>,
) -> Value {
    let mut nodes: Vec<Value> = Vec::new();
    let mut edges: Vec<Value> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    let push_node = |nodes: &mut Vec<Value>,
                     seen: &mut std::collections::HashSet<String>,
                     id: &str,
                     kind: &str| {
        if seen.insert(id.to_owned()) {
            // Short label: last two path segments when path-shaped.
            let label = short_label(id);
            nodes.push(json!({
                "id": id,
                "label": label,
                "kind": kind,
            }));
        }
    };

    for src in sources {
        push_node(&mut nodes, &mut seen, src, "source");
    }

    let actor_id = via_actor.map(|a| format!("actor:{a}"));
    if let Some(ref aid) = actor_id {
        push_node(
            &mut nodes,
            &mut seen,
            aid,
            "actor",
        );
        // Prefer a human label on the actor node.
        if let Some(node) = nodes.iter_mut().find(|n| n.get("id").and_then(Value::as_str) == Some(aid.as_str()))
            && let Some(obj) = node.as_object_mut() {
                obj.insert(
                    "label".into(),
                    Value::String(via_actor.unwrap_or("actor").to_owned()),
                );
            }
    }

    if let Some(tgt) = target {
        push_node(&mut nodes, &mut seen, tgt, "target");
    }

    // Edges: source → actor → target, or source → target when no actor.
    match (actor_id.as_deref(), target) {
        (Some(actor), Some(tgt)) => {
            for src in sources {
                edges.push(json!({
                    "source": src,
                    "target": actor,
                    "kind": "reads",
                }));
            }
            edges.push(json!({
                "source": actor,
                "target": tgt,
                "kind": "publishes",
            }));
        }
        (Some(actor), None) => {
            for src in sources {
                edges.push(json!({
                    "source": src,
                    "target": actor,
                    "kind": "reads",
                }));
            }
        }
        (None, Some(tgt)) => {
            for src in sources {
                edges.push(json!({
                    "source": src,
                    "target": tgt,
                    "kind": "derives",
                }));
            }
        }
        (None, None) => {}
    }

    json!({ "nodes": nodes, "edges": edges })
}

fn short_label(path: &str) -> String {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    if parts.len() >= 2 {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    } else if let Some(last) = parts.last() {
        (*last).to_owned()
    } else {
        path.to_owned()
    }
}

fn ts_to_string(v: &Value) -> String {
    match v {
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        _ => "?".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn matches_lineage_document() {
        let v = json!({
            "kind": "lineage",
            "source_paths": ["substrate/n-6f3a9c/sensor/mic"],
            "via_actor": "whisper",
            "target_path": "substrate/n-daemon/derived/transcript/n-6f3a9c/mic",
        });
        assert_eq!(LineageViewer::matches(&v), LINEAGE_PRIORITY);
    }

    #[test]
    fn rejects_non_lineage() {
        assert_eq!(LineageViewer::matches(&json!({ "text": "hi" })), 0);
        assert_eq!(LineageViewer::matches(&Value::Null), 0);
    }

    #[test]
    fn smoke_build_graph_for_derived_path() {
        // AC: derived path with lineage attached → graph has source, actor, target.
        let sources = vec!["substrate/n-6f3a9c/sensor/mic".to_string()];
        let graph = build_lineage_graph(
            &sources,
            Some("whisper"),
            Some("substrate/n-daemon/derived/transcript/n-6f3a9c/mic"),
        );
        let nodes = graph["nodes"].as_array().expect("nodes");
        let edges = graph["edges"].as_array().expect("edges");
        assert_eq!(nodes.len(), 3, "source + actor + target: {nodes:?}");
        assert_eq!(edges.len(), 2, "reads + publishes: {edges:?}");
        // GraphViewer must accept the synthetic shape.
        assert!(
            super::super::graph::GraphViewer::matches(&graph) > 0,
            "synthetic graph must match GraphViewer"
        );
    }

    #[test]
    fn graph_without_actor_is_source_to_target() {
        let sources = vec!["a".into(), "b".into()];
        let graph = build_lineage_graph(&sources, None, Some("out"));
        let edges = graph["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().all(|e| e["kind"] == "derives"));
    }

    #[test]
    fn short_label_uses_last_two_segments() {
        assert_eq!(
            short_label("substrate/n-6f3a9c/sensor/mic"),
            "sensor/mic"
        );
        assert_eq!(short_label("mic"), "mic");
    }

    #[test]
    fn resolve_target_from_meta_path_in_viewer_context() {
        let doc = json!({
            "kind": "lineage",
            "source_paths": ["substrate/n-1/sensor/mic"],
        });
        let path = "substrate/n-daemon/derived/out/meta/lineage";
        assert_eq!(
            resolve_target_path(&doc, path).as_deref(),
            Some("substrate/n-daemon/derived/out")
        );
    }
}
