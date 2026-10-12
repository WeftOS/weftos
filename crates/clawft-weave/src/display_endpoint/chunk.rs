//! Splitting a scene batch into frames the pad can take. The pad reads
//! each push into an 8 KiB buffer, so one frame carries at most
//! [`weftos_leaf_scene::session::MAX_PUSH_CBOR`] bytes of CBOR.

use weftos_leaf_scene::codec;
use weftos_leaf_scene::SceneOp;

/// Per-frame overhead of the `Ops` wrapper and envelope header, with room.
const FRAME_OVERHEAD: usize = 64;

/// Group `ops` (batches flattened) into chunks that each encode within
/// `max_cbor`. A single op that cannot fit alone is an error naming its
/// size, never silently dropped.
pub fn chunk_ops(ops: &[SceneOp], max_cbor: usize) -> Result<Vec<Vec<SceneOp>>, String> {
    let mut flat = Vec::with_capacity(ops.len());
    flatten(ops, &mut flat);
    let budget = max_cbor.saturating_sub(FRAME_OVERHEAD);
    let mut out: Vec<Vec<SceneOp>> = Vec::new();
    let (mut cur, mut used) = (Vec::new(), 0usize);
    for op in flat {
        let size = codec::encode(&op).map_err(|e| format!("cannot encode op: {e}"))?.len();
        if size > budget {
            return Err(format!("one scene op encodes to {size} bytes; the leaf channel takes {budget} per frame"));
        }
        if used + size > budget && !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
            used = 0;
        }
        cur.push(op);
        used += size;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    Ok(out)
}

fn flatten(ops: &[SceneOp], out: &mut Vec<SceneOp>) {
    for op in ops {
        match op {
            SceneOp::Batch(inner) => flatten(inner, out),
            other => out.push(other.clone()),
        }
    }
}
