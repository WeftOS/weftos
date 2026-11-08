//! Routing core: connection registry, session ownership, and the relay of
//! requests and notifications between harnesses and tool servers.
//!
//! All state sits behind one `std::sync::Mutex` that is never held across an
//! `.await`; outbound frames go through per-connection unbounded channels.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::mpsc::UnboundedSender;
use tokio_tungstenite::tungstenite::Message;
use xai_tool_protocol::{
    ConnectionKind, HelloAckMsg, HelloMsg, HostKind, PROTOCOL_VERSION, ToolServerLifecycleStatus,
    ToolServerStatusPayload, WorkspaceGonePhase, WorkspaceGoneReason,
};

use crate::config::HubConfig;
use crate::passkey::{self, Claims, Role};
use crate::wire;

/// Longest a `session.bind` may wait for the tool server's answer.
pub const BIND_TIMEOUT: Duration = Duration::from_secs(10);
/// Most relayed requests in flight across the hub.
pub const MAX_PENDING: usize = 4096;
/// Largest `metadata` blob a server may register.
pub const MAX_METADATA_BYTES: usize = 64 * 1024;

pub type Tx = UnboundedSender<Message>;
pub type ConnId = u64;

/// Why a connection was refused. `status` is the HTTP status for an upgrade
/// refusal and the reason text for a WS close after it.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: u16,
    pub reason: String,
}
impl Refusal {
    fn new(status: u16, reason: impl Into<String>) -> Self {
        Self {
            status,
            reason: reason.into(),
        }
    }
}

/// A verified credential for one connection.
#[derive(Debug, Clone)]
pub struct Authed {
    pub claims: Claims,
    pub minter_id: String,
    pub host_kind: HostKind,
    pub role: Role,
}

pub(crate) struct Conn {
    pub(crate) tx: Tx,
    pub(crate) kind: ConnectionKind,
    pub(crate) user: String,
    pub(crate) server_id: Option<String>,
    pub(crate) description: String,
    pub(crate) metadata: Value,
    pub(crate) host_kind: HostKind,
    pub(crate) connected_since: u64,
    pub(crate) last_seen_ms: u64,
    pub(crate) status: ToolServerLifecycleStatus,
}

pub(crate) struct Session {
    pub(crate) owner: ConnId,
    pub(crate) user: String,
    pub(crate) server: Option<String>,
    pub(crate) tools: Vec<Value>,
}

pub(crate) enum Pending {
    Call {
        harness: ConnId,
        harness_id: Value,
        session: String,
        server_conn: ConnId,
    },
    Bind {
        harness: ConnId,
        harness_id: Value,
        session: String,
        server_id: String,
        server_conn: ConnId,
    },
}

#[derive(Default)]
pub(crate) struct State {
    pub(crate) conns: HashMap<ConnId, Conn>,
    pub(crate) servers: HashMap<String, ConnId>,
    pub(crate) sessions: HashMap<String, Session>,
    pub(crate) pending: HashMap<String, Pending>,
}

pub struct Hub {
    cfg: HubConfig,
    state: Mutex<State>,
    pub(crate) next: AtomicU64,
}

pub fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}
pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

pub(crate) fn send(tx: &Tx, frame: String) {
    // A closed channel means the peer is already gone; cleanup handles it.
    let _ = tx.send(Message::Text(frame.into()));
}

impl Hub {
    pub fn new(cfg: HubConfig) -> Arc<Self> {
        Arc::new(Self {
            cfg,
            state: Mutex::new(State::default()),
            next: AtomicU64::new(1),
        })
    }

    pub fn config(&self) -> &HubConfig {
        &self.cfg
    }

    pub(crate) fn st(&self) -> MutexGuard<'_, State> {
        // A poisoned lock means a routing bug panicked mid-update; continuing
        // on half-updated state would mis-route, so propagate.
        self.state.lock().expect("hub state poisoned")
    }

    /// Number of live connections (tests, status).
    pub fn connection_count(&self) -> usize {
        self.st().conns.len()
    }

    /// Verify the bearer for `role`. Every refusal carries an HTTP status.
    pub fn authenticate(
        &self,
        bearer: Option<&str>,
        role: Role,
        now: u64,
    ) -> Result<Authed, Refusal> {
        let token = bearer.ok_or_else(|| Refusal::new(401, "missing bearer pass key"))?;
        let (claims, minter) = passkey::verify(
            token,
            &self.cfg.minters,
            &self.cfg.revoked_jti,
            self.cfg.max_ttl_secs,
            now,
        )
        .map_err(|e| Refusal::new(401, e.to_string()))?;
        if !claims.roles.contains(&role) {
            return Err(Refusal::new(403, "pass key does not grant this role"));
        }
        Ok(Authed {
            host_kind: minter.host_kind,
            minter_id: minter.id.clone(),
            claims,
            role,
        })
    }

    /// Validate the hello frame, register the connection, and return the
    /// connection id plus the `hello_ack` text to send.
    pub fn register(
        &self,
        auth: &Authed,
        hello: &HelloMsg,
        tx: Tx,
    ) -> Result<(ConnId, String), Refusal> {
        if hello.protocol_version != PROTOCOL_VERSION {
            return Err(Refusal::new(
                400,
                format!("unsupported protocol version {}", hello.protocol_version),
            ));
        }
        let want = match auth.role {
            Role::Server => ConnectionKind::ToolServer,
            Role::Harness => ConnectionKind::Harness,
        };
        if hello.kind != want {
            return Err(Refusal::new(
                400,
                format!("hello kind {} does not match role", hello.kind),
            ));
        }
        let user = auth.claims.sub.clone();
        let mut server_id = None;
        let mut evicted: Option<Tx> = None;
        let mut st = self.st();
        if auth.role == Role::Server {
            let id = hello
                .server_id
                .as_ref()
                .ok_or_else(|| Refusal::new(400, "tool_server hello requires server_id"))?
                .as_str()
                .to_owned();
            let machine =
                self.cfg.machines.get(&id).ok_or_else(|| {
                    Refusal::new(403, format!("machine {id:?} is not configured"))
                })?;
            if machine.minter != auth.minter_id {
                return Err(Refusal::new(
                    403,
                    "pass key minter is not allowed to serve this machine",
                ));
            }
            if let Some(srv) = &auth.claims.srv
                && srv != &id
            {
                return Err(Refusal::new(
                    403,
                    "pass key is bound to a different machine",
                ));
            }
            if let Some(old) = st.servers.get(&id).copied() {
                let same_user = st.conns.get(&old).is_some_and(|c| c.user == user);
                if !same_user {
                    return Err(Refusal::new(
                        409,
                        "server_id already registered by another user",
                    ));
                }
                evicted = st.conns.get(&old).map(|c| c.tx.clone());
                Self::drop_conn(&mut st, old);
            }
            let meta = hello.metadata.clone().unwrap_or(Value::Null);
            if meta.to_string().len() > MAX_METADATA_BYTES {
                return Err(Refusal::new(400, "metadata too large"));
            }
            server_id = Some(id);
        }
        let cid = self.next.fetch_add(1, Ordering::Relaxed);
        let conn = Conn {
            tx,
            kind: hello.kind,
            user: user.clone(),
            description: hello.description.clone().unwrap_or_else(|| {
                server_id
                    .as_ref()
                    .and_then(|i| self.cfg.machines.get(i))
                    .map(|m| m.description.clone())
                    .unwrap_or_default()
            }),
            metadata: hello.metadata.clone().unwrap_or(Value::Null),
            server_id: server_id.clone(),
            host_kind: auth.host_kind,
            connected_since: now_secs(),
            last_seen_ms: now_ms(),
            status: ToolServerLifecycleStatus::Ready,
        };
        st.conns.insert(cid, conn);
        if let Some(id) = server_id {
            st.servers.insert(id, cid);
        }
        drop(st);
        if let Some(tx) = evicted {
            let _ = tx.send(Message::Close(None));
        }
        let ack = HelloAckMsg {
            connection_id: xai_tool_protocol::ConnectionId::new(format!("c{cid}"))
                .expect("non-empty"),
            user_id: xai_tool_protocol::UserId::new(user)
                .map_err(|e| Refusal::new(400, e.to_string()))?,
            computer_hub_version: wire::HUB_VERSION.to_owned(),
            supported_protocol_versions: vec![PROTOCOL_VERSION.to_owned()],
            capabilities: Vec::new(),
        };
        Ok((cid, serde_json::to_string(&ack).expect("ack serializes")))
    }

    /// Remove a connection and everything hanging off it.
    pub fn disconnect(&self, cid: ConnId) {
        let mut st = self.st();
        Self::drop_conn(&mut st, cid);
    }

    fn drop_conn(st: &mut State, cid: ConnId) {
        let Some(conn) = st.conns.remove(&cid) else {
            return;
        };
        match conn.kind {
            ConnectionKind::ToolServer => {
                if let Some(id) = &conn.server_id
                    && st.servers.get(id) == Some(&cid)
                {
                    st.servers.remove(id);
                }
                // Fail in-flight work routed to this server; tell owners.
                let dead: Vec<String> = st
                    .pending
                    .iter()
                    .filter(|(_, p)| matches!(p, Pending::Call { server_conn, .. } | Pending::Bind { server_conn, .. } if *server_conn == cid))
                    .map(|(k, _)| k.clone())
                    .collect();
                for key in dead {
                    match st.pending.remove(&key) {
                        Some(Pending::Call {
                            harness,
                            harness_id,
                            session,
                            ..
                        })
                        | Some(Pending::Bind {
                            harness,
                            harness_id,
                            session,
                            ..
                        }) => {
                            if let Some(h) = st.conns.get(&harness) {
                                send(
                                    &h.tx,
                                    wire::workspace_unavailable(
                                        &harness_id,
                                        Some(&session),
                                        WorkspaceGoneReason::Disconnect,
                                        WorkspaceGonePhase::InFlightCancelled,
                                    ),
                                );
                            }
                        }
                        None => {}
                    }
                }
                for (sid, s) in st.sessions.iter_mut() {
                    if s.server.as_deref() == conn.server_id.as_deref() && s.server.is_some() {
                        s.server = None;
                        s.tools.clear();
                        if let Some(h) = st.conns.get(&s.owner) {
                            let payload = ToolServerStatusPayload {
                                status: ToolServerLifecycleStatus::Disconnected,
                                session_id: xai_tool_protocol::SessionId::new(sid.clone()).ok(),
                                connection_id: Some(format!("c{cid}")),
                                ..Default::default()
                            };
                            send(&h.tx, json!({"jsonrpc":"2.0","session_id":sid,"method":"tool_server.status","params":payload}).to_string());
                        }
                    }
                }
            }
            _ => {
                let owned: Vec<String> = st
                    .sessions
                    .iter()
                    .filter(|(_, s)| s.owner == cid)
                    .map(|(k, _)| k.clone())
                    .collect();
                for sid in owned {
                    Self::release_session(st, &sid);
                }
                st.pending.retain(|_, p| !matches!(p, Pending::Call { harness, .. } | Pending::Bind { harness, .. } if *harness == cid));
            }
        }
    }

    /// Drop a session and tell its bound server to stop serving it.
    pub(crate) fn release_session(st: &mut State, sid: &str) {
        let Some(s) = st.sessions.remove(sid) else {
            return;
        };
        if let Some(srv) = s
            .server
            .as_ref()
            .and_then(|id| st.servers.get(id))
            .and_then(|c| st.conns.get(c))
        {
            send(&srv.tx, wire::session_unbind(sid));
        }
        st.pending.retain(|_, p| !matches!(p, Pending::Call { session, .. } | Pending::Bind { session, .. } if session == sid));
    }

    /// Handle one inbound text frame from connection `cid`.
    pub fn on_text(self: &Arc<Self>, cid: ConnId, text: &str) {
        let Ok(v) = serde_json::from_str::<Value>(text) else {
            tracing::debug!(cid, "discarding unparseable frame");
            return;
        };
        let kind = {
            let mut st = self.st();
            let Some(c) = st.conns.get_mut(&cid) else {
                return;
            };
            c.last_seen_ms = now_ms();
            c.kind
        };
        let method = v.get("method").and_then(Value::as_str).map(str::to_owned);
        match (method.as_deref(), v.get("id").filter(|i| !i.is_null())) {
            (Some("ping"), _) => {
                let ts = v
                    .get("ts_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(now_ms);
                self.reply(cid, json!({"method":"pong","ts_ms":ts}).to_string());
            }
            (Some("pong"), _) => {}
            (Some(m), Some(id)) => match kind {
                ConnectionKind::Harness => self.harness_request(cid, m, id, &v),
                ConnectionKind::ToolServer => self.server_request(cid, m, id, &v),
                ConnectionKind::BotClient => {}
            },
            (Some(m), None) => match kind {
                ConnectionKind::Harness => self.harness_notification(cid, m, &v),
                ConnectionKind::ToolServer => self.server_notification(cid, m, &v),
                ConnectionKind::BotClient => {}
            },
            (None, _) if kind == ConnectionKind::ToolServer => self.server_response(cid, &v),
            _ => tracing::debug!(cid, "discarding frame with no method"),
        }
    }

    pub(crate) fn reply(&self, cid: ConnId, frame: String) {
        if let Some(c) = self.st().conns.get(&cid) {
            send(&c.tx, frame);
        }
    }
}
