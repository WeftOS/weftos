//! Per-leaf session state: the pending open, the live session, its event
//! queue. All of it behind one short-held std mutex; nothing awaits while
//! holding it.

use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tokio::sync::oneshot;
use weftos_leaf_scene::session::{LeafInfo, SessionFrame, SessionId};
use weftos_leaf_scene::codec;

use super::{EndpointError, OpenReply, PollEvent, PollReply};

/// The leaf's `Opened` answer.
pub(crate) struct Opened {
    pub session: SessionId,
    pub info: LeafInfo,
    pub max_nodes: u32,
    pub max_ops_per_sec: u32,
    pub image_budget_bytes: u32,
}

pub(crate) struct Live {
    token: u64,
    session: SessionId,
    subject: String,
    seq: u32,
    events: VecDeque<PollEvent>,
    rejected: Vec<String>,
    ended: Option<String>,
    dropped: u64,
    last_used: Instant,
    reply: OpenReply,
}

/// What the maintenance task needs to know about a live session.
pub(crate) struct LiveBrief {
    pub token: u64,
    pub session: SessionId,
    idle: Duration,
}

impl LiveBrief {
    pub fn idle_for(&self) -> Duration {
        self.idle
    }
}

#[derive(Default)]
struct SlotState {
    attached: bool,
    pending: Option<oneshot::Sender<Result<Opened, String>>>,
    live: Option<Live>,
}

#[derive(Default)]
pub(crate) struct LeafSlot {
    /// Serialises `open` on this leaf.
    pub open_gate: tokio::sync::Mutex<()>,
    /// Keeps one batch's fragments together on the wire.
    pub send_lock: tokio::sync::Mutex<()>,
    st: Mutex<SlotState>,
}

impl LeafSlot {
    fn st(&self) -> std::sync::MutexGuard<'_, SlotState> {
        self.st.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// True the first time only: the caller then does the subscribing.
    pub fn mark_attached(&self) -> bool {
        !std::mem::replace(&mut self.st().attached, true)
    }

    pub fn has_token(&self, token: u64) -> bool {
        self.st().live.as_ref().is_some_and(|l| l.token == token)
    }

    /// The subject of the running session, if there is one.
    pub fn holder(&self) -> Option<String> {
        self.st().live.as_ref().filter(|l| l.ended.is_none()).map(|l| l.subject.clone())
    }

    /// A still-running session this subject already holds.
    pub fn live_for(&self, subject: &str) -> Option<OpenReply> {
        self.st().live.as_ref().filter(|l| l.subject == subject && l.ended.is_none()).map(|l| l.reply.clone())
    }

    pub fn expect_open(&self) -> oneshot::Receiver<Result<Opened, String>> {
        let (tx, rx) = oneshot::channel();
        self.st().pending = Some(tx);
        rx
    }

    pub fn cancel_open(&self) {
        self.st().pending = None;
    }

    pub fn install(&self, token: u64, subject: &str, o: Opened, leaf_id: &str) -> OpenReply {
        let reply = OpenReply {
            session: token,
            leaf_id: leaf_id.to_owned(),
            width: o.info.width,
            height: o.info.height,
            render_caps: o.info.render_caps,
            touch: o.info.touch,
            ptt: o.info.ptt,
            max_nodes: o.max_nodes,
            max_ops_per_sec: o.max_ops_per_sec,
            image_budget_bytes: o.image_budget_bytes,
        };
        self.st().live = Some(Live {
            token,
            session: o.session,
            subject: subject.to_owned(),
            seq: 0,
            events: VecDeque::new(),
            rejected: Vec::new(),
            ended: None,
            dropped: 0,
            last_used: Instant::now(),
            reply: reply.clone(),
        });
        reply
    }

    /// Mark use and return the leaf session and subject. Ended sessions
    /// are still returned, so a poll can report how they ended.
    pub fn touch(&self, token: u64) -> Result<(SessionId, String), EndpointError> {
        let mut st = self.st();
        let live = st.live.as_mut().filter(|l| l.token == token).ok_or(EndpointError::NoSession)?;
        live.last_used = Instant::now();
        Ok((live.session, live.subject.clone()))
    }

    /// Like [`touch`](Self::touch), but an ended session is an error.
    pub fn touch_live(&self, token: u64) -> Result<(SessionId, String), EndpointError> {
        {
            let st = self.st();
            if let Some(why) = st.live.as_ref().filter(|l| l.token == token).and_then(|l| l.ended.clone()) {
                return Err(EndpointError::Ended(why));
            }
        }
        self.touch(token)
    }

    pub fn next_seq(&self, token: u64) -> Result<u32, EndpointError> {
        let mut st = self.st();
        let live = st.live.as_mut().filter(|l| l.token == token).ok_or(EndpointError::NoSession)?;
        live.seq = live.seq.wrapping_add(1).max(1);
        Ok(live.seq)
    }

    pub fn drain(&self, token: u64, max: usize) -> Result<PollReply, EndpointError> {
        let mut st = self.st();
        let live = st.live.as_mut().filter(|l| l.token == token).ok_or(EndpointError::NoSession)?;
        let n = live.events.len().min(max);
        Ok(PollReply {
            events: live.events.drain(..n).collect(),
            rejected: std::mem::take(&mut live.rejected),
            ended: live.ended.clone(),
            dropped: std::mem::take(&mut live.dropped),
        })
    }

    /// Remove the session; returns the leaf session when it was still
    /// running (so the caller tells the leaf to close).
    pub fn end(&self, token: u64, _why: &str) -> Option<SessionId> {
        let mut st = self.st();
        if st.live.as_ref().is_none_or(|l| l.token != token) {
            return None;
        }
        let live = st.live.take()?;
        live.ended.is_none().then_some(live.session)
    }

    pub fn snapshot(&self) -> Vec<LiveBrief> {
        self.st()
            .live
            .iter()
            .filter(|l| l.ended.is_none())
            .map(|l| LiveBrief { token: l.token, session: l.session, idle: l.last_used.elapsed() })
            .collect()
    }

    /// Fold one leaf -> host frame in.
    pub fn on_frame(&self, frame: SessionFrame, max_events: usize) {
        let mut st = self.st();
        match frame {
            SessionFrame::Opened { session, info, max_nodes, max_ops_per_sec, image_budget_bytes } => {
                if let Some(tx) = st.pending.take() {
                    let _ = tx.send(Ok(Opened { session, info, max_nodes, max_ops_per_sec, image_budget_bytes }));
                }
            }
            SessionFrame::Refused { reason } => {
                if let Some(tx) = st.pending.take() {
                    let _ = tx.send(Err(reason));
                }
            }
            SessionFrame::Input { session, envelope } => {
                let Ok(cbor) = codec::encode(&envelope) else { return };
                let ev = PollEvent::Input { cbor_b64: weftos_leaf_scene::session::b64::encode(&cbor) };
                push(&mut st, session, max_events, ev);
            }
            SessionFrame::Sensor { session, name, pressed, value } => {
                push(&mut st, session, max_events, PollEvent::Sensor { name, pressed, value });
            }
            SessionFrame::Rejected { session, seq, reason } => {
                if let Some(l) = st.live.as_mut().filter(|l| l.session == session) {
                    l.rejected.push(format!("batch {seq} refused: {reason}"));
                }
            }
            SessionFrame::Ended { session, reason } => {
                if let Some(l) = st.live.as_mut().filter(|l| l.session == session) {
                    l.ended.get_or_insert(reason);
                }
            }
            // Host -> leaf frames never arrive this way.
            SessionFrame::Open { .. } | SessionFrame::Ops { .. } | SessionFrame::Close { .. } => {}
        }
    }
}

fn push(st: &mut SlotState, session: SessionId, max_events: usize, ev: PollEvent) {
    let Some(l) = st.live.as_mut().filter(|l| l.session == session && l.ended.is_none()) else { return };
    if l.events.len() >= max_events {
        l.events.pop_front();
        l.dropped += 1;
    }
    l.events.push_back(ev);
}
