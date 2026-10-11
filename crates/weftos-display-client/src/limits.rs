//! Session bounds (ADR-117 §2): a node cap, an op rate and an image
//! budget. The session checks every `apply` against these *before*
//! anything is sent, and rejects the whole batch atomically, so a leaf
//! never sees a half-applied envelope.

use std::collections::BTreeMap;
use std::collections::VecDeque;

use weftos_leaf_scene::{DisplayId, NodeId, Primitive, Scene, SceneOp};

/// What one session may hold and send. The defaults are the D1
/// numbers; the real leaf will advertise its own and the smaller of
/// the two wins.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionLimits {
    /// Most nodes alive at once. ADR-117 fixes this at 4,096.
    pub max_nodes: usize,
    /// Most ops accepted in any one-second window. `Batch` children
    /// count individually.
    pub max_ops_per_sec: u32,
    /// Most bitmap bytes alive at once across every `Bitmap` node.
    pub image_budget_bytes: usize,
}

impl SessionLimits {
    /// ADR-117 §2 node cap.
    pub const MAX_NODES: usize = 4_096;

    /// D1 defaults: 4,096 nodes, 1,000 ops/s, 2 MiB of images (about
    /// two full 800×480 RGB565 frames).
    pub const fn d1() -> Self {
        Self {
            max_nodes: Self::MAX_NODES,
            max_ops_per_sec: 1_000,
            image_budget_bytes: 2 * 1024 * 1024,
        }
    }
}

impl Default for SessionLimits {
    fn default() -> Self {
        Self::d1()
    }
}

/// Sliding one-second window over op timestamps.
#[derive(Debug, Default)]
pub struct OpRateLimiter {
    stamps: VecDeque<u64>,
}

impl OpRateLimiter {
    /// Admit `count` ops at `now_ms`, or say how many would be in the
    /// window if they were. Admitted ops are recorded.
    pub fn admit(&mut self, now_ms: u64, count: u32, max_per_sec: u32) -> Result<(), u32> {
        // Anything a full second old or older has left the window.
        while matches!(self.stamps.front(), Some(&t) if now_ms.saturating_sub(t) >= 1_000) {
            self.stamps.pop_front();
        }
        let would_be = self.stamps.len() as u32 + count;
        if would_be > max_per_sec {
            return Err(would_be);
        }
        for _ in 0..count {
            self.stamps.push_back(now_ms);
        }
        Ok(())
    }

    /// Ops currently inside the window (after the last `admit`).
    pub fn in_window(&self) -> usize {
        self.stamps.len()
    }
}

/// Client-side mirror of what the leaf holds for this session: which
/// nodes are alive and how many bitmap bytes each carries. Cheap
/// enough to keep per session; it is what makes the bounds checkable
/// before an envelope leaves the app.
#[derive(Debug, Default, Clone)]
pub struct SceneLedger {
    /// Live node → bitmap bytes it holds (0 for non-bitmap nodes).
    nodes: BTreeMap<NodeId, usize>,
    image_bytes: usize,
}

impl SceneLedger {
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn image_bytes(&self) -> usize {
        self.image_bytes
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.nodes.contains_key(&id)
    }

    /// Fold `ops` into a copy and return it, so the caller can compare
    /// against the limits and only commit when they pass.
    pub fn projected(&self, display: DisplayId, ops: &[SceneOp]) -> SceneLedger {
        let mut next = self.clone();
        for op in ops {
            next.fold(display, op);
        }
        next
    }

    fn fold(&mut self, display: DisplayId, op: &SceneOp) {
        match op {
            SceneOp::Insert(node) | SceneOp::Update(node) => {
                let id = NodeId::from_parts(display, node.id.path_hash());
                self.put(id, bitmap_bytes(&node.primitive));
            }
            SceneOp::Remove(id) => self.take(*id),
            SceneOp::Clear => self.reset(),
            SceneOp::Replace(scene) => self.replace(scene),
            SceneOp::Batch(inner) => {
                for op in inner {
                    self.fold(display, op);
                }
            }
            SceneOp::SetLayerBlend { .. } | SceneOp::Tween { .. } | SceneOp::CancelTween { .. } => {}
        }
    }

    fn put(&mut self, id: NodeId, bytes: usize) {
        if let Some(old) = self.nodes.insert(id, bytes) {
            self.image_bytes -= old;
        }
        self.image_bytes += bytes;
    }

    fn take(&mut self, id: NodeId) {
        if let Some(old) = self.nodes.remove(&id) {
            self.image_bytes -= old;
        }
    }

    fn reset(&mut self) {
        self.nodes.clear();
        self.image_bytes = 0;
    }

    fn replace(&mut self, scene: &Scene) {
        self.reset();
        for node in &scene.nodes {
            let id = NodeId::from_parts(scene.display_id, node.id.path_hash());
            self.put(id, bitmap_bytes(&node.primitive));
        }
    }
}

/// Bytes a primitive keeps resident on the leaf. Only bitmaps count.
fn bitmap_bytes(p: &Primitive) -> usize {
    match p {
        Primitive::Bitmap { data, .. } => data.len(),
        _ => 0,
    }
}

/// Number of ops `ops` represents for rate purposes: `Batch` children
/// count individually, the batch wrapper itself does not.
pub fn count_ops(ops: &[SceneOp]) -> u32 {
    ops.iter()
        .map(|op| match op {
            SceneOp::Batch(inner) => count_ops(inner),
            _ => 1,
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rate_window_slides() {
        let mut r = OpRateLimiter::default();
        assert!(r.admit(0, 5, 5).is_ok());
        assert_eq!(r.admit(500, 1, 5), Err(6));
        // One second later the first five have aged out.
        assert!(r.admit(1_000, 5, 5).is_ok());
        assert_eq!(r.in_window(), 5);
    }

    #[test]
    fn batch_children_count_individually() {
        let ops = vec![SceneOp::Clear, SceneOp::Batch(vec![SceneOp::Clear, SceneOp::Clear])];
        assert_eq!(count_ops(&ops), 3);
    }
}
