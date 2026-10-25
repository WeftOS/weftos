//! The NPU service's state in node facts (Ventuno cog plan 4.6, ADR-122
//! section 12).
//!
//! A cog that provides the `svc.infer` service (its manifest lists it as an
//! output) also publishes `telemetry/<service>` (`telemetry.v1`): queue depth,
//! whether the NPU is busy, the resident models and their memory. A built-in
//! sink on `telemetry/*` keeps the newest reading of such a cog, and every
//! facts refresh adds it to the node's `accel.npu.*` capability as attributes:
//!
//! | attribute | meaning |
//! |---|---|
//! | `service_state` | `up`, or `stale` when no reading came for [`STALE_SECS`] |
//! | `service_queue_depth` | calls waiting |
//! | `service_busy` | a call is running |
//! | `service_models` | aliases of the resident models |
//! | `service_mem_resident_mib`, `service_mem_budget_mib` | what they use, what they may |
//!
//! A stale service keeps `service_state = stale` and drops the rest, so
//! placement never reads a dead queue as an empty one. Telemetry from other
//! cogs is ignored, and nothing is added on a node with no NPU capability.

use std::sync::{Arc, Mutex, OnceLock};

use clawft_types::placement::{AttrValue, NodeFacts};
use serde_json::Value;
use tokio::task::JoinHandle;
use weftos_cogbus::{Broker, SinkReading};

/// Readings older than this mean the service is not reporting.
pub const STALE_SECS: u64 = 30;
/// The service kind whose provider is the NPU service.
pub const SERVICE_KIND_PREFIX: &str = "svc.infer";
const SINK_QUEUE: usize = 64;

/// One reading of the service.
#[derive(Debug, Clone, PartialEq)]
pub struct NpuStatus {
    pub queue_depth: i64,
    pub busy: bool,
    pub models: Vec<String>,
    pub mem_resident_mib: i64,
    pub mem_budget_mib: i64,
    /// Unix seconds the reading was taken (the node's clock on receipt).
    pub seen_secs: u64,
}

/// The status in a `telemetry.v1` body; `None` when it carries no queue depth.
pub fn parse(body: &Value, seen_secs: u64) -> Option<NpuStatus> {
    let items = body.get("items")?.as_array()?;
    let val = |n: &str| {
        items
            .iter()
            .find(|i| i["name"] == n)
            .and_then(|i| i["value"].as_f64())
    };
    let mut models: Vec<String> = items
        .iter()
        .filter_map(|i| {
            i["name"]
                .as_str()?
                .strip_prefix("model.loaded.")
                .map(String::from)
        })
        .collect();
    models.sort();
    Some(NpuStatus {
        queue_depth: val("queue.depth")? as i64,
        busy: val("npu.busy").is_some_and(|v| v >= 0.5),
        models,
        mem_resident_mib: val("mem.resident").unwrap_or(0.0) as i64,
        mem_budget_mib: val("mem.budget").unwrap_or(0.0) as i64,
        seen_secs,
    })
}

static LATEST: OnceLock<Mutex<Option<NpuStatus>>> = OnceLock::new();

fn latest() -> &'static Mutex<Option<NpuStatus>> {
    LATEST.get_or_init(|| Mutex::new(None))
}

/// Remember `s` as the newest status.
pub fn record(s: NpuStatus) {
    *latest().lock().unwrap_or_else(|p| p.into_inner()) = Some(s);
}

/// Add the newest status to the first `accel.npu.*` capability of `facts`.
pub fn apply(facts: &mut NodeFacts, now_secs: u64) {
    let status = latest().lock().unwrap_or_else(|p| p.into_inner()).clone();
    apply_status(facts, status.as_ref(), now_secs);
}

fn apply_status(facts: &mut NodeFacts, status: Option<&NpuStatus>, now_secs: u64) {
    let Some(s) = status else { return };
    let Some(cap) = facts
        .capabilities
        .iter_mut()
        .find(|c| c.id.as_str().starts_with("accel.npu."))
    else {
        return;
    };
    for k in [
        "service_state",
        "service_queue_depth",
        "service_busy",
        "service_models",
        "service_mem_resident_mib",
        "service_mem_budget_mib",
    ] {
        cap.attrs.remove(k);
    }
    if now_secs.saturating_sub(s.seen_secs) > STALE_SECS {
        cap.attrs
            .insert("service_state".into(), AttrValue::Str("stale".into()));
        return;
    }
    cap.attrs
        .insert("service_state".into(), AttrValue::Str("up".into()));
    cap.attrs
        .insert("service_queue_depth".into(), AttrValue::Int(s.queue_depth));
    cap.attrs
        .insert("service_busy".into(), AttrValue::Bool(s.busy));
    cap.attrs.insert(
        "service_models".into(),
        AttrValue::List(s.models.iter().map(|m| AttrValue::Str(m.clone())).collect()),
    );
    cap.attrs.insert(
        "service_mem_resident_mib".into(),
        AttrValue::Int(s.mem_resident_mib),
    );
    cap.attrs.insert(
        "service_mem_budget_mib".into(),
        AttrValue::Int(s.mem_budget_mib),
    );
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// True when `instance` provides the NPU service (lists `svc.infer` as an output).
fn provides_npu_service(broker: &Broker, instance: &str) -> bool {
    broker
        .instance(instance)
        .is_some_and(|s| s.outputs.iter().any(|o| o.topic == SERVICE_KIND_PREFIX))
}

/// Take `telemetry/*` on `broker` and keep the NPU service's newest reading.
pub fn attach(broker: &Arc<Broker>) -> Result<JoinHandle<()>, String> {
    let mut rx = broker
        .add_sink("telemetry", "telemetry.v1", SINK_QUEUE)
        .map_err(|e| format!("cogbus telemetry sink: {}", e.message))?;
    let broker = broker.clone();
    Ok(tokio::spawn(async move {
        while let Some(r) = rx.recv().await {
            handle(&broker, &r);
        }
    }))
}

fn handle(broker: &Broker, r: &SinkReading) {
    if !provides_npu_service(broker, &r.instance_id) {
        return;
    }
    match parse(&r.delivery.body, now_secs()) {
        Some(s) => record(s),
        None => {
            tracing::debug!(target: "cogbus", instance = %r.instance_id, "npu telemetry without queue.depth ignored")
        }
    }
}

#[cfg(test)]
#[path = "cogbus_npu_facts_tests.rs"]
mod tests;
