//! The broker's side of mesh topics (ADR-122 section 14): what the
//! [`MeshTopicHub`](crate::mesh::MeshTopicHub) reads on a source node
//! (topics, the writer's `[mesh_limits]`, internal readers) and the
//! subscriber node's state (offers from peers, proxy topics, one mesh
//! subscription per proxy topic with its credit and backoff).

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clawft_types::cog_topics::{InputPolicy, MeshLimits, TopicClass, is_registered, valid_topic_kind};
use serde_json::Value;
use tokio::sync::Notify;

use super::{Broker, Sub, Topic, bad};
use crate::audit::granted;
use crate::mesh::wire::{Advert, Credit, TopicReq};
use crate::mesh::{MeshConfig, is_raw, proxy_writer};
use crate::policy::{BusAction, BusRequest};
use crate::proto::ErrorCode;
use crate::queue::SubQueue;
use crate::types::{BusError, Session, refuse, valid_source_id};
use super::sink::{Put, Reading};

/// A topic a peer offers this node.
pub(super) struct Offer {
    node: String,
    schema: String,
    class: TopicClass,
    project: Option<String>,
    expires: Instant,
}

pub(super) enum Phase {
    /// Subscribe at the next pump.
    Want,
    /// Subscribed; readings flow against credit.
    Open,
    /// Ended or refused; subscribe again at `retry_at`.
    Broken { retry_at: Instant, backoff: Duration, why: String },
}

/// One mesh subscription for one proxy topic.
pub(super) struct Remote {
    pub(super) node: String,
    pub(super) kind: String,
    pub(super) source: String,
    pub(super) schema: String,
    policy: InputPolicy,
    raw: bool,
    pub(super) phase: Phase,
    pub(super) outstanding: u64,
    pub(super) renewed: Instant,
    pub(super) received: u64,
    /// A stream topic: records land in a local proxy ring (B6d).
    pub(super) stream: Option<StreamRemote>,
}

/// The stream half of a mesh subscription.
pub(super) struct StreamRemote {
    /// Project the proxy ring belongs to here.
    pub(super) project: Option<String>,
    /// Stream bytes of credit still outstanding.
    pub(super) out_bytes: u64,
}

/// Mesh state of the broker.
pub(super) struct MeshState {
    offers: HashMap<(String, String), Offer>,
    pub(super) remote: BTreeMap<u64, Remote>,
    next: u64,
    pub(super) window: u64,
    pub(super) window_bytes: u64,
    renew_every: Duration,
    max_backoff: Duration,
    mesh_name: Option<String>,
    /// Per instance: its limits, and whether its manifest declared them.
    limits: HashMap<String, (MeshLimits, bool)>,
    pub(super) blobs: super::mesh_blobs::RemoteBlobs,
    pub(super) wake: Arc<Notify>,
}

impl Default for MeshState {
    fn default() -> Self {
        let cfg = MeshConfig::default();
        Self {
            offers: HashMap::new(),
            remote: BTreeMap::new(),
            next: 0,
            window: cfg.window,
            window_bytes: cfg.window_bytes,
            renew_every: cfg.renew_every,
            max_backoff: cfg.max_backoff,
            mesh_name: None,
            limits: HashMap::new(),
            blobs: Default::default(),
            wake: Arc::new(Notify::new()),
        }
    }
}

/// A live topic as the source side sees it.
pub(crate) struct TopicView {
    pub(crate) schema: String,
    pub(crate) class: TopicClass,
    pub(crate) writer: Option<String>,
    pub(crate) project_id: Option<String>,
}

/// `weftos://<mesh>/sensors|services/<source>/topics/<kind>` to
/// `(mesh, kind, source)`.
pub fn parse_mesh_topic(name: &str) -> Option<(&str, &str, &str)> {
    let rest = name.strip_prefix("weftos://")?;
    let mut p = rest.split('/');
    let (mesh, class, source, topics, kind) = (p.next()?, p.next()?, p.next()?, p.next()?, p.next()?);
    let ok = p.next().is_none()
        && !mesh.is_empty()
        && matches!(class, "sensors" | "services")
        && topics == "topics"
        && valid_source_id(source)
        && valid_topic_kind(kind);
    ok.then_some((mesh, kind, source))
}

impl Broker {
    /// Declare `instance`'s `[mesh_limits]` (after `register`); without them
    /// no topic it writes is served to another node. Revoke forgets them.
    pub fn set_mesh_limits(&self, instance: &str, limits: MeshLimits) {
        self.lock().mesh.limits.insert(instance.to_string(), (limits, true));
    }

    /// `instance`'s manifest declared no `[mesh_limits]` (a cog packed before
    /// the rule): it runs with the starting values, and none of its topics
    /// is served to another node until a version declares them.
    pub fn set_default_mesh_limits(&self, instance: &str) {
        self.lock().mesh.limits.insert(instance.to_string(), (MeshLimits::STARTING, false));
    }

    /// Whether `instance` declared its own `[mesh_limits]`.
    pub fn mesh_limits_declared(&self, instance: &str) -> bool {
        self.lock().mesh.limits.get(instance).is_some_and(|l| l.1)
    }

    /// Whether the cog writing `<kind>/<source_id>` declared `[mesh_limits]`,
    /// or why not (the `weaver expose on` check).
    pub fn topic_limits_declared(&self, topic: &str) -> Result<(), String> {
        let (kind, source) = topic.split_once('/').ok_or("not a <kind>/<source_id> topic")?;
        let writer = self
            .mesh_topic(kind, source)
            .and_then(|t| t.writer)
            .filter(|w| !w.starts_with('@'))
            .ok_or_else(|| format!("no cog on this node writes {topic} now"))?;
        if self.mesh_limits_declared(&writer) {
            Ok(())
        } else {
            Err(format!("{writer} (the cog writing {topic}) declares no [mesh_limits]; a cog sets its own limits before it is served to other nodes"))
        }
    }

    /// The `<mesh>` this node's `weftos://` names must carry (unset: any).
    pub fn set_mesh_name(&self, name: &str) {
        self.lock().mesh.mesh_name = Some(name.to_string());
    }

    pub(crate) fn mesh_wake(&self) -> Arc<Notify> {
        self.lock().mesh.wake.clone()
    }

    pub(crate) fn mesh_configure(&self, cfg: &MeshConfig) {
        let mut g = self.lock();
        g.mesh.window = cfg.window.max(2);
        g.mesh.window_bytes = cfg.window_bytes.max(2);
        g.mesh.renew_every = cfg.renew_every;
        g.mesh.max_backoff = cfg.max_backoff;
    }

    pub(super) fn mesh_forget(st: &mut super::State, instance: &str) {
        st.mesh.limits.remove(instance);
    }

    /// The `[mesh_limits]` `instance` registered, if any.
    pub fn mesh_limits(&self, instance: &str) -> Option<MeshLimits> {
        self.lock().mesh.limits.get(instance).map(|l| l.0)
    }

    pub(crate) fn mesh_topic(&self, kind: &str, source: &str) -> Option<TopicView> {
        {
            let g = self.lock();
            if let Some(t) = g.topics.get(&(kind.to_string(), source.to_string())) {
                return Some(TopicView { schema: t.schema.clone(), class: t.class, writer: t.writer.clone(), project_id: t.project_id.clone() });
            }
        }
        let topic = format!("{kind}/{source}");
        self.stream_rows(&|_| true).into_iter().find(|r| r.topic == topic).map(|r| TopicView {
            schema: r.schema,
            class: r.class,
            // A proxy ring of another node's stream is never served onward.
            writer: r.writer.map(|w| if w.starts_with("mesh-in-") { format!("@{w}") } else { w }),
            project_id: r.project_id,
        })
    }

    /// Topics written by a cog on this node (readings and stream rings):
    /// `(topic, schema, class)`.
    pub(crate) fn mesh_offered(&self) -> Vec<(String, String, TopicClass)> {
        let cog = |w: &Option<String>| w.as_deref().is_some_and(|w| !w.starts_with('@') && !w.starts_with("mesh-in-"));
        let mut v: Vec<_> = {
            let g = self.lock();
            g.topics
                .iter()
                .filter(|(_, t)| cog(&t.writer))
                .map(|((k, s), t)| (format!("{k}/{s}"), t.schema.clone(), t.class))
                .collect()
        };
        v.extend(self.stream_rows(&|_| true).into_iter().filter(|r| cog(&r.writer)).map(|r| (r.topic, r.schema, r.class)));
        v
    }

    /// A broker-internal reader of exactly one topic, in that topic's
    /// project (the exposure, not the project rule, decided access).
    pub(crate) fn mesh_open_reader(&self, kind: &str, source: &str, cap: usize, wake: Arc<Notify>) -> Option<(u64, Arc<SubQueue>)> {
        let mut g = self.lock();
        let t = g.topics.get(&(kind.to_string(), source.to_string()))?;
        let (project_id, schema) = (t.project_id.clone(), t.schema.clone());
        let queue = Arc::new(SubQueue::new(cap, wake));
        g.next_sub += 1;
        let id = g.next_sub;
        let sub = Sub { instance: "@mesh-out".into(), project_id, kind: kind.into(), source: Some(source.into()), schema, queue: queue.clone(), also: Vec::new() };
        g.subs.insert(id, sub);
        Some((id, queue))
    }

    pub(crate) fn mesh_close_reader(&self, id: u64) {
        self.lock().subs.remove(&id);
    }

    /// Subscribe session `s` to a topic on another node by its `weftos://`
    /// name: a proxy topic `<kind>/<source_id>` written by `@mesh:<node>`.
    pub(super) fn subscribe_mesh(&self, s: &mut Session, name: &str, schema: &str, raw: bool) -> Result<u64, BusError> {
        let me = s.instance_id.clone();
        let no = |e| refuse(&me, "subscribe", name, e);
        let (mesh, kind, source) = parse_mesh_topic(name)
            .ok_or_else(|| no(bad(ErrorCode::BadRequest, "name must be weftos://<mesh>/sensors/<source_id>/topics/<kind>")))?;
        if kind.starts_with("svc.") {
            return Err(no(bad(ErrorCode::Unsupported, format!("{kind} is a service; use serve and call"))));
        }
        let held = s.subs.len() + self.streams.reader_count(s.conn);
        if held >= self.cfg.max_subs_per_conn {
            return Err(no(bad(ErrorCode::Busy, format!("this connection already holds {held} subscriptions"))));
        }
        let g = self.lock();
        if g.mesh.mesh_name.as_deref().is_some_and(|m| m != mesh) {
            return Err(no(bad(ErrorCode::BadRequest, format!("{mesh} is not this node's mesh"))));
        }
        let spec = self.spec_of(&g, s, "subscribe")?.clone();
        let inputs = s.declared.as_ref().map_or(&spec.inputs, |d| &d.1);
        let decl = inputs
            .iter()
            .find(|i| i.topic == kind)
            .ok_or_else(|| no(bad(ErrorCode::Undeclared, format!("{kind} is not a declared input"))))?
            .clone();
        if decl.schema != schema {
            let m = format!("{kind} is declared as input with schema {}, not {schema}", decl.schema);
            return Err(no(bad(ErrorCode::SchemaMismatch, m)));
        }
        let key = (kind.to_string(), source.to_string());
        let now = Instant::now();
        let offer = g.mesh.offers.get(&key).filter(|o| o.expires > now).ok_or_else(|| {
            no(bad(ErrorCode::Unavailable, format!("no node offers {name} to this node now")))
        })?;
        let (node, project, class) = (offer.node.clone(), offer.project.clone(), offer.class);
        if offer.schema != schema {
            return Err(no(bad(ErrorCode::SchemaMismatch, format!("{name} carries {}", offer.schema))));
        }
        if class == TopicClass::Blob {
            return Err(no(bad(ErrorCode::Unsupported, format!("{name} is a blob topic; blobs travel by handle"))));
        }
        let stream = class == TopicClass::Stream;
        if is_raw(class, schema) && !raw {
            return Err(no(bad(ErrorCode::RawNotExposed, format!("{name} is raw: subscribe with raw: true"))));
        }
        let pid = s.project_id.as_deref();
        let mut req = BusRequest::new(BusAction::Subscribe, &me, &spec.cog_id, pid).on(name, schema);
        let also = self.ext.grants(&req, Some(source));
        if project != s.project_id {
            req.topic_project = project.as_deref();
            if !granted(&also, &project, source) {
                return Err(no(bad(ErrorCode::CrossProject, format!("{name} belongs to another project"))));
            }
        }
        let writer = proxy_writer(&node);
        if let Some(t) = g.topics.get(&key).filter(|_| !stream) {
            if t.writer.as_deref().is_some_and(|w| !w.starts_with("@mesh:")) {
                return Err(no(bad(ErrorCode::WriterConflict, format!("{kind}/{source} is written on this node"))));
            }
            if t.project_id != project || t.schema != schema {
                return Err(no(bad(ErrorCode::WriterConflict, format!("{kind}/{source} is already a different topic here"))));
            }
        }
        drop(g);
        self.ext.gate(&req)?;
        let mut g = self.lock();
        // A stream lands in a proxy ring the hub writes (B6d); the cog reads
        // it with `stream_subscribe` on `<kind>/<source_id>`.
        if !stream {
            let t = g.topics.entry(key.clone()).or_insert_with(|| Topic {
                schema: schema.to_string(),
                class: TopicClass::Reading,
                writer: None,
                project_id: project.clone(),
                seq: 0,
                messages: 0,
                drops: 0,
            });
            t.writer = Some(writer);
        }
        let cap = if decl.policy == InputPolicy::Sample { 1 } else { self.cfg.queue_len };
        let queue = Arc::new(SubQueue::new(cap, s.wake.clone()));
        g.next_sub += 1;
        let id = g.next_sub;
        let sub = Sub { instance: me.clone(), project_id: s.project_id.clone(), kind: kind.into(), source: Some(source.into()), schema: schema.into(), queue: queue.clone(), also };
        g.subs.insert(id, sub);
        let exists = g.mesh.remote.values().any(|r| r.kind == kind && r.source == source);
        if !exists {
            g.mesh.next += 1;
            let rid = g.mesh.next;
            let policy = if decl.policy == InputPolicy::Sample { InputPolicy::Sample } else { InputPolicy::DropOldest };
            let stream = stream.then(|| StreamRemote { project: project.clone(), out_bytes: 0 });
            let r = Remote { node, kind: kind.into(), source: source.into(), schema: schema.into(), policy, raw, phase: Phase::Want, outstanding: 0, renewed: now, received: 0, stream };
            g.mesh.remote.insert(rid, r);
        }
        let wake = g.mesh.wake.clone();
        drop(g);
        wake.notify_one();
        s.subs.insert(id, queue);
        self.ext.noted(s, BusAction::Subscribe, name);
        Ok(id)
    }

    /// `node`'s offer replaces what it offered before.
    pub(crate) fn mesh_advert(&self, node: &str, topics: Vec<Advert>, ttl: Duration) {
        let expires = Instant::now() + ttl;
        let mut g = self.lock();
        g.mesh.offers.retain(|_, o| o.node != node);
        for a in topics.into_iter().take(1024) {
            let Some((k, s)) = a.topic.split_once('/').filter(|(k, s)| valid_topic_kind(k) && valid_source_id(s)) else { continue };
            let Some(class) = TopicClass::parse(&a.class) else { continue };
            if !is_registered(&a.schema) || a.project.as_ref().is_some_and(|p| p.len() > 128) {
                continue;
            }
            let o = Offer { node: node.to_string(), schema: a.schema, class, project: a.project, expires };
            g.mesh.offers.insert((k.to_string(), s.to_string()), o);
        }
    }

    /// One reading from `node` on mesh subscription `rsub`; `None` if this
    /// node holds no such subscription to `node`, else the credit to grant.
    pub(crate) fn mesh_reading(&self, node: &str, rsub: u64, seq: u64, t_ns: u64, body: Value) -> Option<u64> {
        let mut g = self.lock();
        let window = g.mesh.window;
        let r = g.mesh.remote.get_mut(&rsub).filter(|r| r.node == node)?;
        r.phase = Phase::Open;
        r.received += 1;
        r.outstanding = r.outstanding.saturating_sub(1);
        let grant = if r.outstanding <= window / 2 {
            let n = window - r.outstanding;
            r.outstanding = window;
            r.renewed = Instant::now();
            n
        } else {
            0
        };
        let (kind, source, schema) = (r.kind.clone(), r.source.clone(), r.schema.clone());
        let project = g.topics.get(&(kind.clone(), source.clone())).and_then(|t| t.project_id.clone());
        let writer = proxy_writer(node);
        let readers: Vec<(Option<String>, String)> = g
            .subs
            .values()
            .filter(|x| !x.instance.starts_with('@') && x.kind == kind && x.source.as_deref() == Some(&source))
            .map(|x| (x.project_id.clone(), x.instance.clone()))
            .collect();
        Self::mesh_note_handles(&mut g, node, &project, &body, &readers);
        let reading = Reading { kind, source_id: source, schema, t_ns, body };
        let put = Put { writer: &writer, instance: &writer, project, r: reading, to_sink: false, origin: Some(node), seq: Some(seq) };
        if let Err(e) = self.put(&mut g, put) {
            tracing::debug!(target: "cogbus", node, code = ?e.code, "remote reading not delivered");
        }
        Some(grant)
    }

    /// The source ended or refused `rsub`: retry with backoff.
    pub(crate) fn mesh_ended(&self, node: &str, rsub: u64, why: &str) {
        let mut g = self.lock();
        let max = g.mesh.max_backoff;
        if let Some(r) = g.mesh.remote.get_mut(&rsub).filter(|r| r.node == node) {
            let backoff = match &r.phase {
                Phase::Broken { backoff, .. } => (*backoff * 2).min(max),
                _ => Duration::from_secs(1).min(max),
            };
            tracing::info!(target: "cogbus", node, topic = %format!("{}/{}", r.kind, r.source), why, "mesh subscription ended; retrying");
            r.phase = Phase::Broken { retry_at: Instant::now() + backoff, backoff, why: why.to_string() };
        }
    }

    /// The link to `node` is down: its offers go, its subscriptions retry.
    pub(crate) fn mesh_peer_down(&self, node: &str) {
        let remote: Vec<u64> = {
            let mut g = self.lock();
            g.mesh.offers.retain(|_, o| o.node != node);
            g.mesh.remote.iter().filter(|(_, r)| r.node == node).map(|(id, _)| *id).collect()
        };
        for id in remote {
            self.mesh_ended(node, id, "link down");
        }
    }

    /// Frames the subscriber side owes peers now.
    pub(crate) fn mesh_due(&self, now: Instant) -> Vec<(String, TopicReq)> {
        let mut g = self.lock();
        let st = &mut *g;
        let mut out = Vec::new();
        let mut gone = Vec::new();
        for (id, r) in st.mesh.remote.iter_mut() {
            let readers = st.subs.values().filter(|x| !x.instance.starts_with('@') && x.kind == r.kind && x.source.as_deref() == Some(&r.source)).count();
            if readers == 0 {
                if matches!(r.phase, Phase::Open) {
                    out.push((r.node.clone(), TopicReq::Unsubscribe { sub: *id }));
                }
                gone.push((*id, (r.kind.clone(), r.source.clone())));
                continue;
            }
            let due = match &r.phase {
                Phase::Want => true,
                Phase::Broken { retry_at, .. } => *retry_at <= now,
                Phase::Open => {
                    if now.saturating_duration_since(r.renewed) >= st.mesh.renew_every {
                        r.renewed = now;
                        out.push((r.node.clone(), TopicReq::Credit { sub: *id, credit: Credit::default() }));
                    }
                    false
                }
            };
            let offer = st.mesh.offers.get(&(r.kind.clone(), r.source.clone())).filter(|o| o.expires > now && o.schema == r.schema);
            if !due || offer.is_none() {
                continue;
            }
            // The source may have moved to another node; follow its offer.
            if let Some(o) = offer {
                r.node = o.node.clone();
            }
            if let Some(t) = st.topics.get_mut(&(r.kind.clone(), r.source.clone())) {
                t.writer = Some(proxy_writer(&r.node));
            }
            r.phase = Phase::Open;
            r.outstanding = st.mesh.window;
            let bytes = r.stream.as_mut().map_or(0, |x| {
                x.out_bytes = st.mesh.window_bytes;
                x.out_bytes
            });
            r.renewed = now;
            let policy = if r.policy == InputPolicy::Sample { "sample" } else { "drop-oldest" };
            let req = TopicReq::Subscribe {
                sub: *id,
                topic: format!("{}/{}", r.kind, r.source),
                schema: r.schema.clone(),
                policy: policy.into(),
                raw: r.raw,
                credit: Credit { n: st.mesh.window, bytes },
            };
            out.push((r.node.clone(), req));
        }
        for (id, key) in gone {
            st.mesh.remote.remove(&id);
            if let Some(t) = st.topics.get_mut(&key).filter(|t| t.writer.as_deref().is_some_and(|w| w.starts_with("@mesh:"))) {
                t.writer = None;
            }
        }
        out
    }

    /// Mesh subscriptions of this node: `(topic, node, state, received)`.
    pub fn mesh_remote_rows(&self) -> Vec<(String, String, String, u64)> {
        self.lock()
            .mesh
            .remote
            .values()
            .map(|r| {
                let state = match &r.phase {
                    Phase::Want => "pending".to_string(),
                    Phase::Open => "open".to_string(),
                    Phase::Broken { why, .. } => format!("retrying: {why}"),
                };
                (format!("{}/{}", r.kind, r.source), r.node.clone(), state, r.received)
            })
            .collect()
    }
}

