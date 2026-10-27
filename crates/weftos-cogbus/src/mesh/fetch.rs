//! Blob fetch by hash across nodes (ADR-122 section 14, B6e).
//!
//! Source side: a peer may fetch a blob only after a reading this node served
//! it carried the handle, and only while that topic's exposure still serves
//! the peer (raw rule included); access follows the exposure, not the project
//! rule. Slices go out against the cog's `bytes_per_sec` and wait in a queue
//! when it is spent. Subscriber side: a local `blob_get` of a remote handle
//! asks for it once; the slices land in the local store as `@mesh:<node>`'s
//! blob, readable by the local readers the reading reached.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use super::source::Budget;
use super::wire::{TopicData, TopicReq};
use super::{MeshAudience, MeshRecord, MeshTopicHub, mesh_principal, proxy_writer};
use crate::audit::{code_name, now_ms};
use crate::blob::{BlobError, CHUNK};
use crate::policy::{BusAction, BusRequest};
use crate::proto::ErrorCode;

/// A blob handed to a peer in a served reading.
#[derive(Debug, Clone)]
pub(crate) struct Handed {
    pub(crate) exposure: String,
    pub(crate) owner: String,
    pub(crate) project: Option<String>,
    pub(crate) raw: bool,
    pub(crate) bytes_per_sec: u64,
    pub(crate) expires_ms: u64,
}

struct Queued {
    peer: String,
    req: u64,
    hash: String,
    offset: u64,
    len: u64,
}

struct Landing {
    hash: String,
    upload: Option<u64>,
    next: u64,
}

/// Both halves' fetch state.
#[derive(Default)]
pub(crate) struct Fetches {
    handed: Mutex<HashMap<(String, String), Handed>>,
    budgets: Mutex<HashMap<(String, String), Budget>>,
    queue: Mutex<VecDeque<Queued>>,
    landing: Mutex<HashMap<(String, u64), Landing>>,
    next: AtomicU64,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}

impl MeshTopicHub {
    /// A served reading carried `handles` to `peer`.
    pub(crate) fn hand(&self, peer: &str, handles: Vec<clawft_types::cog_topics::BlobHandle>, h: Handed) {
        let mut g = lock(&self.fetches.handed);
        let now = now_ms();
        g.retain(|_, x| x.expires_ms > now);
        for b in handles.into_iter().take(32) {
            if g.len() < 16_384 {
                g.insert((peer.to_string(), b.hash), Handed { expires_ms: b.expires_ms, ..h.clone() });
            }
        }
    }

    /// A peer's `fetch`: checked now, served now or queued for its budget.
    pub(crate) fn on_fetch(&self, peer: &str, req: u64, hash: String, offset: u64, len: u64) {
        let refused = |code: ErrorCode, m: String| {
            let d = TopicData::BlobError { req, code: code_name(code), message: m.clone() };
            self.send_data(peer, &d, &[]);
            self.exposures.record(MeshRecord {
                exposure: format!("blob/{hash}"),
                caller_node: peer.to_string(),
                bytes_out: 0,
                duration_ms: 0,
                outcome: format!("refused:{} {m}", code_name(code)),
            });
        };
        let handed = lock(&self.fetches.handed).get(&(peer.to_string(), hash.clone())).cloned();
        let Some(h) = handed.filter(|h| h.expires_ms > now_ms()) else {
            return refused(ErrorCode::NotFound, format!("this node handed you no blob {hash}"));
        };
        match self.exposures.serve(&h.exposure, peer) {
            Err(why) => return refused(ErrorCode::Denied, format!("{} is not served to this node: {why}", h.exposure)),
            Ok(v) if h.raw && !(v.raw && matches!(v.audience, MeshAudience::Nodes(_))) => {
                return refused(ErrorCode::RawNotExposed, format!("{} is raw and no longer exposed raw to this node", h.exposure));
            }
            Ok(_) => {}
        }
        if offset == 0 {
            let principal = mesh_principal(peer);
            let topic = format!("blob/{hash}");
            let req = BusRequest::new(BusAction::MeshFetch, &principal, "-", None).on(&topic, "blob.v1");
            if let Err(e) = self.broker.ext.gate(&req) {
                return refused(e.code, e.message);
            }
            self.broker.ext.allowed(BusAction::MeshFetch, &principal, None, &topic);
        }
        lock(&self.fetches.queue).push_back(Queued { peer: peer.to_string(), req, hash, offset, len });
        self.serve_fetches();
    }

    /// Serve queued slices whose peer has byte budget left.
    pub(crate) fn serve_fetches(&self) {
        let Some(store) = self.broker.blob_store().cloned() else { return };
        let now = Instant::now();
        let mut wait = VecDeque::new();
        loop {
            let Some(q) = lock(&self.fetches.queue).pop_front() else { break };
            let Some(h) = lock(&self.fetches.handed).get(&(q.peer.clone(), q.hash.clone())).cloned() else { continue };
            let want = q.len.clamp(1, CHUNK as u64).min(h.bytes_per_sec.max(1)) as usize;
            let ok = {
                let mut b = lock(&self.fetches.budgets);
                let budget = b.entry((q.peer.clone(), h.exposure.clone())).or_insert_with(|| Budget::new(h.bytes_per_sec as f64, now));
                budget.take(want as f64, now)
            };
            if !ok {
                wait.push_back(q);
                continue;
            }
            let d = match store.get(now_ms(), &h.owner, h.project.as_deref(), &q.hash, q.offset, want) {
                Ok((handle, bytes, eof)) => (TopicData::Blob { req: q.req, size: handle.size, eof }, bytes),
                Err(e) => (TopicData::BlobError { req: q.req, code: code_name(ErrorCode::NotFound), message: e.to_string() }, Vec::new()),
            };
            self.send_data(&q.peer, &d.0, &d.1);
        }
        lock(&self.fetches.queue).extend(wait);
    }

    /// Ask the nodes holding the blobs local readers want for their first slice.
    pub(crate) fn start_fetches(&self) {
        for (hash, node) in self.broker.mesh_blobs_due() {
            let req = self.fetches.next.fetch_add(1, Ordering::Relaxed) + 1;
            lock(&self.fetches.landing).insert((node.clone(), req), Landing { hash: hash.clone(), upload: None, next: 0 });
            self.send_req(&node, &TopicReq::Fetch { req, hash, offset: 0, len: CHUNK as u64 });
        }
    }

    /// One slice (or a refusal) for a fetch of ours.
    pub(crate) fn on_blob(&self, node: &str, req: u64, size: u64, eof: bool, bytes: &[u8]) {
        let key = (node.to_string(), req);
        let Some(mut l) = lock(&self.fetches.landing).remove(&key) else { return };
        match self.land(node, &mut l, size, eof, bytes) {
            Ok(true) => self.broker.mesh_blob_done(&l.hash, Ok(())),
            Ok(false) => {
                let next = TopicReq::Fetch { req, hash: l.hash.clone(), offset: l.next, len: CHUNK as u64 };
                lock(&self.fetches.landing).insert(key, l);
                self.send_req(node, &next);
            }
            Err(why) => self.broker.mesh_blob_done(&l.hash, Err(why)),
        }
    }

    /// The source refused a fetch of ours.
    pub(crate) fn on_blob_error(&self, node: &str, req: u64, code: &str, message: &str) {
        if let Some(l) = lock(&self.fetches.landing).remove(&(node.to_string(), req)) {
            self.broker.mesh_blob_done(&l.hash, Err(format!("{code}: {message}")));
        }
    }

    /// Write one slice into the local store; `Ok(true)` when the blob landed.
    fn land(&self, node: &str, l: &mut Landing, size: u64, eof: bool, bytes: &[u8]) -> Result<bool, String> {
        let store = self.broker.blob_store().cloned().ok_or("this node has no content store")?;
        let meta = self.broker.mesh_blob(&l.hash).ok_or("the handle is no longer known")?;
        if size != meta.size || (bytes.is_empty() && !eof) {
            return Err("the source sent a different blob".into());
        }
        let owner = proxy_writer(node);
        let now = now_ms();
        let upload = match l.upload {
            Some(u) => u,
            None => {
                let ttl = meta.expires_ms.saturating_sub(now).max(1);
                let u = store.begin(now, &owner, meta.project.as_deref(), &meta.media_type, size, Some(ttl)).map_err(|e: BlobError| e.to_string())?;
                l.upload = Some(u);
                u
            }
        };
        let landed = store.chunk(now, &owner, upload, l.next, bytes, eof).map_err(|e| e.to_string())?;
        l.next += bytes.len() as u64;
        let Some(handle) = landed else { return Ok(false) };
        if handle.hash != l.hash {
            return Err("the bytes do not hash to the handle".into());
        }
        for (project, instance) in &meta.readers {
            store.grant(std::slice::from_ref(&l.hash), (meta.project.as_deref(), &owner), (project.as_deref(), instance));
        }
        Ok(true)
    }
}
