//! Service calls (ADR-122 section 12): request/reply on `svc.<name>/<source_id>`.
//!
//! A service is a topic whose kind starts with `svc.`. Its provider lists the
//! kind as an `[[outputs]]` entry and its callers as an `[[inputs]]` entry
//! (same schema, class `reading`), so placement and the manifest checks that
//! guard readings guard calls too. One connection per topic `serve`s it; a
//! `call` is routed to it as a pushed [`CallDelivery`] and the caller's
//! request is answered when the provider replies, when the deadline passes
//! ([`Broker::expire_calls`], driven by the server) or when the provider goes
//! away. A call asks governance (`cogbus.call`) every time, `serve` once
//! (`cogbus.serve`); the broker holds no call state past its answer.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use clawft_types::cog_topics::TopicClass;
use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;

use super::{Broker, bad, now_ns};
use crate::audit::granted;
use crate::policy::{BusAction, BusRequest};
use crate::proto::{CallDelivery, DEFAULT_CALL_MS, ErrorCode, Event, MAX_BODY, MAX_CALL_MS};
use crate::types::{BusError, Session, refuse, valid_source_id};
use clawft_types::cog_topics::valid_topic_kind;

/// Topic kinds of services start with this.
pub(crate) const KIND_PREFIX: &str = "svc.";
/// Calls one service may have pending.
pub const MAX_PENDING_PER_SERVICE: usize = 64;
/// Calls one caller instance may have pending.
pub const MAX_PENDING_PER_CALLER: usize = 16;

static CONN: AtomicU64 = AtomicU64::new(1);

pub(crate) fn next_conn() -> u64 {
    CONN.fetch_add(1, Ordering::Relaxed)
}

struct Server {
    instance: String,
    conn: u64,
    project_id: Option<String>,
    schema: String,
    tx: UnboundedSender<Event>,
}

struct Pending {
    service: (String, String),
    caller: String,
    caller_conn: u64,
    caller_id: u64,
    caller_tx: UnboundedSender<Event>,
    server_conn: u64,
    deadline: Instant,
}

/// Providers and calls in flight.
#[derive(Default)]
pub(crate) struct Svc {
    servers: BTreeMap<(String, String), Server>,
    calls: HashMap<u64, Pending>,
    next: u64,
}

fn fail_call(p: &Pending, code: ErrorCode, message: &str) {
    let _ = p.caller_tx.send(Event::Error {
        id: p.caller_id,
        code,
        message: message.into(),
    });
}

impl Svc {
    fn pending_on(&self, svc: &(String, String)) -> usize {
        self.calls.values().filter(|p| &p.service == svc).count()
    }

    fn pending_from(&self, caller: &str) -> usize {
        self.calls.values().filter(|p| p.caller == caller).count()
    }

    /// Fail every call routed to a provider connection that matches `gone`.
    fn fail_provider_calls(&mut self, gone: impl Fn(&Pending) -> bool) {
        let ids: Vec<u64> = self.calls.iter().filter(|(_, p)| gone(p)).map(|(i, _)| *i).collect();
        for id in ids {
            if let Some(p) = self.calls.remove(&id) {
                fail_call(&p, ErrorCode::Unavailable, "the service went away");
            }
        }
    }

    /// The connection closed: it no longer serves or waits.
    pub(crate) fn drop_conn(&mut self, conn: u64) {
        self.servers.retain(|_, s| s.conn != conn);
        self.fail_provider_calls(|p| p.server_conn == conn);
        self.calls.retain(|_, p| p.caller_conn != conn);
    }

    /// The instance was revoked: all its connections go.
    pub(crate) fn drop_instance(&mut self, instance: &str) {
        let conns: Vec<u64> = self
            .servers
            .values()
            .filter(|s| s.instance == instance)
            .map(|s| s.conn)
            .collect();
        self.servers.retain(|_, s| s.instance != instance);
        self.fail_provider_calls(|p| conns.contains(&p.server_conn));
        self.calls.retain(|_, p| p.caller != instance);
    }
}

fn split_topic(topic: &str) -> Option<(&str, &str)> {
    topic
        .split_once('/')
        .filter(|(k, s)| k.starts_with(KIND_PREFIX) && valid_topic_kind(k) && valid_source_id(s))
}

impl Broker {
    /// Become the provider of `<kind>/<source_id>`.
    pub fn serve_topic(
        &self,
        s: &Session,
        kind: &str,
        source_id: &str,
        schema: &str,
    ) -> Result<Value, BusError> {
        let topic = format!("{kind}/{source_id}");
        let me = s.instance_id.as_str();
        let no = |e| refuse(me, "serve", &topic, e);
        if !kind.starts_with(KIND_PREFIX) || !valid_topic_kind(kind) || !valid_source_id(source_id)
        {
            return Err(no(bad(
                ErrorCode::BadRequest,
                "a service is svc.<name>/<source_id>",
            )));
        }
        let mut g = self.lock();
        let spec = self.spec_of(&g, s, "serve")?;
        let outputs = s.declared.as_ref().map_or(&spec.outputs, |d| &d.0);
        let decl = outputs.iter().find(|o| o.topic == kind).ok_or_else(|| {
            no(bad(ErrorCode::Undeclared, format!("{kind} is not a declared output")))
        })?;
        if decl.schema != schema {
            let m = format!("{kind} is declared with schema {}, not {schema}", decl.schema);
            return Err(no(bad(ErrorCode::SchemaMismatch, m)));
        }
        if decl.class != TopicClass::Reading {
            return Err(no(bad(ErrorCode::Unsupported, "a service output has class reading")));
        }
        let key = (kind.to_string(), source_id.to_string());
        if let Some(cur) = g.svc.servers.get(&key)
            && cur.instance != me
        {
            let m = format!("{topic} is served by another instance");
            return Err(no(bad(ErrorCode::WriterConflict, m)));
        }
        let (project, cog) = (spec.project_id.clone(), spec.cog_id.clone());
        let req = BusRequest::new(BusAction::Serve, me, &cog, project.as_deref());
        self.ext.gate(&req.on(&topic, schema))?;
        // A provider that reconnects replaces its own earlier connection.
        let old = g.svc.servers.insert(
            key,
            Server {
                instance: me.to_string(),
                conn: s.conn,
                project_id: project,
                schema: schema.to_string(),
                tx: s.push.clone(),
            },
        );
        if let Some(old) = old
            && old.conn != s.conn
        {
            g.svc.fail_provider_calls(|p| p.server_conn == old.conn);
        }
        drop(g);
        self.ext.noted(s, BusAction::Serve, &topic);
        Ok(json!({ "topic": topic }))
    }

    /// Route a call to the provider. On `Ok` the caller's request `req_id`
    /// is answered later, through the session's pushed frames.
    pub fn call_service(
        &self,
        s: &Session,
        req_id: u64,
        topic: &str,
        schema: &str,
        body: Value,
        timeout_ms: Option<u64>,
    ) -> Result<(), BusError> {
        let me = s.instance_id.as_str();
        let no = |e| refuse(me, "call", topic, e);
        let (kind, source) = split_topic(topic).ok_or_else(|| {
            no(bad(ErrorCode::BadRequest, "a call goes to svc.<name>/<source_id>"))
        })?;
        let size = serde_json::to_vec(&body).map(|b| b.len()).unwrap_or(usize::MAX);
        if size > MAX_BODY {
            let m = format!("body of {size} bytes exceeds {MAX_BODY}");
            return Err(no(bad(ErrorCode::TooLarge, m)));
        }
        let timeout = Duration::from_millis(timeout_ms.unwrap_or(DEFAULT_CALL_MS).clamp(1, MAX_CALL_MS));
        let mut g = self.lock();
        let spec = self.spec_of(&g, s, "call")?;
        let inputs = s.declared.as_ref().map_or(&spec.inputs, |d| &d.1);
        let decl = inputs.iter().find(|i| i.topic == kind).ok_or_else(|| {
            no(bad(ErrorCode::Undeclared, format!("{kind} is not a declared input")))
        })?;
        if decl.schema != schema {
            let m = format!("{kind} is declared as input with schema {}, not {schema}", decl.schema);
            return Err(no(bad(ErrorCode::SchemaMismatch, m)));
        }
        let key = (kind.to_string(), source.to_string());
        let server = g.svc.servers.get(&key).ok_or_else(|| {
            no(bad(ErrorCode::Unavailable, format!("nothing serves {topic}")))
        })?;
        if server.schema != schema {
            let m = format!("{topic} carries {}", server.schema);
            return Err(no(bad(ErrorCode::SchemaMismatch, m)));
        }
        let mut req = BusRequest::new(BusAction::Call, me, &spec.cog_id, s.project_id.as_deref())
            .on(topic, schema);
        if server.project_id != s.project_id {
            let also = self.ext.grants(&req, Some(source));
            if !granted(&also, &server.project_id, source) {
                let m = format!("{topic} belongs to another project");
                return Err(no(bad(ErrorCode::CrossProject, m)));
            }
            req.topic_project = server.project_id.as_deref();
        }
        self.ext.gate(&req)?;
        if g.svc.pending_on(&key) >= MAX_PENDING_PER_SERVICE
            || g.svc.pending_from(me) >= MAX_PENDING_PER_CALLER
        {
            return Err(no(bad(ErrorCode::Busy, "too many calls pending")));
        }
        g.svc.next += 1;
        let call = g.svc.next;
        let Some(server) = g.svc.servers.get(&key) else {
            return Err(no(bad(ErrorCode::Unavailable, format!("nothing serves {topic}"))));
        };
        let handles = self.handles_in(&body);
        if !handles.is_empty() {
            self.grant_handles(&handles, (s.project_id.as_deref(), me), (server.project_id.as_deref(), &server.instance));
        }
        let delivery = CallDelivery {
            call,
            topic: topic.to_string(),
            schema: schema.to_string(),
            from: me.to_string(),
            node_id: self.cfg.node_id.clone(),
            recv_ns: now_ns(),
            timeout_ms: timeout.as_millis() as u64,
            body,
        };
        if server.tx.send(Event::Call(delivery)).is_err() {
            return Err(no(bad(ErrorCode::Unavailable, format!("nothing serves {topic}"))));
        }
        let server_conn = server.conn;
        g.svc.calls.insert(
            call,
            Pending {
                service: key,
                caller: me.to_string(),
                caller_conn: s.conn,
                caller_id: req_id,
                caller_tx: s.push.clone(),
                server_conn,
                deadline: Instant::now() + timeout,
            },
        );
        drop(g);
        self.ext.noted(s, BusAction::Call, topic);
        Ok(())
    }

    /// The provider answers `call`; `Err(message)` is a service error.
    pub fn reply_call(
        &self,
        s: &Session,
        call: u64,
        result: Result<Value, String>,
    ) -> Result<(), BusError> {
        let me = s.instance_id.as_str();
        let no = |e| refuse(me, "reply", "-", e);
        let mut g = self.lock();
        let mine = g.svc.calls.get(&call).is_some_and(|p| p.server_conn == s.conn);
        if !mine {
            return Err(no(bad(
                ErrorCode::BadRequest,
                format!("no call {call} is waiting on this connection (answered, expired or not yours)"),
            )));
        }
        let Some(p) = g.svc.calls.remove(&call) else {
            return Err(no(bad(ErrorCode::BadRequest, format!("no call {call}"))));
        };
        drop(g);
        let ev = match result {
            Ok(body) => {
                let size = serde_json::to_vec(&body).map(|b| b.len()).unwrap_or(usize::MAX);
                if size > MAX_BODY {
                    fail_call(&p, ErrorCode::TooLarge, "the reply is over the body limit");
                    let m = format!("reply of {size} bytes exceeds {MAX_BODY}");
                    return Err(no(bad(ErrorCode::TooLarge, m)));
                }
                // A reply that carries the provider's handles hands them to
                // the caller (B5: this is how a result over 64 KiB travels).
                let handles = self.handles_in(&body);
                if !handles.is_empty() {
                    let caller_project = self.instance(&p.caller).and_then(|c| c.project_id);
                    self.grant_handles(&handles, (s.project_id.as_deref(), me), (caller_project.as_deref(), &p.caller));
                }
                Event::Ok {
                    id: p.caller_id,
                    result: json!({ "body": body }),
                }
            }
            Err(message) => Event::Error {
                id: p.caller_id,
                code: ErrorCode::ServiceError,
                message,
            },
        };
        let _ = p.caller_tx.send(ev);
        Ok(())
    }

    /// Answer every call past its deadline with a timeout; returns how many.
    /// The socket server calls this on a short timer.
    pub fn expire_calls(&self) -> usize {
        let now = Instant::now();
        let mut g = self.lock();
        let late: Vec<u64> = g
            .svc
            .calls
            .iter()
            .filter(|(_, p)| p.deadline <= now)
            .map(|(i, _)| *i)
            .collect();
        for id in &late {
            if let Some(p) = g.svc.calls.remove(id) {
                fail_call(&p, ErrorCode::Timeout, "the call timed out");
            }
        }
        late.len()
    }

    /// Calls in flight (for tests and `cogbus list`).
    pub fn pending_calls(&self) -> usize {
        self.lock().svc.calls.len()
    }
}
