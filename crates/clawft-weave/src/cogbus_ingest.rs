//! The ingest bridge as a cogbus consumer (ADR-122 section 9, phase B4).
//!
//! - **Compatibility listener.** The bridge keeps its HTTP contract
//!   (`POST /api/v1/store/ingest` with `COGNITUM_COG_TOKEN`, the same shapes,
//!   auth and rate limits). Its router is a [`BusRouter`]: each accepted post
//!   is published on the broker as a `store.vector.v1` reading by the
//!   broker-internal writer [`BRIDGE_WRITER`] for the posting instance, then
//!   handed to the store sink, whose answer (the store's counts, or why it
//!   could not take the batch) is the HTTP answer. The sink takes the batch
//!   the bridge already parsed, so the stored floats are exactly today's. A
//!   post the broker refuses (another writer holds the topic) is logged and
//!   still delivered, so no released cog regresses.
//! - **Store sink.** [`StoreSink`] is the broker's built-in subscriber to
//!   `store.vector/*`. It forwards through the same router, forwarders and
//!   vector store dir as the bridge did, and writes to the store of the
//!   project that placed the publishing instance only: the instance's
//!   binding (project and placing controller, from its registration) must
//!   name the project of the reading's topic.
//! - **Broker-native cogs.** A cog with `WEFTOS_COGBUS` whose `[[outputs]]`
//!   declare `store.vector` / `store.vector.v1` publishes its own readings;
//!   they reach the store through the same sink, under the same per-instance
//!   rate budget as the bridge. This path needs no TCP port at all.
//!
//! Topic source id: the posting **instance id** (`store.vector/<instance>`),
//! not the cog id, because the store keys vectors by instance and because two
//! placements of one cog (two projects on one node) would otherwise contend
//! for one topic. An instance id that is not a valid source id (over 64
//! chars, upper case) becomes `i-<16 hex of its BLAKE3>`. The store namespace
//! is always the real instance id, whatever the topic is called.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use clawft_kernel::cog_ingest::{
    BridgeHandle, BridgeScope, Forwarder, IngestBatch, IngestBridge, IngestError, IngestOutcome,
    InstanceBinding, RateBudget, StoreRouter, parse_batch,
};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use weftos_cogbus::broker::valid_source_id;
use weftos_cogbus::{Broker, Reading, SinkReading};

use crate::cog_ingest_serve::BridgeSection;

/// Topic kind the store sink takes.
pub const STORE_KIND: &str = "store.vector";
/// Its schema.
pub const STORE_SCHEMA: &str = "store.vector.v1";
/// The broker-internal identity the compatibility listener writes as.
pub const BRIDGE_WRITER: &str = "@ingest-bridge";
/// Readings from broker-native cogs the sink may have pending.
pub const SINK_QUEUE: usize = 1024;
/// `bridge.bind` value that turns the HTTP listener off.
pub const BIND_OFF: &str = "off";

/// The `store.vector` source id for an instance.
pub fn source_of(instance_id: &str) -> String {
    if valid_source_id(instance_id) {
        instance_id.to_string()
    } else {
        format!("i-{}", &blake3::hash(instance_id.as_bytes()).to_hex()[..16])
    }
}

/// A batch as a `store.vector.v1` body.
pub fn body_of(batch: &IngestBatch) -> Value {
    let v: Vec<Value> = batch.vectors.iter().map(|v| json!([v.id, v.values])).collect();
    json!({ "vectors": v, "dedup": batch.dedup })
}

/// The batch in a `store.vector.v1` body, checked exactly as an HTTP body is.
/// Envelope fields and `metrics` are not part of the batch.
pub fn batch_of(body: &Value) -> Result<IngestBatch, IngestError> {
    let obj = body
        .as_object()
        .ok_or_else(|| IngestError::Malformed("body must be an object".into()))?;
    let inner: serde_json::Map<String, Value> = ["vectors", "dedup"]
        .into_iter()
        .filter_map(|k| obj.get(k).map(|v| (k.to_string(), v.clone())))
        .collect();
    let bytes = serde_json::to_vec(&inner).map_err(|e| IngestError::Malformed(e.to_string()))?;
    parse_batch(&bytes)
}

/// The broker's store sink.
pub struct StoreSink {
    router: Arc<dyn StoreRouter>,
    budget: RateBudget,
    bound: Mutex<HashMap<String, InstanceBinding>>,
}

impl StoreSink {
    /// Sink over the ingest router, charging broker-native readings to `budget`.
    pub fn new(router: Arc<dyn StoreRouter>, budget: RateBudget) -> Self {
        Self { router, budget, bound: Mutex::default() }
    }

    /// An instance registered with the broker may publish to its store.
    pub fn bind(&self, b: InstanceBinding) {
        if let Ok(mut m) = self.bound.lock() {
            m.insert(b.instance_id.clone(), b);
        }
    }

    /// The instance is gone.
    pub fn unbind(&self, instance_id: &str) {
        if let Ok(mut m) = self.bound.lock() {
            m.remove(instance_id);
        }
        self.budget.forget(instance_id);
    }

    /// True if a store owner is known for `b`.
    pub fn has_route(&self, b: &InstanceBinding) -> bool {
        self.router.route(b).is_some()
    }

    /// Write `batch` to the store of `b`'s placement.
    pub async fn deliver(&self, b: &InstanceBinding, batch: &IngestBatch) -> Result<IngestOutcome, IngestError> {
        let fwd = self
            .router
            .route(b)
            .ok_or_else(|| IngestError::Unavailable("no store owner for this placement".into()))?;
        fwd.forward(b, batch).await
    }

    /// A reading a cog published itself.
    pub async fn take(&self, r: &SinkReading) -> Result<IngestOutcome, IngestError> {
        let b = self
            .bound
            .lock()
            .ok()
            .and_then(|m| m.get(&r.instance_id).cloned())
            .ok_or(IngestError::Unauthorized)?;
        if b.project_id != r.project_id {
            return Err(IngestError::Forbidden);
        }
        let batch = batch_of(&r.delivery.body)?;
        if !self.budget.charge_request(&b.instance_id)
            || !self.budget.charge_vectors(&b.instance_id, batch.vectors.len())
        {
            return Err(IngestError::RateLimited);
        }
        self.deliver(&b, &batch).await
    }
}

/// Drain the broker's `store.vector/*` queue into `sink`.
pub fn spawn_sink(sink: Arc<StoreSink>, mut rx: mpsc::Receiver<SinkReading>) -> JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(r) = rx.recv().await {
            match sink.take(&r).await {
                Ok(o) => tracing::debug!(target: "cogbus", topic = %r.delivery.topic, accepted = o.accepted, "store sink"),
                Err(e) => tracing::warn!(target: "cogbus", topic = %r.delivery.topic, instance = %r.instance_id,
                    error = %e, "store sink refused a reading"),
            }
        }
    })
}

/// The bridge's router when the broker runs: routes exactly where the
/// ingest router does, through the broker.
pub struct BusRouter(Arc<BusForwarder>);

/// Publishes a post on the broker and hands it to the store sink.
pub struct BusForwarder {
    broker: Arc<Broker>,
    sink: Arc<StoreSink>,
}

impl BusRouter {
    /// Router publishing on `broker` and storing through `sink`.
    pub fn new(broker: Arc<Broker>, sink: Arc<StoreSink>) -> Self {
        Self(Arc::new(BusForwarder { broker, sink }))
    }
}

impl StoreRouter for BusRouter {
    fn route(&self, b: &InstanceBinding) -> Option<Arc<dyn Forwarder>> {
        self.0.sink.has_route(b).then(|| self.0.clone() as Arc<dyn Forwarder>)
    }
}

#[async_trait]
impl Forwarder for BusForwarder {
    async fn forward(&self, b: &InstanceBinding, batch: &IngestBatch) -> Result<IngestOutcome, IngestError> {
        let t_ns = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos() as u64);
        let r = Reading {
            kind: STORE_KIND.into(),
            source_id: source_of(&b.instance_id),
            schema: STORE_SCHEMA.into(),
            t_ns,
            body: body_of(batch),
        };
        if let Err(e) = self.broker.publish_internal(BRIDGE_WRITER, &b.instance_id, b.project_id.clone(), r) {
            // ADR-122 B3: into the activity log (rate limited); never a stop.
            self.broker.record_refusal(&e, BRIDGE_WRITER);
            tracing::warn!(target: "cogbus", instance = %b.instance_id, error = %e,
                "ingest post not published on cogbus; delivered to the store directly");
        }
        self.sink.deliver(b, batch).await
    }
}

/// What [`attach`] starts.
pub struct Attached {
    /// The store sink.
    pub sink: Arc<StoreSink>,
    /// Its worker, draining the broker's `store.vector/*` queue.
    pub task: JoinHandle<()>,
    /// The router the bridge should use.
    pub router: Arc<dyn StoreRouter>,
}

/// The store sink on `broker` over the ingest `router`.
pub fn attach(broker: Arc<Broker>, router: Arc<dyn StoreRouter>, budget: RateBudget) -> Result<Attached, String> {
    let rx = broker
        .add_sink(STORE_KIND, STORE_SCHEMA, SINK_QUEUE)
        .map_err(|e| format!("cogbus store sink: {}", e.message))?;
    let sink = Arc::new(StoreSink::new(router, budget));
    let task = spawn_sink(sink.clone(), rx);
    let router = Arc::new(BusRouter::new(broker, sink.clone()));
    Ok(Attached { sink, task, router })
}

/// Bind the shared bridge listener: `bridge.bind`, or `bridge.fallback_bind`
/// when that fails; `None` with the reason when neither is listening (or
/// `bind` is `off`). The broker path does not depend on either.
pub async fn bind_bridge(bridge: &Arc<IngestBridge>, b: &BridgeSection) -> (Option<BridgeHandle>, Option<String>) {
    let Ok(primary) = b.bind.parse::<SocketAddr>() else {
        return (None, Some(format!("bridge.bind is {:?}", b.bind)));
    };
    let first = match bridge.bind(primary, BridgeScope::Any).await {
        Ok(l) => return (Some(l), None),
        Err(e) => format!("cannot bind {primary}: {e}"),
    };
    let Some(alt) = b.fallback_bind.as_deref().and_then(|s| s.parse::<SocketAddr>().ok()) else {
        return (None, Some(first));
    };
    match bridge.bind(alt, BridgeScope::Any).await {
        Ok(l) => {
            tracing::warn!(bridge = %l.addr(), reason = %first,
                "cog ingest bridge on its fallback address: cogs that honour COGNITUM_INGEST_URL are served, \
                 released cogs posting to the fixed address are not; cogbus store.vector is unaffected");
            (Some(l), None)
        }
        Err(e) => (None, Some(format!("{first}; fallback: cannot bind {alt}: {e}"))),
    }
}

#[cfg(test)]
#[path = "cogbus_ingest_tests.rs"]
mod tests;
