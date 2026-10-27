//! Built-in sinks and internal publishers (ADR-122 section 9).
//!
//! A sink is a built-in subscriber to `<kind>/*` of every project, for one of
//! the daemon's own services (the store sink takes `store.vector/*`). Each
//! reading it gets names the project of its topic and the instance that
//! published it, so the sink keeps projects apart the same way the topic
//! table does. A cog's reading reaches the sink through a bounded queue: when
//! the queue is full the reading is dropped for the sink and counted on the
//! topic; the cog never waits.
//!
//! An internal publisher (the ingest compatibility listener) is not a cog. It
//! writes as a broker-internal identity starting with `@`, which no instance
//! id can (instance ids start with a letter or digit), and is held to the
//! topic rules (one writer, one project, one schema per topic) but not to a
//! manifest, since it has none. Its reading is handed back to the caller
//! instead of being queued to the sink, so the caller can wait for the sink's
//! answer (the HTTP listener answers with the store's counts).

use clawft_types::cog_topics::{TopicClass, is_registered, valid_topic_kind};
use serde_json::Value;
use tokio::sync::mpsc;

use super::{Broker, State, Topic, bad, check_decl, now_ns};
use crate::proto::{Delivery, ErrorCode, MAX_BODY};
use crate::types::{BusError, refuse, valid_source_id};

/// A reading handed to a sink.
#[derive(Debug, Clone, PartialEq)]
pub struct SinkReading {
    /// Project of the topic (`None`: the node's own bucket).
    pub project_id: Option<String>,
    /// The cog instance the reading is from. For an internal publisher, the
    /// instance it published for (the one whose token posted to the bridge).
    pub instance_id: String,
    /// The reading as subscribers see it.
    pub delivery: Delivery,
}

/// One reading to publish.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
    /// Topic kind.
    pub kind: String,
    /// Source id.
    pub source_id: String,
    /// Registered schema id.
    pub schema: String,
    /// Producer's capture clock, nanoseconds.
    pub t_ns: u64,
    /// The reading body.
    pub body: Value,
}

pub(super) struct SinkSlot {
    pub(super) schema: String,
    tx: mpsc::Sender<SinkReading>,
}

pub(super) struct Put<'a> {
    pub(super) writer: &'a str,
    pub(super) instance: &'a str,
    pub(super) project: Option<String>,
    pub(super) r: Reading,
    pub(super) to_sink: bool,
    /// The node a remote reading came from (a proxy topic); else this one.
    pub(super) origin: Option<&'a str>,
    /// The source topic's `seq` for a remote reading, so a drop at the
    /// source is a gap here too.
    pub(super) seq: Option<u64>,
}

impl Broker {
    /// Add the built-in sink for `<kind>/*` with `schema` across every
    /// project, holding at most `cap` readings; the daemon drains the
    /// receiver. One sink per kind.
    pub fn add_sink(
        &self,
        kind: &str,
        schema: &str,
        cap: usize,
    ) -> Result<mpsc::Receiver<SinkReading>, BusError> {
        check_decl(kind, schema)?;
        let mut g = self.lock();
        if g.sinks.contains_key(kind) {
            return Err(bad(ErrorCode::BadRequest, format!("{kind} already has a sink")));
        }
        let (tx, rx) = mpsc::channel(cap.max(1));
        let schema = schema.to_string();
        g.sinks.insert(kind.to_string(), SinkSlot { schema, tx });
        Ok(rx)
    }

    /// Publish `r` as the internal identity `writer` (`@` first) for
    /// `instance_id` of `project_id`. Subscribers of that project get it as
    /// usual; the sink of its kind does not: the reading comes back for the
    /// caller to hand over.
    pub fn publish_internal(
        &self,
        writer: &str,
        instance_id: &str,
        project_id: Option<String>,
        r: Reading,
    ) -> Result<SinkReading, BusError> {
        let topic = format!("{}/{}", r.kind, r.source_id);
        let no = |e| refuse(writer, "publish", &topic, e);
        if !writer.starts_with('@') || writer.len() < 2 {
            return Err(no(bad(ErrorCode::BadRequest, "internal writers start with @")));
        }
        if !valid_topic_kind(&r.kind) || !valid_source_id(&r.source_id) || !is_registered(&r.schema)
        {
            let m = "bad topic kind, source id or schema";
            return Err(no(bad(ErrorCode::BadRequest, m)));
        }
        let size = serde_json::to_vec(&r.body).map_or(usize::MAX, |b| b.len());
        if size > MAX_BODY {
            let m = format!("body of {size} bytes exceeds {MAX_BODY}; larger is a blob");
            return Err(no(bad(ErrorCode::TooLarge, m)));
        }
        let put = Put {
            writer,
            instance: instance_id,
            project: project_id.clone(),
            r,
            to_sink: false,
            origin: None,
            seq: None,
        };
        let delivery = self.put(&mut self.lock(), put)?;
        Ok(SinkReading {
            project_id,
            instance_id: instance_id.to_string(),
            delivery,
        })
    }

    /// Write one reading to its topic and fan it out, once the publisher has
    /// passed its own checks.
    pub(super) fn put(&self, st: &mut State, p: Put<'_>) -> Result<Delivery, BusError> {
        let Put { writer, instance, project, r, to_sink, origin, seq } = p;
        let topic = format!("{}/{}", r.kind, r.source_id);
        let no = |e| refuse(writer, "publish", &topic, e);
        let key = (r.kind.clone(), r.source_id.clone());
        let t = st.topics.entry(key).or_insert_with(|| Topic {
            schema: r.schema.clone(),
            class: TopicClass::Reading,
            writer: None,
            project_id: project.clone(),
            seq: 0,
            messages: 0,
            drops: 0,
        });
        if t.writer.as_deref().is_some_and(|w| w != writer) {
            return Err(no(bad(
                ErrorCode::WriterConflict,
                format!("{topic} is written by another instance"),
            )));
        }
        if t.project_id != project {
            return Err(no(bad(
                ErrorCode::CrossProject,
                format!("{topic} belongs to another project"),
            )));
        }
        if t.schema != r.schema {
            return Err(no(bad(
                ErrorCode::SchemaMismatch,
                format!("{topic} carries {}", t.schema),
            )));
        }
        t.writer = Some(writer.to_string());
        t.seq = seq.unwrap_or(t.seq + 1);
        t.messages += 1;
        let kind = r.kind;
        let d = Delivery {
            sub: 0,
            topic: topic.clone(),
            schema: r.schema,
            source_id: r.source_id,
            node_id: origin.map_or_else(|| self.cfg.node_id.clone(), str::to_string),
            t_ns: r.t_ns,
            seq: t.seq,
            recv_ns: now_ns(),
            body: r.body,
        };
        for (id, sub) in &st.subs {
            if sub.matches(&kind, &d.source_id, t) && sub.queue.push(Delivery { sub: *id, ..d.clone() }) {
                t.drops += 1;
            }
        }
        if let Some(s) = st.sinks.get(&kind).filter(|s| to_sink && s.schema == d.schema) {
            let sr = SinkReading {
                project_id: project,
                instance_id: instance.to_string(),
                delivery: d.clone(),
            };
            if s.tx.try_send(sr).is_err() {
                t.drops += 1;
                tracing::warn!(target: "cogbus", topic, "sink queue full or closed; reading dropped for the sink");
            }
        }
        Ok(d)
    }
}
