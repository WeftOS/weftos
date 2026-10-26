//! The broker's stream state: one ring per `<kind>/<source_id>`, its
//! writer, its readers, and the descriptors handed to each.
//!
//! A revoked instance is noticed lazily: callers pass `alive`, which asks the
//! broker whether an instance is still registered. Lock order: this lock may
//! be held while `alive` takes the broker's lock, never the other way round.

use std::collections::{BTreeMap, HashMap};
use std::os::fd::OwnedFd;
use std::sync::{Mutex, MutexGuard};

use clawft_types::cog_topics::{InputPolicy, TopicClass, valid_topic_kind};
use serde_json::{Value, json};

use super::ring;
use super::shm::{self, Mapping, Shm};
use super::wire::{Geometry, StreamNotice};
use super::{Outgoing, STREAM_SUB_BASE, SessionStream, StreamConfig};
use crate::audit::granted;
use crate::policy::Grant;
use crate::proto::{ErrorCode, Event};
use crate::types::{BusError, InstanceSpec, Session, TopicRow, refuse, valid_source_id};

mod open;

type Key = (String, String);
/// Is this instance still registered with the broker?
pub(crate) type Alive<'a> = &'a dyn Fn(&str) -> bool;
/// Governance for an open, given the live topic's project when it is not
/// the instance's (B3's gate, after the manifest rules).
pub(crate) type Gate<'a> = &'a dyn Fn(Option<&str>) -> Result<(), BusError>;
/// Governance for a subscribe: the grants it may use, or the refusal.
pub(crate) type Gov<'a> = &'a dyn Fn(Option<&str>) -> Result<Vec<Grant>, BusError>;

struct Ring {
    schema: String,
    project_id: Option<String>,
    writer: Option<String>,
    writer_conn: Option<u64>,
    shm: Shm,
    map: Mapping,
    g: Geometry,
    ring_id: u64,
    demotions: u64,
}

struct Sub {
    instance: String,
    project_id: Option<String>,
    conn: u64,
    key: Key,
    schema: String,
    policy: InputPolicy,
    cursor: Option<Shm>,
    also: Vec<Grant>,
}

impl Sub {
    /// May this subscription read a ring of `project`?
    fn reads(&self, project: &Option<String>) -> bool {
        project == &self.project_id || granted(&self.also, project, &self.key.1)
    }
}

#[derive(Default)]
struct State {
    rings: BTreeMap<Key, Ring>,
    subs: BTreeMap<u64, Sub>,
    conns: HashMap<u64, tokio::sync::mpsc::UnboundedSender<Outgoing>>,
    next_sub: u64,
    bytes: u64,
}

/// An answer and the descriptors that go with it.
pub(crate) struct Answer {
    pub result: Value,
    pub fds: Vec<OwnedFd>,
}

fn bad(code: ErrorCode, m: impl Into<String>) -> BusError {
    BusError::new(code, m)
}

fn dup(fd: &OwnedFd) -> Result<OwnedFd, BusError> {
    fd.try_clone()
        .map_err(|e| bad(ErrorCode::BadRequest, format!("dup: {e}")))
}

fn shm_err(e: std::io::Error) -> BusError {
    let code = if e.kind() == std::io::ErrorKind::Unsupported {
        ErrorCode::Unsupported
    } else {
        ErrorCode::BadRequest
    };
    bad(code, format!("shared memory: {e}"))
}

fn ring_id() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let h = blake3::hash(&[super::now_ns().to_le_bytes(), n.to_le_bytes()].concat());
    u64::from_le_bytes(h.as_bytes()[..8].try_into().unwrap_or_default())
}

impl State {
    fn notify(&self, conn: u64, n: StreamNotice, fds: Vec<OwnedFd>) {
        if let Some(tx) = self.conns.get(&conn) {
            let _ = tx.send(Outgoing {
                ev: Event::Stream(n),
                fds,
            });
        }
    }

    /// Drop rings with no live writer instance and no readers.
    fn gc(&mut self, alive: Alive<'_>) {
        let subs = &self.subs;
        let mut freed = 0u64;
        self.rings.retain(|k, r| {
            let keep = r.writer.as_deref().is_some_and(alive) || subs.values().any(|s| &s.key == k);
            if !keep {
                freed += r.g.bytes() as u64;
            }
            keep
        });
        self.bytes -= freed;
    }

    fn drop_sub(&mut self, id: u64) {
        let Some(sub) = self.subs.remove(&id) else {
            return;
        };
        if sub.policy == InputPolicy::Block
            && let Some(c) = self.rings.get(&sub.key).and_then(|r| r.writer_conn)
        {
            let topic = format!("{}/{}", sub.key.0, sub.key.1);
            let n = StreamNotice::Reader {
                topic,
                sub: id,
                added: false,
                fds: 0,
            };
            self.notify(c, n, Vec::new());
        }
    }
}

/// Stream rings of one broker.
pub(crate) struct Streams {
    cfg: StreamConfig,
    st: Mutex<State>,
}

impl Streams {
    pub(crate) fn new(cfg: StreamConfig) -> Self {
        Self {
            cfg,
            st: Mutex::new(State {
                next_sub: STREAM_SUB_BASE,
                ..State::default()
            }),
        }
    }

    pub(crate) fn deadline_ms(&self) -> u64 {
        self.cfg.block_deadline.as_millis() as u64
    }

    fn lock(&self) -> MutexGuard<'_, State> {
        self.st.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Stream readers held by connection `conn`.
    pub(crate) fn reader_count(&self, conn: u64) -> usize {
        self.lock().subs.values().filter(|x| x.conn == conn).count()
    }

    /// Read `<kind>/<source_id>`.
    pub(crate) fn subscribe(
        &self,
        spec: &InstanceSpec,
        s: &Session,
        ss: &SessionStream,
        topic: &str,
        schema: &str,
        gov: Gov<'_>,
    ) -> Result<Answer, BusError> {
        let me = s.instance_id.as_str();
        let no = |e| refuse(me, "stream_subscribe", topic, e);
        let (kind, source) = topic.split_once('/').ok_or_else(|| {
            no(bad(
                ErrorCode::BadRequest,
                "topic must be <kind>/<source_id>",
            ))
        })?;
        if source == "*" {
            return Err(no(bad(
                ErrorCode::Unsupported,
                "a stream subscription names one source",
            )));
        }
        if !valid_topic_kind(kind) || !valid_source_id(source) {
            return Err(no(bad(
                ErrorCode::BadRequest,
                "bad topic kind or source id",
            )));
        }
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
        let key = (kind.to_string(), source.to_string());
        let mut st = self.lock();
        st.conns.insert(ss.conn, ss.tx.clone());
        let foreign = st.rings.get(&key).and_then(|r| r.project_id.clone());
        let foreign = foreign.filter(|p| Some(p) != s.project_id.as_ref());
        let also = gov(foreign.as_deref())?;
        let mut fds = Vec::new();
        let mut ring_info = Value::Null;
        if let Some(r) = st.rings.get(&key) {
            if r.project_id != s.project_id && !granted(&also, &r.project_id, source) {
                return Err(no(bad(
                    ErrorCode::CrossProject,
                    format!("{topic} belongs to another project"),
                )));
            }
            if r.schema != schema {
                return Err(no(bad(
                    ErrorCode::SchemaMismatch,
                    format!("{topic} carries {}", r.schema),
                )));
            }
            fds.push(dup(&r.shm.ro)?);
            ring_info =
                json!({ "ring_id": r.ring_id, "geometry": r.g, "head": ring::head(&r.map) });
        }
        let cursor = match decl.policy {
            InputPolicy::Block => {
                Some(shm::create(ring::CURSOR_BYTES).map_err(|e| no(shm_err(e)))?)
            }
            _ => None,
        };
        if let Some(c) = &cursor {
            fds.push(dup(&c.rw)?);
        }
        st.next_sub += 1;
        let id = st.next_sub;
        if let (Some(c), Some(w)) = (&cursor, st.rings.get(&key).and_then(|r| r.writer_conn)) {
            let n = StreamNotice::Reader {
                topic: topic.to_string(),
                sub: id,
                added: true,
                fds: 1,
            };
            st.notify(w, n, vec![dup(&c.ro)?]);
        }
        st.subs.insert(
            id,
            Sub {
                instance: me.to_string(),
                project_id: s.project_id.clone(),
                conn: ss.conn,
                key,
                schema: schema.to_string(),
                policy: decl.policy,
                cursor,
                also,
            },
        );
        let result = json!({
            "sub": id, "topic": topic, "source_id": source, "policy": decl.policy,
            "ring": ring_info, "cursor": decl.policy == InputPolicy::Block, "fds": fds.len(),
        });
        Ok(Answer { result, fds })
    }

    /// The writer of `sub`'s ring reports that `block` reader `sub` held it
    /// past the deadline: it becomes `drop-oldest`, logged, and told.
    pub(crate) fn demote(
        &self,
        s: &Session,
        ss: &SessionStream,
        sub: u64,
    ) -> Result<Value, BusError> {
        let me = s.instance_id.as_str();
        let mut st = self.lock();
        let Some(key) = st.subs.get(&sub).map(|x| x.key.clone()) else {
            let e = bad(
                ErrorCode::UnknownSubscription,
                format!("no stream subscription {sub}"),
            );
            return Err(refuse(me, "stream_demote", "-", e));
        };
        let topic = format!("{}/{}", key.0, key.1);
        if st.rings.get(&key).and_then(|r| r.writer_conn) != Some(ss.conn) {
            let e = bad(
                ErrorCode::BadRequest,
                "only the topic's writer reports its readers",
            );
            return Err(refuse(me, "stream_demote", &topic, e));
        }
        let deadline = self.cfg.block_deadline.as_millis();
        let Some(x) = st.subs.get_mut(&sub) else {
            return Ok(json!({}));
        };
        if x.policy != InputPolicy::Block {
            return Ok(json!({ "policy": x.policy }));
        }
        x.policy = InputPolicy::DropOldest;
        let (reader, conn, project) = (x.instance.clone(), x.conn, x.project_id.clone());
        if let Some(r) = st.rings.get_mut(&key) {
            r.demotions += 1;
        }
        tracing::warn!(target: "cogbus", writer = me, reader = %reader, topic = %topic, sub,
            deadline_ms = deadline as u64, "block reader held the writer past the deadline; now drop-oldest");
        let reason = format!("held the writer past the {deadline} ms block deadline");
        st.notify(
            conn,
            StreamNotice::Policy {
                sub,
                policy: InputPolicy::DropOldest,
                reason,
            },
            Vec::new(),
        );
        Ok(json!({
            "policy": InputPolicy::DropOldest, "reader": reader, "project": project, "topic": topic,
        }))
    }

    /// True if `sub` is a stream subscription of this connection.
    pub(crate) fn owns(&self, ss: &SessionStream, sub: u64) -> bool {
        sub > STREAM_SUB_BASE
            && self
                .lock()
                .subs
                .get(&sub)
                .is_some_and(|x| x.conn == ss.conn)
    }

    pub(crate) fn unsubscribe(
        &self,
        s: &Session,
        ss: &SessionStream,
        sub: u64,
        alive: Alive<'_>,
    ) -> Result<String, BusError> {
        if !self.owns(ss, sub) {
            let e = bad(
                ErrorCode::UnknownSubscription,
                format!("no subscription {sub} on this connection"),
            );
            return Err(refuse(&s.instance_id, "unsubscribe", "-", e));
        }
        let mut st = self.lock();
        let topic = st
            .subs
            .get(&sub)
            .map(|x| format!("{}/{}", x.key.0, x.key.1));
        st.drop_sub(sub);
        st.gc(alive);
        Ok(topic.unwrap_or_else(|| "-".into()))
    }

    /// The connection closed: its readers go, its rings lose their writer
    /// connection but stay readable. (Revoking an instance closes its
    /// connections, so this is also where a revoked instance's state goes.)
    pub(crate) fn close(&self, ss: &SessionStream, alive: Alive<'_>) {
        let conn = ss.conn;
        let mut st = self.lock();
        st.conns.remove(&conn);
        let ids: Vec<u64> = st
            .subs
            .iter()
            .filter(|(_, x)| x.conn == conn)
            .map(|(i, _)| *i)
            .collect();
        for id in ids {
            st.drop_sub(id);
        }
        for r in st.rings.values_mut() {
            if r.writer_conn == Some(conn) {
                r.writer_conn = None;
            }
        }
        st.gc(alive);
    }

    /// `cogbus list` rows: `messages` is the ring's head seq, `drops` the
    /// `block` readers demoted (reader-side skips are counted by readers).
    pub(crate) fn rows(
        &self,
        keep: &dyn Fn(Option<&String>) -> bool,
        alive: Alive<'_>,
    ) -> Vec<TopicRow> {
        let st = self.lock();
        st.rings
            .iter()
            .filter(|(_, r)| keep(r.project_id.as_ref()))
            .map(|(k, r)| TopicRow {
                topic: format!("{}/{}", k.0, k.1),
                schema: r.schema.clone(),
                class: TopicClass::Stream,
                writer: r.writer.clone().filter(|w| alive(w)),
                project_id: r.project_id.clone(),
                subscribers: st.subs.values().filter(|x| &x.key == k).count(),
                messages: ring::head(&r.map),
                drops: r.demotions,
            })
            .collect()
    }

    fn bytes(&self) -> u64 {
        self.lock().bytes
    }
}

impl crate::Broker {
    /// This broker with stream tunables `cfg` (before any stream opens).
    pub fn with_stream_config(mut self, cfg: StreamConfig) -> Self {
        self.streams = Streams::new(cfg);
        self
    }

    /// Shared memory held by stream rings on this node (node facts).
    pub fn stream_bytes(&self) -> u64 {
        self.streams.bytes()
    }

    pub(crate) fn alive(&self, instance: &str) -> bool {
        self.instance(instance).is_some()
    }

    pub(crate) fn stream_rows(&self, keep: &dyn Fn(Option<&String>) -> bool) -> Vec<TopicRow> {
        self.streams.rows(keep, &|i| self.alive(i))
    }
}
