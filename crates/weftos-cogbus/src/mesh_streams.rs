//! The broker's side of remote streams (ADR-122 section 14, B6d): which mesh
//! subscriptions are streams (the hub keeps a proxy ring writer for each),
//! and the byte credit their chunks use.

use super::Broker;
use crate::mesh::wire::Credit;

/// One open remote stream as the hub needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RemoteStream {
    pub(crate) rsub: u64,
    pub(crate) node: String,
    pub(crate) kind: String,
    pub(crate) source: String,
    pub(crate) schema: String,
    pub(crate) project: Option<String>,
}

impl Broker {
    /// Every remote stream subscription.
    pub(crate) fn mesh_streams(&self) -> Vec<RemoteStream> {
        let g = self.lock();
        g.mesh
            .remote
            .iter()
            .filter_map(|(id, r)| {
                let s = r.stream.as_ref()?;
                Some(RemoteStream { rsub: *id, node: r.node.clone(), kind: r.kind.clone(), source: r.source.clone(), schema: r.schema.clone(), project: s.project.clone() })
            })
            .collect()
    }

    /// One chunk of `len` bytes from `node` on stream subscription `rsub`:
    /// `None` if this node holds no such stream subscription, else the
    /// credit to grant (zero when the window is still more than half full).
    pub(crate) fn mesh_chunk(&self, node: &str, rsub: u64, len: u64) -> Option<Credit> {
        let mut g = self.lock();
        let (window, window_bytes) = (g.mesh.window, g.mesh.window_bytes);
        let r = g.mesh.remote.get_mut(&rsub).filter(|r| r.node == node)?;
        r.phase = super::mesh_ops::Phase::Open;
        r.received += 1;
        r.outstanding = r.outstanding.saturating_sub(1);
        let s = r.stream.as_mut()?;
        s.out_bytes = s.out_bytes.saturating_sub(len);
        if r.outstanding > window / 2 && s.out_bytes > window_bytes / 2 {
            return Some(Credit::default());
        }
        let grant = Credit { n: window - r.outstanding, bytes: window_bytes - s.out_bytes };
        r.outstanding = window;
        s.out_bytes = window_bytes;
        r.renewed = std::time::Instant::now();
        Some(grant)
    }
}
