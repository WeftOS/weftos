//! The leaf side of a display session: the pad firmware and the host
//! emulator both run this state machine, so the wire behaviour is
//! tested once. It owns no scene and no socket. Frames go in, and
//! [`Action`]s come out for the caller to render or send.

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use crate::envelope::{InputEnvelope, SceneEnvelope};
use crate::id::{DisplayId, NodeId};
use crate::op::SceneOp;
use crate::primitive::Primitive;
use super::{LeafInfo, SessionFrame, SessionId};

/// Node ids from here up belong to the firmware (boot screen, raster
/// shim, the system strip). No session may touch them.
pub const SYSTEM_ID_BASE: u32 = 0xF0_0000;

/// A host that stays silent this long loses the screen. The daemon
/// sends an empty `Ops` as a keepalive well inside it.
pub const IDLE_TIMEOUT_MS: u64 = 45_000;

/// What the leaf offers and bounds.
#[derive(Debug, Clone)]
pub struct ServiceConfig {
    pub display: DisplayId,
    pub info: LeafInfo,
    pub max_nodes: u32,
    pub max_ops_per_sec: u32,
    pub image_budget_bytes: u32,
    /// Firmware nodes to remove when a session takes the screen (the
    /// boot text), so the app starts on a clean display.
    pub hide_on_open: Vec<NodeId>,
}

/// What the caller must do after handing the service a frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Send this frame to the host (publish on `.input`).
    Send(SessionFrame),
    /// Apply these ops to the scene store. Render now when `render`
    /// is set; otherwise keep the damage for the closing fragment.
    Apply { envelope: SceneEnvelope, render: bool },
    /// The session is gone and its nodes were removed by the `Apply`
    /// before this: the firmware may redraw its idle screen.
    SessionEnded,
}

struct Holder {
    id: SessionId,
    subject: String,
    last_seq: u32,
    /// Session-owned node -> bitmap bytes it holds.
    owned: BTreeMap<u32, u32>,
    last_heard_ms: u64,
    window_start_ms: u64,
    window_ops: u32,
}

impl Holder {
    fn image_bytes(owned: &BTreeMap<u32, u32>) -> u64 {
        owned.values().map(|&b| u64::from(b)).sum()
    }
}

pub struct DisplayService {
    cfg: ServiceConfig,
    holder: Option<Holder>,
    next_session: u32,
}

impl DisplayService {
    pub fn new(cfg: ServiceConfig) -> Self {
        Self { cfg, holder: None, next_session: 1 }
    }

    /// The session currently holding the screen.
    pub fn active(&self) -> Option<SessionId> {
        self.holder.as_ref().map(|h| h.id)
    }

    pub fn subject(&self) -> Option<&str> {
        self.holder.as_ref().map(|h| h.subject.as_str())
    }

    pub fn owned_nodes(&self) -> usize {
        self.holder.as_ref().map_or(0, |h| h.owned.len())
    }

    /// Handle one host -> leaf frame at leaf time `now_ms`.
    pub fn handle(&mut self, frame: SessionFrame, now_ms: u64) -> Vec<Action> {
        match frame {
            SessionFrame::Open { subject, display, .. } => self.open(subject, display, now_ms),
            SessionFrame::Ops { session, seq, more, envelope } => self.ops(session, seq, more, envelope, now_ms),
            SessionFrame::Close { session } => self.close(session),
            // Leaf -> host frames are not valid in this direction.
            _ => Vec::new(),
        }
    }

    /// Drop a session whose host went quiet. Call it about once a second.
    pub fn tick(&mut self, now_ms: u64) -> Vec<Action> {
        let idle = self.holder.as_ref().is_some_and(|h| now_ms.saturating_sub(h.last_heard_ms) > IDLE_TIMEOUT_MS);
        if idle { self.end("host went quiet") } else { Vec::new() }
    }

    /// The link to the parent dropped for good: end the session at once.
    pub fn link_lost(&mut self) -> Vec<Action> {
        self.end("link to the host dropped")
    }

    /// A hit-tested pointer event from the touch panel, as the session's
    /// `Input` frame. `None` when no session holds the screen, so the
    /// caller falls back to the raw `leaf_input` path.
    pub fn input(&self, envelope: InputEnvelope) -> Option<SessionFrame> {
        let session = self.active()?;
        Some(SessionFrame::Input { session, envelope })
    }

    /// A sensor edge such as `("ptt.down", true)`. `None` without a session.
    pub fn sensor(&self, name: &str, pressed: bool) -> Option<SessionFrame> {
        let session = self.active()?;
        Some(SessionFrame::Sensor { session, name: name.to_string(), pressed: Some(pressed), value: None })
    }

    fn open(&mut self, subject: String, display: DisplayId, now_ms: u64) -> Vec<Action> {
        if display != self.cfg.display {
            return alloc::vec![self.refuse("no such display")];
        }
        if let Some(h) = self.holder.as_mut() {
            if h.subject == subject {
                // Idempotent: a reconnect gets the same session back.
                h.last_heard_ms = now_ms;
                let id = h.id;
                return alloc::vec![Action::Send(self.opened(id))];
            }
            let reason = alloc::format!("display busy: held by {}", h.subject);
            return alloc::vec![self.refuse(&reason)];
        }
        let id = SessionId(self.next_session);
        self.next_session = self.next_session.wrapping_add(1).max(1);
        self.holder = Some(Holder {
            id,
            subject,
            last_seq: 0,
            owned: BTreeMap::new(),
            last_heard_ms: now_ms,
            window_start_ms: now_ms,
            window_ops: 0,
        });
        let mut out = Vec::new();
        if !self.cfg.hide_on_open.is_empty() {
            let ops = self.cfg.hide_on_open.iter().map(|n| SceneOp::Remove(*n)).collect();
            out.push(Action::Apply { envelope: SceneEnvelope::new(self.cfg.display, ops), render: true });
        }
        out.push(Action::Send(self.opened(id)));
        out
    }

    fn opened(&self, session: SessionId) -> SessionFrame {
        SessionFrame::Opened {
            session,
            info: self.cfg.info.clone(),
            max_nodes: self.cfg.max_nodes,
            max_ops_per_sec: self.cfg.max_ops_per_sec,
            image_budget_bytes: self.cfg.image_budget_bytes,
        }
    }

    fn refuse(&self, reason: &str) -> Action {
        Action::Send(SessionFrame::Refused { reason: reason.to_string() })
    }

    fn ops(&mut self, session: SessionId, seq: u32, more: bool, env: SceneEnvelope, now_ms: u64) -> Vec<Action> {
        let cfg = &self.cfg;
        let Some(h) = self.holder.as_mut().filter(|h| h.id == session) else {
            return alloc::vec![Action::Send(SessionFrame::Ended { session, reason: "no such session".to_string() })];
        };
        h.last_heard_ms = now_ms;
        if seq != 0 && seq <= h.last_seq {
            return Vec::new(); // journal replay
        }
        h.last_seq = seq.max(h.last_seq);
        if env.ops.is_empty() {
            return Vec::new(); // keepalive
        }
        if now_ms.saturating_sub(h.window_start_ms) >= 1_000 {
            h.window_start_ms = now_ms;
            h.window_ops = 0;
        }
        let count = count_ops(&env.ops);
        if h.window_ops.saturating_add(count) > cfg.max_ops_per_sec {
            return alloc::vec![reject(session, seq, "op rate exceeded")];
        }
        let mut owned = h.owned.clone();
        let mut out = Vec::new();
        if let Err(why) = translate(&env.ops, cfg.display, &mut owned, &mut out) {
            return alloc::vec![reject(session, seq, why)];
        }
        if owned.len() > cfg.max_nodes as usize {
            return alloc::vec![reject(session, seq, "node budget exceeded")];
        }
        if Holder::image_bytes(&owned) > u64::from(cfg.image_budget_bytes) {
            return alloc::vec![reject(session, seq, "image budget exceeded")];
        }
        h.window_ops += count;
        h.owned = owned;
        if out.is_empty() {
            return Vec::new();
        }
        alloc::vec![Action::Apply { envelope: SceneEnvelope::new(cfg.display, out), render: !more }]
    }

    fn close(&mut self, session: SessionId) -> Vec<Action> {
        if self.active() == Some(session) { self.end("closed by the host") } else { Vec::new() }
    }

    /// End the holder's session: remove its nodes, tell the host why.
    fn end(&mut self, reason: &str) -> Vec<Action> {
        let Some(h) = self.holder.take() else { return Vec::new() };
        let mut out = Vec::new();
        if !h.owned.is_empty() {
            let ops = h.owned.keys().map(|k| SceneOp::Remove(NodeId::from_parts(self.cfg.display, *k))).collect();
            out.push(Action::Apply { envelope: SceneEnvelope::new(self.cfg.display, ops), render: true });
        }
        out.push(Action::SessionEnded);
        if reason != "closed by the host" {
            out.push(Action::Send(SessionFrame::Ended { session: h.id, reason: reason.to_string() }));
        }
        out
    }
}

fn reject(session: SessionId, seq: u32, reason: &str) -> Action {
    Action::Send(SessionFrame::Rejected { session, seq, reason: reason.to_string() })
}

fn count_ops(ops: &[SceneOp]) -> u32 {
    ops.iter().map(|op| if let SceneOp::Batch(inner) = op { count_ops(inner) } else { 1 }).sum()
}

fn bitmap_bytes(p: &Primitive) -> u32 {
    match p {
        Primitive::Bitmap { data, .. } => data.len() as u32,
        _ => 0,
    }
}

/// Check `ops` against ownership and rewrite the ones that would reach
/// past the session (`Clear`, `Replace`) into per-node removals.
fn translate(
    ops: &[SceneOp],
    display: DisplayId,
    owned: &mut BTreeMap<u32, u32>,
    out: &mut Vec<SceneOp>,
) -> Result<(), &'static str> {
    for op in ops {
        match op {
            SceneOp::Insert(n) | SceneOp::Update(n) => {
                check_node(n.id, display)?;
                owned.insert(n.id.path_hash(), bitmap_bytes(&n.primitive));
                out.push(op.clone());
            }
            SceneOp::Remove(id) => {
                check_node(*id, display)?;
                owned.remove(&id.path_hash());
                out.push(op.clone());
            }
            SceneOp::Tween { id, .. } | SceneOp::CancelTween { id, .. } => {
                check_node(*id, display)?;
                if !owned.contains_key(&id.path_hash()) {
                    return Err("node not owned by this session");
                }
                out.push(op.clone());
            }
            SceneOp::SetLayerBlend { .. } => return Err("layer blend is not allowed in a session"),
            SceneOp::Clear => {
                remove_all(owned, display, out);
            }
            SceneOp::Replace(scene) => {
                remove_all(owned, display, out);
                for n in &scene.nodes {
                    check_node(n.id, display)?;
                    owned.insert(n.id.path_hash(), bitmap_bytes(&n.primitive));
                    out.push(SceneOp::Insert(n.clone()));
                }
            }
            SceneOp::Batch(inner) => translate(inner, display, owned, out)?,
        }
    }
    Ok(())
}

fn remove_all(owned: &mut BTreeMap<u32, u32>, display: DisplayId, out: &mut Vec<SceneOp>) {
    let ids: BTreeSet<u32> = owned.keys().copied().collect();
    owned.clear();
    out.extend(ids.into_iter().map(|k| SceneOp::Remove(NodeId::from_parts(display, k))));
}

fn check_node(id: NodeId, display: DisplayId) -> Result<(), &'static str> {
    if id.display_id() != display {
        return Err("node is on another display");
    }
    if id.path_hash() >= SYSTEM_ID_BASE {
        return Err("node id is reserved for the firmware");
    }
    Ok(())
}
