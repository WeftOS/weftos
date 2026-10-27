//! The source side of mesh topics: serving this node's exposed topics to
//! admitted peers (ADR-122 section 14, B6b; streams B6d; blobs B6e).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Instant;

use clawft_types::cog_topics::{InputPolicy, MeshLimits, TopicClass};

use super::wire::{Advert, Credit, TopicData, TopicReq};
use super::{MeshAudience, MeshRecord, MeshTopicHub, exposure_id, is_raw, mesh_principal};
use crate::audit::code_name;
use crate::policy::{BusAction, BusActivity, BusRequest, Outcome};
use crate::proto::ErrorCode;
use crate::queue::SubQueue;

/// A byte or message budget refilled at a fixed rate, with a burst of one
/// second's worth (at least one unit).
#[derive(Debug, Clone)]
pub(crate) struct Budget {
    per_sec: f64,
    cap: f64,
    tokens: f64,
    last: Instant,
}

impl Budget {
    pub(crate) fn new(per_sec: f64, now: Instant) -> Self {
        let cap = per_sec.max(1.0);
        Self { per_sec, cap, tokens: cap, last: now }
    }

    pub(crate) fn take(&mut self, n: f64, now: Instant) -> bool {
        let dt = now.saturating_duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + dt * self.per_sec).min(self.cap);
        if self.tokens >= n {
            self.tokens -= n;
            true
        } else {
            false
        }
    }
}

/// One remote subscription this node serves.
pub(crate) struct Served {
    pub(crate) topic: String,
    /// A raw topic: re-checked against the raw rule on every pump.
    pub(crate) raw: bool,
    /// The writer and the topic's project (blobs it hands out are theirs).
    pub(crate) owner: String,
    pub(crate) project: Option<String>,
    pub(crate) reader: u64,
    pub(crate) queue: Arc<SubQueue>,
    pub(crate) credit: Credit,
    pub(crate) last_credit: Instant,
    pub(crate) limits: MeshLimits,
    pub(crate) readings: Budget,
    pub(crate) sent: u64,
    pub(crate) bytes: u64,
    pub(crate) drops: u64,
    pub(crate) started: Instant,
}

/// Served subscriptions by (peer, the peer's subscription id).
#[derive(Default)]
pub(crate) struct Source {
    served: Mutex<HashMap<(String, u64), Served>>,
}

impl Source {
    pub(crate) fn lock(&self) -> MutexGuard<'_, HashMap<(String, u64), Served>> {
        self.served.lock().unwrap_or_else(|p| p.into_inner())
    }
}

/// A refusal of a peer's subscribe.
struct No(ErrorCode, String);

fn no(code: ErrorCode, m: impl Into<String>) -> No {
    No(code, m.into())
}

impl MeshTopicHub {
    pub(crate) fn on_request(&self, peer: &str, req: TopicReq) {
        match req {
            TopicReq::Subscribe { sub, topic, schema, policy, raw, credit } => {
                let at = Instant::now();
                match self.admit(peer, sub, &topic, &schema, &policy, raw, credit, at) {
                    Ok(()) => self.note(peer, &topic, 0, at, "ok".into(), None),
                    Err(No(code, m)) => {
                        let c = code_name(code);
                        let d = TopicData::Error { sub, code: c.clone(), message: m.clone() };
                        self.send_data(peer, &d, &[]);
                        self.note(peer, &topic, 0, at, format!("refused:{c} {m}"), Some((c, m)));
                    }
                }
            }
            TopicReq::Credit { sub, credit } => {
                if let Some(s) = self.src.lock().get_mut(&(peer.to_string(), sub)) {
                    s.credit.n = s.credit.n.saturating_add(credit.n).min(self.cfg.window * 4);
                    s.credit.bytes = s.credit.bytes.saturating_add(credit.bytes).min(self.cfg.window_bytes * 4);
                    s.last_credit = Instant::now();
                }
            }
            TopicReq::Unsubscribe { sub } => {
                if let Some(s) = self.src.lock().remove(&(peer.to_string(), sub)) {
                    self.finish(peer, s, "unsubscribed");
                }
            }
            TopicReq::Fetch { req, hash, offset, len } => self.on_fetch(peer, req, hash, offset, len),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn admit(&self, peer: &str, sub: u64, topic: &str, schema: &str, policy: &str, raw: bool, credit: Credit, at: Instant) -> Result<(), No> {
        let (kind, source) = topic
            .split_once('/')
            .filter(|(k, s)| clawft_types::cog_topics::valid_topic_kind(k) && crate::types::valid_source_id(s))
            .ok_or_else(|| no(ErrorCode::BadRequest, "topic must be <kind>/<source_id>"))?;
        let t = self
            .broker
            .mesh_topic(kind, source)
            .filter(|t| t.writer.as_deref().is_some_and(|w| !w.starts_with('@')))
            .ok_or_else(|| no(ErrorCode::NotFound, format!("{topic} is not written by a cog on this node")))?;
        let writer = t.writer.clone().unwrap_or_default();
        let declared = self
            .broker
            .mesh_limits(&writer)
            .ok_or_else(|| no(ErrorCode::Unavailable, format!("{topic}: its cog declared no [mesh_limits]")))?;
        if t.schema != schema {
            return Err(no(ErrorCode::SchemaMismatch, format!("{topic} carries {}", t.schema)));
        }
        let id = exposure_id(topic);
        let view = self.exposures.serve(&id, peer).map_err(|why| no(ErrorCode::Denied, format!("{id} is not served to this node: {why}")))?;
        let principal = mesh_principal(peer);
        let mut req = BusRequest::new(BusAction::MeshSubscribe, &principal, "-", None).on(topic, schema);
        req.topic_project = t.project_id.as_deref();
        self.broker.ext.gate(&req).map_err(|e| no(e.code, e.message))?;
        let policy = InputPolicy::parse(policy).ok_or_else(|| no(ErrorCode::BadRequest, "unknown policy"))?;
        if policy == InputPolicy::Block {
            return Err(no(ErrorCode::Unsupported, "policy block is refused across nodes"));
        }
        if is_raw(t.class, &t.schema) {
            let named = matches!(view.audience, MeshAudience::Nodes(_));
            if !(view.raw && named && raw) {
                let why = if !raw {
                    "the subscribe did not ask for raw data"
                } else if !view.raw {
                    "the exposure is not switched on with --raw"
                } else {
                    "raw data is served only to an audience of named nodes"
                };
                return Err(no(ErrorCode::RawNotExposed, format!("{topic} is raw: {why}")));
            }
        }
        if t.class != TopicClass::Reading {
            return Err(no(ErrorCode::Unsupported, format!("{topic} is a {:?} topic; only readings cross the mesh here", t.class)));
        }
        let limits = declared.narrowed(view.rate_per_min, view.bytes_per_sec);
        let mut served = self.src.lock();
        if served.len() >= self.cfg.max_served {
            return Err(no(ErrorCode::Busy, "this node serves its limit of remote subscriptions"));
        }
        let held = served.keys().filter(|(p, _)| p == peer).count();
        if held >= limits.subscriptions_per_peer as usize {
            return Err(no(ErrorCode::Busy, format!("this node already serves you {held} subscriptions")));
        }
        if served.contains_key(&(peer.to_string(), sub)) {
            return Err(no(ErrorCode::BadRequest, format!("subscription {sub} is already open")));
        }
        let cap = if policy == InputPolicy::Sample { 1 } else { self.cfg.queue_len };
        let (reader, queue) = self
            .broker
            .mesh_open_reader(kind, source, cap, self.wake.clone())
            .ok_or_else(|| no(ErrorCode::NotFound, format!("{topic} went away")))?;
        let credit = Credit { n: credit.n.min(self.cfg.window * 4), bytes: credit.bytes.min(self.cfg.window_bytes * 4) };
        let readings = Budget::new(f64::from(limits.readings_per_min) / 60.0, at);
        served.insert(
            (peer.to_string(), sub),
            Served { topic: topic.to_string(), raw: is_raw(t.class, &t.schema), owner: writer.clone(), project: t.project_id.clone(), reader, queue, credit, last_credit: at, limits, readings, sent: 0, bytes: 0, drops: 0, started: at },
        );
        drop(served);
        self.broker.ext.allowed(BusAction::MeshSubscribe, &principal, None, topic);
        Ok(())
    }

    /// Serve pending readings against credit and limits; end subscriptions
    /// whose credit lapsed or whose exposure no longer covers the peer.
    pub(crate) fn pump_source(&self) {
        let now = Instant::now();
        let mut ended = Vec::new();
        let mut out = Vec::new();
        let mut hands = Vec::new();
        {
            let mut served = self.src.lock();
            for ((peer, sub), s) in served.iter_mut() {
                let lapse = std::time::Duration::from_secs(u64::from(s.limits.credit_timeout_s));
                if now.saturating_duration_since(s.last_credit) > lapse {
                    ended.push(((peer.clone(), *sub), "credit not renewed".to_string()));
                    continue;
                }
                match self.exposures.serve(&exposure_id(&s.topic), peer) {
                    Err(why) => {
                        ended.push(((peer.clone(), *sub), format!("withdrawn: {why}")));
                        continue;
                    }
                    Ok(v) if s.raw && !(v.raw && matches!(v.audience, MeshAudience::Nodes(_))) => {
                        ended.push(((peer.clone(), *sub), "withdrawn: raw is no longer exposed to this node".into()));
                        continue;
                    }
                    Ok(_) => {}
                }
                for d in s.queue.drain() {
                    if s.credit.n == 0 || !s.readings.take(1.0, now) {
                        s.drops += 1;
                        continue;
                    }
                    s.credit.n -= 1;
                    let handles = clawft_types::cog_topics::find_handles(&d.body);
                    if !handles.is_empty() {
                        let h = super::fetch::Handed {
                            exposure: exposure_id(&s.topic),
                            owner: s.owner.clone(),
                            project: s.project.clone(),
                            raw: s.raw,
                            bytes_per_sec: s.limits.bytes_per_sec,
                            expires_ms: 0,
                        };
                        hands.push((peer.clone(), handles, h));
                    }
                    let len = serde_json::to_vec(&d.body).map_or(0, |b| b.len() as u64);
                    out.push((peer.clone(), *sub, TopicData::Reading { sub: *sub, seq: d.seq, t_ns: d.t_ns, body: d.body }, len));
                }
            }
        }
        for (peer, handles, h) in hands {
            self.hand(&peer, handles, h);
        }
        for (peer, sub, d, len) in out {
            let sent = self.send_data(&peer, &d, &[]);
            if let Some(s) = self.src.lock().get_mut(&(peer, sub)) {
                if sent {
                    s.sent += 1;
                    s.bytes += len;
                } else {
                    s.drops += 1;
                }
            }
        }
        for (key, why) in ended {
            if let Some(s) = self.src.lock().remove(&key) {
                self.send_data(&key.0, &TopicData::End { sub: key.1, reason: why.clone() }, &[]);
                self.finish(&key.0, s, &why);
            }
        }
    }

    /// End every subscription served to `peer`.
    pub(crate) fn end_peer(&self, peer: &str, why: &str) {
        let gone: Vec<((String, u64), Served)> = {
            let mut g = self.src.lock();
            let keys: Vec<_> = g.keys().filter(|(p, _)| p == peer).cloned().collect();
            keys.into_iter().filter_map(|k| g.remove(&k).map(|s| (k, s))).collect()
        };
        for ((p, _), s) in gone {
            self.finish(&p, s, why);
        }
    }

    /// The operator switched `exposure` off or narrowed it: end every remote
    /// subscription of it now (local wins, ADR-120 section 4).
    pub fn withdraw(&self, exposure: &str) {
        let gone: Vec<((String, u64), Served)> = {
            let mut g = self.src.lock();
            let keys: Vec<_> = g.iter().filter(|(_, s)| exposure_id(&s.topic) == exposure).map(|(k, _)| k.clone()).collect();
            keys.into_iter().filter_map(|k| g.remove(&k).map(|s| (k, s))).collect()
        };
        for ((peer, sub), s) in gone {
            self.send_data(&peer, &TopicData::End { sub, reason: "withdrawn by the operator".into() }, &[]);
            self.finish(&peer, s, "withdrawn by the operator");
        }
    }

    /// Tell each peer which topics it may subscribe to now.
    pub fn announce(&self, peers: &[super::PeerInfo]) {
        let topics = self.broker.mesh_offered();
        for p in peers.iter().filter(|p| p.admitted) {
            let offered: Vec<Advert> = topics
                .iter()
                .filter_map(|(topic, schema, class)| {
                    let v = self.exposures.serve(&exposure_id(topic), &p.node_id).ok()?;
                    let project = match &v.audience {
                        MeshAudience::Project(id) => Some(id.clone()),
                        _ => None,
                    };
                    Some(Advert { topic: topic.clone(), schema: schema.clone(), class: class_name(*class).into(), project, raw: v.raw })
                })
                .collect();
            let ttl_s = self.cfg.advert_ttl.as_secs().max(1);
            self.send_data(&p.node_id, &TopicData::Advert { topics: offered, ttl_s }, &[]);
        }
    }

    /// Served subscriptions: (peer, topic, sent, drops), for `cogbus list`.
    pub fn served(&self) -> Vec<(String, String, u64, u64)> {
        let mut v: Vec<_> = self.src.lock().iter().map(|((p, _), s)| (p.clone(), s.topic.clone(), s.sent, s.drops)).collect();
        v.sort();
        v
    }

    fn finish(&self, peer: &str, s: Served, why: &str) {
        self.broker.mesh_close_reader(s.reader);
        let outcome = format!("ended:{why} sent {} dropped {}", s.sent, s.drops);
        self.note(peer, &s.topic, s.bytes, s.started, outcome, None);
    }

    fn note(&self, peer: &str, topic: &str, bytes_out: u64, since: Instant, outcome: String, refused: Option<(String, String)>) {
        let rec = MeshRecord {
            exposure: exposure_id(topic),
            caller_node: peer.to_string(),
            bytes_out,
            duration_ms: since.elapsed().as_millis() as u64,
            outcome,
        };
        self.exposures.record(rec);
        if let Some((code, reason)) = refused {
            // The cogbus activity log as well; a peer is not an instance, so
            // this never counts toward a cog's repeated-refusal stop.
            self.broker.ext.hooks.sink.record(&BusActivity {
                ts_ms: crate::audit::now_ms(),
                action: BusAction::MeshSubscribe.as_str().into(),
                instance: mesh_principal(peer),
                project: None,
                topic: topic.to_string(),
                outcome: Outcome::Refused { code, reason, suppressed: 0 },
            });
        }
    }
}

pub(crate) fn class_name(c: TopicClass) -> &'static str {
    match c {
        TopicClass::Reading => "reading",
        TopicClass::Stream => "stream",
        TopicClass::Blob => "blob",
    }
}

#[cfg(test)]
impl MeshTopicHub {
    /// Pretend `peer`'s subscription `sub` last renewed its credit `ago`.
    pub(crate) fn age_credit_for_test(&self, peer: &str, sub: u64, ago: std::time::Duration) {
        if let Some(s) = self.src.lock().get_mut(&(peer.to_string(), sub)) {
            s.last_credit = Instant::now().checked_sub(ago).unwrap_or(s.last_credit);
        }
    }
}
