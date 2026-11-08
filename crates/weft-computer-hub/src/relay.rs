//! Request and notification handlers: what a harness may ask, what a tool
//! server may send, and how the two are joined. Split from `hub.rs` for size.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use serde_json::{Value, json};
use xai_tool_protocol::{
    ServerId, ServerInfo, ServersListResult, ToolServerLifecycleStatus, WorkspaceGonePhase,
    WorkspaceGoneReason,
};

use crate::hub::{BIND_TIMEOUT, ConnId, Hub, MAX_PENDING, Pending, Session, State, send};
use crate::wire;

impl Hub {
    pub(crate) fn harness_request(
        self: &Arc<Self>,
        cid: ConnId,
        method: &str,
        id: &Value,
        v: &Value,
    ) {
        let session = v
            .get("session_id")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let sess = session.as_deref();
        let params = v.get("params").cloned().unwrap_or(Value::Null);
        // Capture the sender up front: `reply` must not lock state, because
        // several arms below already hold the guard.
        let Some(tx) = self.st().conns.get(&cid).map(|c| c.tx.clone()) else {
            return;
        };
        let reply = |frame: String| send(&tx, frame);
        match method {
            "session_open" => {
                let Some(sid) = sess else {
                    return reply(wire::invalid_params(id, None, "session_id required"));
                };
                if xai_tool_protocol::SessionId::new(sid).is_err() {
                    return reply(wire::invalid_params(id, sess, "invalid session_id"));
                }
                let mut st = self.st();
                let user = st
                    .conns
                    .get(&cid)
                    .map(|c| c.user.clone())
                    .unwrap_or_default();
                match st.sessions.get_mut(sid) {
                    Some(s) if s.user != user => {
                        reply(wire::forbidden(id, sess, "session belongs to another user"))
                    }
                    Some(s) => {
                        // Same user reopening (reconnect replay): take ownership, keep the binding.
                        s.owner = cid;
                        reply(wire::ok(id, sess, json!({})));
                    }
                    None => {
                        st.sessions.insert(
                            sid.to_owned(),
                            Session {
                                owner: cid,
                                user,
                                server: None,
                                tools: vec![],
                            },
                        );
                        reply(wire::ok(id, sess, json!({})));
                    }
                }
            }
            "session_close" | "session_detach" => {
                let Some(sid) = sess else {
                    return reply(wire::invalid_params(id, None, "session_id required"));
                };
                let mut st = self.st();
                if st.sessions.get(sid).is_some_and(|s| s.owner == cid) {
                    Self::release_session(&mut st, sid);
                }
                reply(wire::ok(id, sess, json!({})));
            }
            "servers.list" => {
                let st = self.st();
                let user = st.conns.get(&cid).map(|c| c.user.as_str()).unwrap_or("");
                let mut servers: Vec<ServerInfo> = st
                    .servers
                    .iter()
                    .filter_map(|(sid, c)| st.conns.get(c).map(|c| (sid, c)))
                    .filter(|(_, c)| c.user == user)
                    .filter_map(|(sid, c)| {
                        Some(ServerInfo {
                            server_id: ServerId::new(sid.clone()).ok()?,
                            session_id: None,
                            description: c.description.clone(),
                            metadata: c.metadata.clone(),
                            connected_since: wire::rfc3339(c.connected_since),
                            status: c.status,
                            last_seen_ms: Some(c.last_seen_ms),
                            host_kind: Some(c.host_kind),
                        })
                    })
                    .collect();
                servers.sort_by(|a, b| a.server_id.as_str().cmp(b.server_id.as_str()));
                let result =
                    serde_json::to_value(ServersListResult { servers }).expect("serializes");
                drop(st);
                reply(wire::ok(id, sess, result));
            }
            "session_bind_server" => self.session_bind(cid, id, sess, &params),
            "session_unbind_server" => {
                let Some(sid) = sess else {
                    return reply(wire::invalid_params(id, None, "session_id required"));
                };
                let mut st = self.st();
                if let Some(s) = st.sessions.get_mut(sid).filter(|s| s.owner == cid) {
                    let srv = s.server.take();
                    s.tools.clear();
                    if let Some(c) = srv
                        .as_ref()
                        .and_then(|i| st.servers.get(i))
                        .and_then(|c| st.conns.get(c))
                    {
                        send(&c.tx, wire::session_unbind(sid));
                    }
                }
                reply(wire::ok(id, sess, json!({})));
            }
            "tools.list" => {
                let st = self.st();
                match sess
                    .and_then(|s| st.sessions.get(s))
                    .filter(|s| s.owner == cid)
                {
                    Some(s) => reply(wire::ok(
                        id,
                        sess,
                        json!({"tools": s.tools, "workspace_bound": s.server.is_some()}),
                    )),
                    None => reply(wire::err(id, sess, -32006, "session not found", None)),
                }
            }
            "tool.call" => self.tool_call(cid, id, sess, params),
            other => reply(wire::method_not_found(id, sess, other)),
        }
    }

    pub(crate) fn session_bind(
        self: &Arc<Self>,
        cid: ConnId,
        id: &Value,
        sess: Option<&str>,
        params: &Value,
    ) {
        let Some(sid) = sess else {
            return self.reply(cid, wire::invalid_params(id, None, "session_id required"));
        };
        let Some(server_id) = params.get("server_id").and_then(Value::as_str) else {
            return self.reply(cid, wire::invalid_params(id, sess, "server_id required"));
        };
        let mut st = self.st();
        let Some(user) = st.conns.get(&cid).map(|c| c.user.clone()) else {
            return;
        };
        if st.sessions.get(sid).is_none_or(|s| s.owner != cid) {
            drop(st);
            return self.reply(
                cid,
                wire::err(
                    id,
                    sess,
                    -32006,
                    "session not open on this connection",
                    None,
                ),
            );
        }
        // A server of another user is reported exactly like a missing one.
        let target = st
            .servers
            .get(server_id)
            .copied()
            .filter(|c| st.conns.get(c).is_some_and(|c| c.user == user));
        let Some(server_conn) = target else {
            drop(st);
            return self.reply(
                cid,
                wire::workspace_unavailable(
                    id,
                    sess,
                    WorkspaceGoneReason::NotBound,
                    WorkspaceGonePhase::RouteMissing,
                ),
            );
        };
        if st.pending.len() >= MAX_PENDING {
            drop(st);
            return self.reply(cid, wire::err(id, sess, -32016, "hub busy", None));
        }
        let hub_id = format!(
            "{}{}",
            wire::HUB_ID_PREFIX,
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let mut bind_params = json!({"session_id": sid});
        for k in ["cwd", "metadata"] {
            if let Some(val) = params.get(k) {
                bind_params[k] = val.clone();
            }
        }
        st.pending.insert(
            hub_id.clone(),
            Pending::Bind {
                harness: cid,
                harness_id: id.clone(),
                session: sid.to_owned(),
                server_id: server_id.to_owned(),
                server_conn,
            },
        );
        if let Some(c) = st.conns.get(&server_conn) {
            send(&c.tx, wire::session_bind(&hub_id, bind_params));
        }
        drop(st);
        let hub = Arc::clone(self);
        tokio::spawn(async move {
            tokio::time::sleep(BIND_TIMEOUT).await;
            let timed_out = hub.st().pending.remove(&hub_id);
            if let Some(Pending::Bind {
                harness,
                harness_id,
                session,
                ..
            }) = timed_out
            {
                hub.reply(
                    harness,
                    wire::workspace_unavailable(
                        &harness_id,
                        Some(&session),
                        WorkspaceGoneReason::Disconnect,
                        WorkspaceGonePhase::RouteMissing,
                    ),
                );
            }
        });
    }

    pub(crate) fn tool_call(&self, cid: ConnId, id: &Value, sess: Option<&str>, params: Value) {
        let Some(sid) = sess else {
            return self.reply(cid, wire::invalid_params(id, None, "session_id required"));
        };
        let mut st = self.st();
        let route = st
            .sessions
            .get(sid)
            .filter(|s| s.owner == cid)
            .map(|s| s.server.as_ref().and_then(|i| st.servers.get(i)).copied());
        let reply_to = |st: &State, frame: String| {
            if let Some(c) = st.conns.get(&cid) {
                send(&c.tx, frame)
            }
        };
        let server_conn = match route {
            None => {
                return reply_to(
                    &st,
                    wire::err(
                        id,
                        sess,
                        -32006,
                        "session not open on this connection",
                        None,
                    ),
                );
            }
            Some(None) => {
                return reply_to(
                    &st,
                    wire::workspace_unavailable(
                        id,
                        sess,
                        WorkspaceGoneReason::NotBound,
                        WorkspaceGonePhase::RouteMissing,
                    ),
                );
            }
            Some(Some(c)) => c,
        };
        if st.pending.len() >= MAX_PENDING {
            return reply_to(&st, wire::err(id, sess, -32016, "hub busy", None));
        }
        let hub_id = format!(
            "{}{}",
            wire::HUB_ID_PREFIX,
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        st.pending.insert(
            hub_id.clone(),
            Pending::Call {
                harness: cid,
                harness_id: id.clone(),
                session: sid.to_owned(),
                server_conn,
            },
        );
        if let Some(c) = st.conns.get(&server_conn) {
            send(&c.tx, json!({"jsonrpc":"2.0","id":hub_id,"session_id":sid,"method":"tool_call_request","params":params}).to_string());
        }
    }

    pub(crate) fn harness_notification(&self, cid: ConnId, method: &str, v: &Value) {
        if !wire::HARNESS_TO_SERVER_NOTIFS.contains(&method) {
            return tracing::debug!(cid, method, "dropping unsupported harness notification");
        }
        let Some(sid) = v.get("session_id").and_then(Value::as_str) else {
            return;
        };
        let st = self.st();
        let target = st
            .sessions
            .get(sid)
            .filter(|s| s.owner == cid)
            .and_then(|s| s.server.as_ref())
            .and_then(|i| st.servers.get(i))
            .and_then(|c| st.conns.get(c));
        if let Some(c) = target {
            send(&c.tx, v.to_string());
        }
    }

    pub(crate) fn server_request(&self, cid: ConnId, method: &str, id: &Value, v: &Value) {
        let sess = v.get("session_id").and_then(Value::as_str);
        match method {
            "serve" => {
                let n = v
                    .pointer("/params/tools")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                self.reply(cid, wire::ok(id, sess, json!({"accepted": n})));
            }
            other => self.reply(cid, wire::method_not_found(id, sess, other)),
        }
    }

    pub(crate) fn server_notification(&self, cid: ConnId, method: &str, v: &Value) {
        if wire::DONATE_NOTIFS.contains(&method) {
            return;
        }
        let mut st = self.st();
        if method == "tool_server.status" {
            if let Some(status) = v
                .pointer("/params/status")
                .and_then(|s| serde_json::from_value(s.clone()).ok())
                && let Some(c) = st.conns.get_mut(&cid)
                && status != ToolServerLifecycleStatus::Disconnected
            {
                c.status = status;
            }
            return;
        }
        if !wire::SERVER_TO_HARNESS_NOTIFS.contains(&method) {
            return tracing::debug!(cid, method, "dropping unsupported server notification");
        }
        let Some(sid) = v.get("session_id").and_then(Value::as_str) else {
            return;
        };
        let server_id = st.conns.get(&cid).and_then(|c| c.server_id.clone());
        let owner = st
            .sessions
            .get(sid)
            .filter(|s| s.server.is_some() && s.server == server_id)
            .map(|s| s.owner);
        if let Some(c) = owner.and_then(|o| st.conns.get(&o)) {
            send(&c.tx, v.to_string());
        }
    }

    pub(crate) fn server_response(&self, cid: ConnId, v: &Value) {
        let Some(key) = v.get("id").and_then(Value::as_str) else {
            return;
        };
        let mut st = self.st();
        // Only the server the request went to may answer it.
        let owned = matches!(st.pending.get(key),
            Some(Pending::Call { server_conn, .. } | Pending::Bind { server_conn, .. }) if *server_conn == cid);
        if !owned {
            return tracing::debug!(cid, key, "response for unknown or foreign request");
        }
        let Some(p) = st.pending.remove(key) else {
            return;
        };
        let (harness, harness_id, session) = match &p {
            Pending::Call {
                harness,
                harness_id,
                session,
                ..
            }
            | Pending::Bind {
                harness,
                harness_id,
                session,
                ..
            } => (*harness, harness_id.clone(), session.clone()),
        };
        if let Pending::Bind { server_id, .. } = &p
            && let Some(result) = v.get("result")
            && let Some(s) = st.sessions.get_mut(&session)
        {
            s.server = Some(server_id.clone());
            s.tools = result
                .get("tools")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
        }
        let mut out = v.clone();
        out["id"] = harness_id;
        if let Some(c) = st.conns.get(&harness) {
            send(&c.tx, out.to_string());
        }
    }
}
