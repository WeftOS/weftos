//! Opening a stream as its writer: the manifest checks, ring creation and
//! replacement, and the notices that follow.

use std::os::fd::AsFd;

use clawft_types::cog_topics::{InputPolicy, TopicClass, valid_topic_kind};
use serde_json::json;

use super::{Alive, Answer, Key, Ring, State, Streams, bad, dup, ring_id, shm_err};
use crate::proto::ErrorCode;
use crate::stream::SessionStream;
use crate::stream::ring;
use crate::stream::shm::{self, Mapping, Shm};
use crate::stream::wire::{Geometry, StreamNotice};
use crate::types::{BusError, InstanceSpec, Session, refuse, valid_source_id};

impl Streams {
    fn new_ring(
        &self,
        st: &mut State,
        g: Geometry,
        head: u64,
    ) -> Result<(Shm, Mapping, u64), BusError> {
        let bytes = g.bytes() as u64;
        if st.bytes + bytes > self.cfg.node_cap_bytes {
            let m = format!(
                "a {bytes}-byte ring would pass the node's {} stream bytes",
                self.cfg.node_cap_bytes
            );
            return Err(bad(ErrorCode::TooLarge, m));
        }
        let shm = shm::create(g.bytes()).map_err(shm_err)?;
        let map = Mapping::new(shm.rw.as_fd(), g.bytes(), true).map_err(shm_err)?;
        let id = ring_id();
        ring::init(&map, g, id, head);
        st.bytes += bytes;
        Ok((shm, map, id))
    }

    /// Open `<kind>/<source_id>` as its writer. `spec` is the instance's
    /// manifest; `s.declared` narrows it.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open(
        &self,
        spec: &InstanceSpec,
        s: &Session,
        ss: &SessionStream,
        kind: &str,
        source_id: &str,
        schema: &str,
        max_record: u64,
        ring_bytes: Option<u64>,
        seq_base: Option<u64>,
        alive: Alive<'_>,
    ) -> Result<Answer, BusError> {
        let topic = format!("{kind}/{source_id}");
        let me = s.instance_id.as_str();
        let no = |e| refuse(me, "stream_open", &topic, e);
        if !valid_topic_kind(kind) || !valid_source_id(source_id) {
            return Err(no(bad(
                ErrorCode::BadRequest,
                "bad topic kind or source id",
            )));
        }
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
        if decl.class != TopicClass::Stream {
            let m = format!("{kind} is declared as {:?}, not a stream", decl.class);
            return Err(no(bad(ErrorCode::Unsupported, m)));
        }
        if max_record == 0 || max_record > self.cfg.max_record {
            let m = format!("max_record must be 1..={}", self.cfg.max_record);
            return Err(no(bad(ErrorCode::TooLarge, m)));
        }
        let want = ring_bytes.unwrap_or(self.cfg.default_ring_bytes);
        if want > self.cfg.max_ring_bytes {
            let m = format!("ring of {want} bytes exceeds {}", self.cfg.max_ring_bytes);
            return Err(no(bad(ErrorCode::TooLarge, m)));
        }
        let g = Geometry::fit(max_record, want)
            .ok_or_else(|| no(bad(ErrorCode::BadRequest, "ring too small for two records")))?;
        let conn = ss.conn;
        let key = (kind.to_string(), source_id.to_string());
        let mut st = self.lock();
        st.conns.insert(conn, ss.tx.clone());
        let base = seq_base.unwrap_or(0);
        let replaced = match st.rings.get(&key) {
            Some(r) => {
                if r.project_id != s.project_id {
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
                if r.writer.as_deref().is_some_and(|w| w != me && alive(w)) {
                    return Err(no(bad(
                        ErrorCode::WriterConflict,
                        format!("{topic} is written by another instance"),
                    )));
                }
                if r.writer_conn.is_some_and(|c| c != conn) {
                    return Err(no(bad(
                        ErrorCode::WriterConflict,
                        format!("{topic} is open on another connection"),
                    )));
                }
                (r.g != g).then(|| ring::head(&r.map).max(base))
            }
            None => Some(base),
        };
        if let Some(head) = replaced {
            let old = st.rings.remove(&key);
            if let Some(o) = &old {
                st.bytes -= o.g.bytes() as u64;
            }
            let made = self.new_ring(&mut st, g, head);
            let (shm, map, ring_id) = match made {
                Ok(m) => m,
                Err(e) => {
                    if let Some(o) = old {
                        st.bytes += o.g.bytes() as u64;
                        st.rings.insert(key, o);
                    }
                    return Err(no(e));
                }
            };
            let demotions = old.map_or(0, |o| o.demotions);
            st.rings.insert(
                key.clone(),
                Ring {
                    schema: schema.to_string(),
                    project_id: s.project_id.clone(),
                    writer: None,
                    writer_conn: None,
                    shm,
                    map,
                    g,
                    ring_id,
                    demotions,
                },
            );
            self.announce_ring(&st, &key);
        }
        let r = st
            .rings
            .get_mut(&key)
            .ok_or_else(|| bad(ErrorCode::BadRequest, "ring vanished"))?;
        if replaced.is_none() && base > 0 {
            ring::raise_head(&r.map, base);
        }
        r.writer = Some(me.to_string());
        r.writer_conn = Some(conn);
        let fds = vec![dup(&r.shm.rw)?];
        let result = json!({
            "topic": topic, "ring_id": r.ring_id, "geometry": r.g, "head": ring::head(&r.map),
            "block_deadline_ms": self.cfg.block_deadline.as_millis() as u64, "fds": 1,
        });
        let st = &*st;
        for (id, sub) in &st.subs {
            if sub.key == key
                && sub.policy == InputPolicy::Block
                && let Some(c) = &sub.cursor
            {
                let n = StreamNotice::Reader {
                    topic: topic.clone(),
                    sub: *id,
                    added: true,
                    fds: 1,
                };
                st.notify(conn, n, vec![dup(&c.ro)?]);
            }
        }
        Ok(Answer { result, fds })
    }

    /// Tell every matching reader of `key` about its (new) ring.
    fn announce_ring(&self, st: &State, key: &Key) {
        let Some(r) = st.rings.get(key) else { return };
        for (id, sub) in &st.subs {
            if &sub.key != key || sub.schema != r.schema || sub.project_id != r.project_id {
                continue;
            }
            let Ok(fd) = r.shm.ro.try_clone() else {
                continue;
            };
            let n = StreamNotice::Ring {
                sub: *id,
                ring_id: r.ring_id,
                head: ring::head(&r.map),
                geometry: r.g,
                fds: 1,
            };
            st.notify(sub.conn, n, vec![fd]);
        }
    }
}
