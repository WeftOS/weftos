//! The broker core (ADR-122 sections 2, 3 and 5, readings only), with no
//! transport: the [`server`](crate::server) drives it per connection.
//!
//! Rules a connection is held to:
//!
//! - it presents the token of a registered instance (and, when the instance
//!   was registered with a uid, connects as that uid);
//! - it publishes only output kinds of its manifest, with the declared
//!   schema and class `reading` (`stream` and `blob` are later phases); one
//!   instance writes a topic;
//! - it subscribes only to input kinds of its manifest with the declared
//!   schema, and only to topics of its own project unless the policy grants
//!   another project's ([`crate::policy::BusPolicy::grants`]). A
//!   project-less instance shares the node's own bucket with other
//!   project-less instances.
//!
//! The daemon's own services use [`sink`] (built-in subscribers across
//! projects, internal publishers with no manifest) instead of a connection.
//!
//! On top of these, declare, a cog's first publish on a topic and subscribe
//! ask the governance policy ([`crate::audit`]); internal publishers (`@...`)
//! are not asked. Every refusal goes through [`refuse`], which marks it for
//! [`Broker::record_refusal`].

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use clawft_types::cog_topics::{InputPolicy, TopicClass, is_registered, valid_topic_kind};
use serde_json::Value;
use tokio::sync::{Notify, watch};

use crate::audit::{Ext, granted};
use crate::policy::{BusAction, BusRequest, Grant};
use crate::proto::{ErrorCode, InputSpec, MAX_BODY, OutputSpec};
use crate::queue::SubQueue;
use crate::types::token_hash;
pub use crate::types::{BrokerConfig, BusError, InstanceSpec, Session, TopicRow, refuse, valid_source_id};
pub use sink::{Reading, SinkReading};
use sink::{Put, SinkSlot};

#[path = "sink.rs"]
mod sink;
#[path = "rows.rs"]
mod rows;
#[path = "svc.rs"]
mod svc;
#[path = "blobops.rs"]
mod blobops;

struct Instance {
    spec: InstanceSpec,
    hash: [u8; 32],
    revoked: watch::Sender<bool>,
}

struct Topic {
    schema: String,
    class: TopicClass,
    writer: Option<String>,
    project_id: Option<String>,
    seq: u64,
    messages: u64,
    drops: u64,
}

struct Sub {
    instance: String,
    project_id: Option<String>,
    kind: String,
    source: Option<String>,
    schema: String,
    queue: Arc<SubQueue>,
    /// Other projects' topics granted at subscribe.
    also: Vec<Grant>,
}

impl Sub {
    fn matches(&self, kind: &str, source: &str, t: &Topic) -> bool {
        self.kind == kind
            && self.source.as_deref().is_none_or(|s| s == source)
            && self.schema == t.schema
            && (self.project_id == t.project_id || granted(&self.also, &t.project_id, source))
    }
}

#[derive(Default)]
struct State {
    by_hash: HashMap<[u8; 32], String>,
    instances: HashMap<String, Instance>,
    topics: BTreeMap<(String, String), Topic>,
    subs: BTreeMap<u64, Sub>,
    next_sub: u64,
    sinks: HashMap<String, SinkSlot>,
    svc: svc::Svc,
    /// Declared message rate per (instance, kind), msgs/s.
    rates: HashMap<(String, String), f64>,
    /// Publish budgets per (instance, kind, source).
    buckets: HashMap<(String, String, String), crate::rate::Bucket>,
}

fn now_ns() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

fn bad(code: ErrorCode, m: impl Into<String>) -> BusError {
    BusError::new(code, m)
}

/// The broker.
pub struct Broker {
    cfg: BrokerConfig,
    state: Mutex<State>,
    pub(crate) streams: crate::stream::Streams,
    pub(crate) ext: Ext,
    pub(crate) blobs: Option<Arc<crate::blob::BlobStore>>,
    /// Open connections per instance (by connection id); sessions release
    /// their own slot on drop.
    conns: Arc<Mutex<HashMap<String, HashSet<u64>>>>,
}

impl Broker {
    /// Empty broker.
    pub fn new(cfg: BrokerConfig) -> Self {
        Self {
            cfg,
            state: Mutex::new(State::default()),
            streams: crate::stream::Streams::new(Default::default()),
            ext: Ext::default(),
            blobs: None,
            conns: Arc::default(),
        }
    }

    /// Serve blob requests from `store` (B5). Without it they are
    /// `unsupported`.
    pub fn with_blobs(mut self, store: Arc<crate::blob::BlobStore>) -> Self {
        self.blobs = Some(store);
        self
    }

    /// The content store, if the node has one.
    pub fn blob_store(&self) -> Option<&Arc<crate::blob::BlobStore>> {
        self.blobs.as_ref()
    }

    /// Declare the message rate (per second) of `instance`'s output `kind`;
    /// publishes beyond it (with [`crate::rate::SLACK`]) are `rate_limited`.
    /// Call after `register`; revoke forgets it, so a restart registers
    /// again.
    pub fn set_output_rate(&self, instance: &str, kind: &str, per_sec: f64) {
        if per_sec.is_finite() && per_sec > 0.0 {
            self.lock().rates.insert((instance.to_string(), kind.to_string()), per_sec);
        }
    }

    /// The broker's tunables.
    pub fn config(&self) -> &BrokerConfig {
        &self.cfg
    }

    /// The node id stamped on deliveries.
    pub fn node_id(&self) -> &str {
        &self.cfg.node_id
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        // A panic under the lock leaves plain data; keep serving it.
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Accept `token` for `spec` (at place, and again at start).
    pub fn register(&self, spec: InstanceSpec, token: &str) -> Result<(), BusError> {
        if token.len() < 32 {
            return Err(bad(ErrorCode::BadRequest, "token shorter than 32 chars"));
        }
        for o in &spec.outputs {
            check_decl(&o.topic, &o.schema)?;
        }
        for i in &spec.inputs {
            check_decl(&i.topic, &i.schema)?;
        }
        let h = token_hash(token);
        let mut g = self.lock();
        if g.instances.contains_key(&spec.instance_id) || g.by_hash.contains_key(&h) {
            return Err(bad(
                ErrorCode::BadRequest,
                "instance or token already registered",
            ));
        }
        g.by_hash.insert(h, spec.instance_id.clone());
        let (revoked, _) = watch::channel(false);
        g.instances.insert(
            spec.instance_id.clone(),
            Instance {
                spec,
                hash: h,
                revoked,
            },
        );
        Ok(())
    }

    /// Stop accepting an instance: its connections close, its subscriptions
    /// go, and the topics it wrote keep their counters with no writer.
    pub fn revoke(&self, instance_id: &str) -> bool {
        let mut g = self.lock();
        let Some(inst) = g.instances.remove(instance_id) else {
            return false;
        };
        g.by_hash.remove(&inst.hash);
        g.subs.retain(|_, s| s.instance != instance_id);
        for t in g.topics.values_mut() {
            if t.writer.as_deref() == Some(instance_id) {
                t.writer = None;
            }
        }
        g.svc.drop_instance(instance_id);
        self.conns.lock().unwrap_or_else(|p| p.into_inner()).remove(instance_id);
        g.rates.retain(|(i, _), _| i != instance_id);
        g.buckets.retain(|(i, _, _), _| i != instance_id);
        if let Some(b) = &self.blobs {
            b.drop_uploads(instance_id);
        }
        let _ = inst.revoked.send(true);
        true
    }

    /// The registration of `instance_id`, if live.
    pub fn instance(&self, instance_id: &str) -> Option<InstanceSpec> {
        self.lock()
            .instances
            .get(instance_id)
            .map(|i| i.spec.clone())
    }

    /// Open a session for `token` from a peer running as `peer_uid`.
    pub fn authenticate(&self, token: &str, peer_uid: Option<u32>) -> Result<Session, BusError> {
        let g = self.lock();
        let inst = g
            .by_hash
            .get(&token_hash(token))
            .and_then(|id| g.instances.get(id))
            .ok_or_else(|| {
                refuse(
                    "-",
                    "hello",
                    "-",
                    bad(ErrorCode::Unauthorized, "unknown token"),
                )
            })?;
        let id = &inst.spec.instance_id;
        if let Some(want) = inst.spec.uid
            && peer_uid != Some(want)
        {
            let m = format!("peer uid {peer_uid:?} is not the instance's uid {want}");
            return Err(refuse(id, "hello", "-", bad(ErrorCode::UidMismatch, m)));
        }
        let (id, project_id, revoked) = (id.clone(), inst.spec.project_id.clone(), inst.revoked.subscribe());
        let conn = svc::next_conn();
        let slot = {
            let mut open = self.conns.lock().unwrap_or_else(|p| p.into_inner());
            let held = open.entry(id.clone()).or_default();
            if held.len() >= self.cfg.max_conns_per_instance {
                let m = format!("{id} already has {} connections open", held.len());
                let none = held.is_empty();
                if none {
                    open.remove(&id);
                }
                return Err(refuse(&id, "hello", "-", bad(ErrorCode::Busy, m)));
            }
            held.insert(conn);
            crate::types::Slot { conns: self.conns.clone(), instance: id.clone(), conn }
        };
        let (push, pushed) = tokio::sync::mpsc::unbounded_channel();
        Ok(Session {
            conn,
            push,
            pushed: Some(pushed),
            instance_id: id,
            project_id,
            declared: None,
            subs: BTreeMap::new(),
            wake: Arc::new(Notify::new()),
            revoked,
            _slot: slot,
        })
    }

    fn spec_of<'a>(
        &self,
        g: &'a State,
        s: &Session,
        action: &str,
    ) -> Result<&'a InstanceSpec, BusError> {
        g.instances
            .get(&s.instance_id)
            .map(|i| &i.spec)
            .ok_or_else(|| {
                refuse(
                    &s.instance_id,
                    action,
                    "-",
                    bad(ErrorCode::Revoked, "instance revoked"),
                )
            })
    }

    /// Narrow the session to `outputs` / `inputs`, each of which must be in
    /// the manifest exactly.
    pub fn declare(
        &self,
        s: &mut Session,
        outputs: Vec<OutputSpec>,
        inputs: Vec<InputSpec>,
    ) -> Result<Value, BusError> {
        let g = self.lock();
        let spec = self.spec_of(&g, s, "declare")?;
        for o in &outputs {
            if !spec.outputs.contains(o) {
                let m = format!(
                    "output {} ({}, {:?}) is not in the manifest",
                    o.topic, o.schema, o.class
                );
                return Err(refuse(
                    &s.instance_id,
                    "declare",
                    &o.topic,
                    bad(ErrorCode::Undeclared, m),
                ));
            }
        }
        for i in &inputs {
            if !spec.inputs.contains(i) {
                let m = format!("input {} ({}) is not in the manifest", i.topic, i.schema);
                return Err(refuse(
                    &s.instance_id,
                    "declare",
                    &i.topic,
                    bad(ErrorCode::Undeclared, m),
                ));
            }
        }
        let spec = spec.clone();
        drop(g);
        self.ext.declare(&spec, &outputs, &inputs)?;
        let v = serde_json::json!({ "outputs": outputs.len(), "inputs": inputs.len() });
        s.declared = Some((outputs, inputs));
        Ok(v)
    }

    /// Publish one reading; returns its `seq`.
    pub fn publish(
        &self,
        s: &Session,
        kind: &str,
        source_id: &str,
        schema: &str,
        t_ns: u64,
        body: Value,
    ) -> Result<u64, BusError> {
        let topic = format!("{kind}/{source_id}");
        let me = s.instance_id.as_str();
        let no = |e| refuse(me, "publish", &topic, e);
        if !valid_topic_kind(kind) || !valid_source_id(source_id) {
            return Err(no(bad(
                ErrorCode::BadRequest,
                "bad topic kind or source id",
            )));
        }
        let size = serde_json::to_vec(&body)
            .map(|b| b.len())
            .unwrap_or(usize::MAX);
        if size > MAX_BODY {
            return Err(no(bad(
                ErrorCode::TooLarge,
                format!("body of {size} bytes exceeds {MAX_BODY}; larger is a blob"),
            )));
        }
        if kind.starts_with(svc::KIND_PREFIX) {
            return Err(no(bad(
                ErrorCode::Unsupported,
                format!("{kind} is a service; use serve and call"),
            )));
        }
        let mut g = self.lock();
        let spec = self.spec_of(&g, s, "publish")?;
        let outputs = s.declared.as_ref().map_or(&spec.outputs, |d| &d.0);
        let decl = outputs.iter().find(|o| o.topic == kind).ok_or_else(|| {
            no(bad(
                ErrorCode::Undeclared,
                format!("{kind} is not a declared output"),
            ))
        })?;
        if decl.schema != schema {
            let m = format!(
                "{kind} is declared with schema {}, not {schema}",
                decl.schema
            );
            return Err(no(bad(ErrorCode::SchemaMismatch, m)));
        }
        if decl.class != TopicClass::Reading {
            let m = format!("class {:?} is not in B1 readings; streams use stream_open", decl.class);
            return Err(no(bad(ErrorCode::Unsupported, m)));
        }
        let project = spec.project_id.clone();
        let cog = spec.cog_id.clone();
        let key = (kind.to_string(), source_id.to_string());
        if let Some(per_sec) = g.rates.get(&(me.to_string(), kind.to_string())).copied() {
            let now = std::time::Instant::now();
            let bucket = g
                .buckets
                .entry((me.to_string(), kind.to_string(), source_id.to_string()))
                .or_insert_with(|| crate::rate::Bucket::new(per_sec, now));
            if !bucket.take(now) {
                let m = format!("{topic} is declared at {per_sec} msg/s (up to {} allowed)", per_sec * crate::rate::SLACK);
                return Err(no(bad(ErrorCode::RateLimited, m)));
            }
        }
        let mine = g.topics.get(&key).and_then(|t| t.writer.as_deref()) == Some(me);
        if !mine {
            let mut req = BusRequest::new(BusAction::Publish, me, &cog, project.as_deref());
            req.topic_project = g.topics.get(&key).and_then(|t| t.project_id.as_deref());
            self.ext.gate(&req.on(&topic, schema))?;
        }
        let handles = self.handles_in(&body);
        if !handles.is_empty() {
            // A handle published is a handle handed to the readers this
            // reading will reach.
            let readers: Vec<String> = g
                .subs
                .values()
                .filter(|x| {
                    x.kind == kind
                        && x.source.as_deref().is_none_or(|src| src == source_id)
                        && x.schema == schema
                        && (x.project_id == project || granted(&x.also, &project, source_id))
                })
                .map(|x| x.instance.clone())
                .collect();
            for to in readers {
                self.grant_handles(&handles, me, &to);
            }
        }
        let r = Reading {
            kind: kind.to_string(),
            source_id: source_id.to_string(),
            schema: schema.to_string(),
            t_ns,
            body,
        };
        let put = Put {
            writer: me,
            instance: me,
            project,
            r,
            to_sink: true,
        };
        let sent = self.put(&mut g, put).map(|d| d.seq);
        drop(g);
        // The first reading of a topic announces it (no broker lock held).
        if matches!(sent, Ok(1)) {
            self.ext.hooks.sink.topic_opened(&crate::policy::TopicInfo {
                topic: format!("{kind}/{source_id}"),
                kind: kind.to_string(),
                source_id: source_id.to_string(),
                schema: schema.to_string(),
                class: TopicClass::Reading,
                instance: me.to_string(),
                cog_id: cog,
                project_id: s.project_id.clone(),
            });
        }
        sent
    }

    /// Subscribe to `topic` (`<kind>/<source_id>` or `<kind>/*`); returns
    /// the subscription id.
    pub fn subscribe(&self, s: &mut Session, topic: &str, schema: &str) -> Result<u64, BusError> {
        let me = s.instance_id.clone();
        let no = |e| refuse(&me, "subscribe", topic, e);
        let (kind, source) = topic
            .split_once('/')
            .filter(|(k, src)| valid_topic_kind(k) && (*src == "*" || valid_source_id(src)))
            .ok_or_else(|| {
                no(bad(
                    ErrorCode::BadRequest,
                    "topic must be <kind>/<source_id> or <kind>/*",
                ))
            })?;
        if kind.starts_with(svc::KIND_PREFIX) {
            return Err(no(bad(
                ErrorCode::Unsupported,
                format!("{kind} is a service; use serve and call"),
            )));
        }
        let held = s.subs.len() + self.streams.reader_count(s.conn);
        if held >= self.cfg.max_subs_per_conn {
            let m = format!("this connection already holds {held} subscriptions");
            return Err(no(bad(ErrorCode::Busy, m)));
        }
        let mut g = self.lock();
        let spec = self.spec_of(&g, s, "subscribe")?;
        let inputs = s.declared.as_ref().map_or(&spec.inputs, |d| &d.1);
        let decl = inputs.iter().find(|i| i.topic == kind).ok_or_else(|| {
            no(bad(
                ErrorCode::Undeclared,
                format!("{kind} is not a declared input"),
            ))
        })?;
        if decl.schema != schema {
            let m = format!(
                "{kind} is declared as input with schema {}, not {schema}",
                decl.schema
            );
            return Err(no(bad(ErrorCode::SchemaMismatch, m)));
        }
        let source = (source != "*").then(|| source.to_string());
        let pid = s.project_id.as_deref();
        let mut req =
            BusRequest::new(BusAction::Subscribe, &me, &spec.cog_id, pid).on(topic, schema);
        let also = self.ext.grants(&req, source.as_deref());
        if let Some(src) = &source
            && let Some(t) = g.topics.get(&(kind.to_string(), src.clone()))
        {
            if t.project_id != s.project_id {
                req.topic_project = t.project_id.as_deref();
            }
            if t.project_id != s.project_id && !granted(&also, &t.project_id, src) {
                return Err(no(bad(
                    ErrorCode::CrossProject,
                    format!("{topic} belongs to another project"),
                )));
            }
            if t.schema != schema {
                return Err(no(bad(
                    ErrorCode::SchemaMismatch,
                    format!("{topic} carries {}", t.schema),
                )));
            }
        }
        self.ext.gate(&req)?;
        let cap = match decl.policy {
            InputPolicy::Sample => 1,
            InputPolicy::DropOldest => self.cfg.queue_len,
            InputPolicy::Block => {
                tracing::info!(target: "cogbus", instance = %me, topic, "policy block is drop-oldest for readings in B1 (B2 owns block)");
                self.cfg.queue_len
            }
        };
        let queue = Arc::new(SubQueue::new(cap, s.wake.clone()));
        g.next_sub += 1;
        let id = g.next_sub;
        g.subs.insert(
            id,
            Sub {
                instance: me.clone(),
                project_id: s.project_id.clone(),
                kind: kind.to_string(),
                source,
                schema: schema.to_string(),
                queue: queue.clone(),
                also,
            },
        );
        drop(g);
        s.subs.insert(id, queue);
        self.ext.noted(s, BusAction::Subscribe, topic);
        Ok(id)
    }

    /// Drop one of this session's subscriptions.
    pub fn unsubscribe(&self, s: &mut Session, sub: u64) -> Result<(), BusError> {
        if s.subs.remove(&sub).is_none() {
            let e = bad(
                ErrorCode::UnknownSubscription,
                format!("no subscription {sub} on this connection"),
            );
            return Err(refuse(&s.instance_id, "unsubscribe", "-", e));
        }
        let topic = self.lock().subs.remove(&sub).map(|x| match &x.source {
            Some(src) => format!("{}/{src}", x.kind),
            None => format!("{}/*", x.kind),
        });
        let topic = topic.unwrap_or_else(|| "-".into());
        self.ext.noted(s, BusAction::Unsubscribe, &topic);
        Ok(())
    }

    /// The connection closed: drop its subscriptions.
    pub fn close(&self, s: &Session) {
        let mut g = self.lock();
        for id in s.subs.keys() {
            g.subs.remove(id);
        }
        g.svc.drop_conn(s.conn);

    }
}

fn check_decl(topic: &str, schema: &str) -> Result<(), BusError> {
    if !valid_topic_kind(topic) || !is_registered(schema) {
        return Err(bad(
            ErrorCode::BadRequest,
            format!("bad declaration {topic} / {schema}"),
        ));
    }
    Ok(())
}
