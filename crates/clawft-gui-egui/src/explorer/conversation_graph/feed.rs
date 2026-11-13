//! Live / replay feed for `conversation.graph` (G2 / G4 scaffold).
//!
//! G2: poll head via Live command / aux channel and merge into [`GraphModel`].
//! G4: request graph-at-T (degraded window filter or full `conversation.replay`).
//!
//! Wiring into `native_live::poll_aux_rpcs` is intentionally deferred — this
//! module owns the pure merge/request helpers so G2 can land without GUI shell
//! churn.

use super::model::GraphModel;
use serde_json::{Value, json};

/// Parameters for the ADR-067 P0 RPC.
#[derive(Debug, Clone)]
pub struct GraphQuery {
    pub conv_id: String,
    pub since: Option<u64>,
    pub window_to_ts: Option<u64>,
}

impl GraphQuery {
    pub fn head(conv_id: impl Into<String>) -> Self {
        Self {
            conv_id: conv_id.into(),
            since: None,
            window_to_ts: None,
        }
    }

    /// JSON-RPC params body for `conversation.graph`.
    pub fn to_params(&self) -> Value {
        let mut p = json!({ "conv_id": self.conv_id });
        if let Some(since) = self.since {
            p["since"] = json!(since);
        }
        if let Some(to_ts) = self.window_to_ts {
            p["window"] = json!({ "to_ts": to_ts });
        }
        p
    }
}

/// Apply a successful RPC payload to the model (G2).
pub fn apply_head(model: &mut GraphModel, payload: &Value) -> Result<(), String> {
    if let Some(cid) = payload.get("conv_id").and_then(Value::as_str) {
        model.conv_id = cid.to_string();
    }
    model.merge_head(payload)
}

/// G4 degraded path: rebuild from a full head filtered to `ts_ms <= t`.
///
/// Full-fidelity scrub needs P1-graph snapshot lineage; until then callers
/// should surface [`super::HISTORY_INCOMPLETE_HINT`].
pub fn filter_model_to_t(model: &GraphModel, t_ms: u64) -> GraphModel {
    let nodes: Vec<_> = model
        .nodes
        .iter()
        .filter(|n| n.ts_ms <= t_ms)
        .cloned()
        .collect();
    let ids: std::collections::HashSet<_> = nodes.iter().map(|n| n.id.clone()).collect();
    let edges: Vec<_> = model
        .edges
        .iter()
        .filter(|e| ids.contains(&e.source) && ids.contains(&e.target))
        .cloned()
        .collect();
    GraphModel {
        conv_id: model.conv_id.clone(),
        nodes,
        edges,
    }
}

/// Optional G4 full path: params for `conversation.replay { conv_id, at }`.
pub fn replay_params(conv_id: &str, at_chain_seq: u64) -> Value {
    json!({ "conv_id": conv_id, "at": at_chain_seq })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn query_params_shape() {
        let q = GraphQuery {
            conv_id: "c1".into(),
            since: Some(3),
            window_to_ts: Some(999),
        };
        let p = q.to_params();
        assert_eq!(p["conv_id"], "c1");
        assert_eq!(p["since"], 3);
        assert_eq!(p["window"]["to_ts"], 999);
    }

    #[test]
    fn filter_to_t_drops_future_nodes() {
        let m = GraphModel::fixture();
        let mid = filter_model_to_t(&m, 2_000);
        assert!(mid.nodes.iter().all(|n| n.ts_ms <= 2_000));
        assert!(mid.nodes.len() < m.nodes.len());
    }

    #[test]
    fn apply_head_merges() {
        let mut m = GraphModel::fixture();
        let payload = json!({
            "conv_id": "c-live",
            "nodes": m.nodes.iter().map(|n| json!({
                "id": n.id,
                "role": n.role.as_str(),
                "state": n.state.as_str(),
                "text": n.text,
                "ts_ms": n.ts_ms,
                "chain_seq": n.chain_seq,
                "conv_id": "c-live",
                "classification": {}
            })).collect::<Vec<_>>(),
            "edges": m.edges.iter().map(|e| json!({
                "source": e.source,
                "target": e.target,
                "kind": e.kind,
                "weight": e.weight
            })).collect::<Vec<_>>()
        });
        apply_head(&mut m, &payload).unwrap();
        assert_eq!(m.conv_id, "c-live");
    }
}
