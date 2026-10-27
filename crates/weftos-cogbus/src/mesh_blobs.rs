//! Blob handles that arrived in remote readings (ADR-122 section 14, B6e):
//! the subscriber node remembers which node holds each blob and which local
//! readers were handed it. A local `blob_get` of such a hash starts one fetch
//! from that node into the local store (owner `@mesh:<node>`, readable by
//! those readers), answers `unavailable` until it lands, and is served
//! locally from then on. Blobs travel only when fetched.

use std::collections::{HashMap, HashSet};

use clawft_types::cog_topics::find_handles;
use serde_json::Value;

use super::{Broker, State, bad};
use crate::proto::ErrorCode;
use crate::types::{BusError, Session, refuse};

/// Remote handles remembered at once (oldest-expiring dropped first).
const MAX_REMOTE_BLOBS: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlobState {
    /// Known, not asked for.
    Idle,
    /// A reader asked; the hub fetches at its next pump.
    Wanted,
    /// The hub is fetching it.
    Fetching,
    /// The fetch failed; reads say why.
    Failed(String),
}

/// A blob some node handed this one in a reading.
#[derive(Debug, Clone)]
pub(crate) struct RemoteBlob {
    pub(crate) node: String,
    pub(crate) project: Option<String>,
    pub(crate) media_type: String,
    pub(crate) size: u64,
    pub(crate) expires_ms: u64,
    pub(crate) readers: HashSet<(Option<String>, String)>,
    pub(crate) state: BlobState,
}

/// Remote blobs by hash.
#[derive(Default)]
pub(super) struct RemoteBlobs(HashMap<String, RemoteBlob>);

impl Broker {
    /// A reading from `node` on a proxy topic of `project` reached `readers`:
    /// remember the handles it carries.
    pub(super) fn mesh_note_handles(st: &mut State, node: &str, project: &Option<String>, body: &Value, readers: &[(Option<String>, String)]) {
        let handles = find_handles(body);
        if handles.is_empty() || readers.is_empty() {
            return;
        }
        let now = crate::audit::now_ms();
        let map = &mut st.mesh.blobs.0;
        map.retain(|_, b| b.expires_ms > now);
        for h in handles {
            if map.len() >= MAX_REMOTE_BLOBS && !map.contains_key(&h.hash) {
                let Some(oldest) = map.iter().min_by_key(|(_, b)| b.expires_ms).map(|(k, _)| k.clone()) else { break };
                map.remove(&oldest);
            }
            let e = map.entry(h.hash.clone()).or_insert_with(|| RemoteBlob {
                node: node.to_string(),
                project: project.clone(),
                media_type: h.media_type.clone(),
                size: h.size,
                expires_ms: h.expires_ms,
                readers: HashSet::new(),
                state: BlobState::Idle,
            });
            if e.node == node {
                e.readers.extend(readers.iter().cloned());
                e.expires_ms = e.expires_ms.max(h.expires_ms);
            }
        }
    }

    /// `s` read `hash` and the local store has no such blob: if a remote
    /// reading handed it to `s`, the refusal to answer instead (the fetch is
    /// started, or why it failed).
    pub(super) fn mesh_blob_miss(&self, s: &Session, hash: &str) -> Option<BusError> {
        let me = (s.project_id.clone(), s.instance_id.clone());
        let topic = format!("blob/{hash}");
        let mut g = self.lock();
        let b = g.mesh.blobs.0.get_mut(hash).filter(|b| b.readers.contains(&me))?;
        let e = match &b.state {
            BlobState::Failed(why) => bad(ErrorCode::NotFound, format!("blob {hash} could not be fetched from {}: {why}", b.node)),
            _ => {
                if b.state == BlobState::Idle {
                    b.state = BlobState::Wanted;
                }
                bad(ErrorCode::Unavailable, format!("blob {hash} is being fetched from {}; retry", b.node))
            }
        };
        let wake = g.mesh.wake.clone();
        drop(g);
        wake.notify_one();
        Some(refuse(&s.instance_id, "blob.get", &topic, e))
    }

    /// Wanted blobs, now marked as being fetched: `(hash, node)`.
    pub(crate) fn mesh_blobs_due(&self) -> Vec<(String, String)> {
        let mut g = self.lock();
        let mut out = Vec::new();
        for (hash, b) in g.mesh.blobs.0.iter_mut().filter(|(_, b)| b.state == BlobState::Wanted) {
            b.state = BlobState::Fetching;
            out.push((hash.clone(), b.node.clone()));
        }
        out
    }

    /// What this node knows of remote blob `hash`.
    pub(crate) fn mesh_blob(&self, hash: &str) -> Option<RemoteBlob> {
        self.lock().mesh.blobs.0.get(hash).cloned()
    }

    /// The fetch of `hash` ended: landed (forget it, the store has it) or
    /// failed (reads say why).
    pub(crate) fn mesh_blob_done(&self, hash: &str, result: Result<(), String>) {
        let mut g = self.lock();
        match result {
            Ok(()) => {
                g.mesh.blobs.0.remove(hash);
            }
            Err(why) => {
                if let Some(b) = g.mesh.blobs.0.get_mut(hash) {
                    b.state = BlobState::Failed(why);
                }
            }
        }
    }
}
