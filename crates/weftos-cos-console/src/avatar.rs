//! The avatar (nodes 100–199): a phosphor-green field in the visual
//! panel, plus the state caption. The field is the twigl march in
//! [`crate::avatar_shader`], streamed as RGB565 tiles because one mesh
//! frame on the pad is 8192 bytes. `tick(now_ms)` emits at most one
//! tile, so a full picture arrives over a handful of host frames.

use std::collections::BTreeMap;

use weftos_leaf_scene::{px, BitmapFormat, Layer, Node, Primitive, SceneOp, Style, Transform};

use crate::avatar_shader::{self, render, FIELD_W, FIELD_X, FIELD_Y, TILE_H, TILES};
use crate::prim::text;
use crate::status::AgentState;
use crate::text::{center, truncate};
use crate::theme::{nid, AMBER, DIM, SMALL, VISUAL};

pub use crate::avatar_shader::{FIELD_H, FIELD_BYTES};

/// First tile. Tiles occupy `ID_FIELD .. ID_FIELD + TILES`.
pub const ID_FIELD: u32 = 100;
pub const ID_CAPTION: u32 = 169;
pub const ID_REASON: u32 = 170;

/// How long one painted field is held before the next march. The host
/// ticks about every 33 ms and sends one tile each tick, so this waits
/// out all `TILES` strips before the picture is replaced.
const FRAME_MS: u32 = 720;

#[derive(Debug, Clone)]
pub struct Avatar {
    state: AgentState,
    nodes: BTreeMap<u32, Node>,
    field: Vec<u8>,
    last_render_ms: u32,
    tile_next: usize,
    force_render: bool,
    rendered_once: bool,
}

impl Default for Avatar {
    fn default() -> Self {
        Self::new()
    }
}

impl Avatar {
    pub fn new() -> Self {
        let mut a = Self {
            state: AgentState::Idle,
            nodes: BTreeMap::new(),
            field: vec![0u8; avatar_shader::FIELD_BYTES],
            last_render_ms: 0,
            tile_next: 0,
            force_render: true,
            rendered_once: false,
        };
        a.nodes = a.caption_nodes().into_iter().map(|n| (n.id.path_hash(), n)).collect();
        a
    }

    pub fn state(&self) -> &AgentState {
        &self.state
    }

    pub fn node(&self, id: u32) -> Option<&Node> {
        self.nodes.get(&id)
    }

    /// Caption and reason, for a first draw or after a `Clear`. Marks
    /// the field to be streamed again; the tiles themselves go out from
    /// [`Self::tick`] one per frame.
    pub fn all_ops(&mut self) -> Vec<SceneOp> {
        self.force_render = true;
        self.tile_next = 0;
        self.caption_nodes()
            .into_iter()
            .map(|n| {
                self.nodes.insert(n.id.path_hash(), n.clone());
                SceneOp::Insert(n)
            })
            .collect()
    }

    /// Switch pose. The caption updates now; the field follows on the
    /// next ticks.
    pub fn set_state(&mut self, state: AgentState) -> Vec<SceneOp> {
        if self.state != state {
            self.force_render = true;
            self.tile_next = 0;
        }
        self.state = state;
        self.caption_ops()
    }

    /// Advance the field to `now_ms`. At most one tile update, so the
    /// publish stays inside the pad's mesh frame.
    pub fn tick(&mut self, now_ms: u32) -> Vec<SceneOp> {
        let due = !self.rendered_once || now_ms.saturating_sub(self.last_render_ms) >= FRAME_MS;
        if self.force_render || due {
            let time_s = now_ms as f32 / 1000.0;
            render(&avatar_shader::mood_for(&self.state), time_s, &mut self.field);
            self.last_render_ms = now_ms;
            self.tile_next = 0;
            self.force_render = false;
            self.rendered_once = true;
        }
        if self.tile_next >= TILES {
            return Vec::new();
        }
        let k = self.tile_next;
        self.tile_next += 1;
        let node = self.tile_node(k);
        vec![SceneOp::Update(node)]
    }

    fn tile_node(&mut self, k: usize) -> Node {
        let y0 = k as i32 * TILE_H;
        let row = (FIELD_W * 2) as usize;
        let mut data = Vec::with_capacity(row * TILE_H as usize);
        for row_i in 0..TILE_H {
            let start = ((y0 + row_i) * FIELD_W * 2) as usize;
            data.extend_from_slice(&self.field[start..start + row]);
        }
        let id = ID_FIELD + k as u32;
        let node = Node {
            id: nid(id),
            layer: Layer::Widget,
            transform: Transform::translate(px(FIELD_X), px(FIELD_Y + y0)),
            primitive: Primitive::Bitmap {
                w: px(FIELD_W),
                h: px(TILE_H),
                format: BitmapFormat::Raw565,
                data,
            },
            style: Style::default(),
            input: None,
        };
        self.nodes.insert(id, node.clone());
        node
    }

    fn caption_ops(&mut self) -> Vec<SceneOp> {
        self.caption_nodes()
            .into_iter()
            .map(|n| {
                self.nodes.insert(n.id.path_hash(), n.clone());
                SceneOp::Update(n)
            })
            .collect()
    }

    fn caption_nodes(&self) -> Vec<Node> {
        let cols = SMALL.cols(VISUAL.w - 24);
        let caption = match &self.state {
            AgentState::Idle => "idle",
            AgentState::Listening => "listening",
            AgentState::Thinking => "thinking",
            AgentState::Speaking => "speaking",
            AgentState::Blocked(_) => "blocked",
        };
        let cap_color = if self.state.is_blocked() { AMBER } else { DIM };
        let reason = match &self.state {
            AgentState::Blocked(r) => center(&truncate(r, cols), cols),
            _ => String::new(),
        };
        vec![
            text(ID_CAPTION, &SMALL, VISUAL.x + 12, 400, center(&format!("- {caption} -"), cols), cap_color),
            text(ID_REASON, &SMALL, VISUAL.x + 12, 416, reason, AMBER),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use weftos_leaf_scene::{codec, SceneEnvelope};

    /// Pad `FrameReader` rejects a TCP body longer than this. The body is
    /// the mesh envelope, which is larger than the carrier JSON.
    const PAD_TCP_MAX: usize = 8192;

    fn mesh_frame_len(ops: Vec<SceneOp>) -> (usize, usize, usize) {
        let op_cbor: usize = ops.iter().map(|op| codec::encode(op).expect("op").len()).sum();
        let frame = weftos_leaf_scene::session::SessionFrame::Ops {
            session: weftos_leaf_scene::session::SessionId(1),
            seq: 1,
            more: false,
            envelope: SceneEnvelope::new(0, ops),
        };
        let carrier = frame
            .to_carrier("32d0764012f0a3acc9a20bafd809b8a5")
            .expect("carrier");
        let carrier_json: serde_json::Value = serde_json::from_str(&carrier).expect("carrier json");
        // Same shape `MeshIpcEnvelope` + `KernelMessage` serialize to.
        // Source id is the user weaver's `weave.toml` node_id.
        let env = serde_json::json!({
            "source_node": "42b37925-9ff9-4879-b0e4-c632b097d046",
            "dest_node": "32d0764012f0a3acc9a20bafd809b8a5",
            "message": {
                "id": "ipc-1375198",
                "from": 0,
                "target": {"Topic": "mesh.leaf.32d0764012f0a3acc9a20bafd809b8a5.push"},
                "payload": {"Json": carrier_json},
                "timestamp": "2026-10-14T02:53:07.123456789Z"
            },
            "hop_count": 0,
            "envelope_id": "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee"
        });
        let wire = serde_json::to_vec(&env).expect("envelope");
        (op_cbor, carrier.len(), wire.len())
    }

    #[test]
    fn one_tile_fits_the_pad_mesh_frame() {
        let mut avatar = Avatar::new();
        let ops = avatar.tick(10);
        assert_eq!(ops.len(), 1, "a tick publishes one tile");
        let (op_len, carrier_len, wire_len) = mesh_frame_len(ops);
        eprintln!("tile op_cbor={op_len} carrier={carrier_len} tcp={wire_len}");
        let budget = weftos_leaf_scene::session::MAX_PUSH_CBOR - 64;
        assert!(
            op_len <= budget,
            "one tile is {op_len} bytes; the leaf channel takes {budget}"
        );
        assert!(
            wire_len <= PAD_TCP_MAX,
            "tile mesh frame is {wire_len} bytes (op cbor {op_len}, carrier {carrier_len}); pad cap is {PAD_TCP_MAX}"
        );
    }

    /// A chunk packed to the daemon's CBOR budget, which is what one
    /// `display.ops` of a full redraw becomes. The pad's cap is the
    /// TCP body, so a legal chunk can still be too big.
    #[test]
    fn daemon_chunk_budget_fits_the_pad_tcp_frame() {
        let budget = weftos_leaf_scene::session::MAX_PUSH_CBOR - 64;
        let mut avatar = Avatar::new();
        let mut seed = avatar.tick(10).pop().expect("tile");
        let fits = |n: usize| {
            let mut trial = seed.clone();
            let SceneOp::Update(node) = &mut trial else {
                panic!("tile is an update");
            };
            let Primitive::Bitmap { data, .. } = &mut node.primitive else {
                panic!("tile is a bitmap");
            };
            data.resize(n, 0x5A);
            codec::encode(&trial).expect("encode").len() <= budget
        };
        let mut lo = 0usize;
        let mut hi = budget;
        while lo < hi {
            let mid = (lo + hi).div_ceil(2);
            if fits(mid) {
                lo = mid;
            } else {
                hi = mid - 1;
            }
        }
        let SceneOp::Update(node) = &mut seed else {
            panic!("tile is an update");
        };
        let Primitive::Bitmap { data, .. } = &mut node.primitive else {
            panic!("tile is a bitmap");
        };
        data.resize(lo, 0x5A);
        let (op_len, carrier_len, wire_len) = mesh_frame_len(vec![seed]);
        eprintln!("max-op op_cbor={op_len} carrier={carrier_len} tcp={wire_len} data_len={lo}");
        assert!(
            op_len <= budget,
            "grown tile is {op_len}, budget {budget}"
        );
        assert!(
            wire_len <= PAD_TCP_MAX,
            "max chunk mesh frame is {wire_len} bytes (op cbor {op_len}, carrier {carrier_len}); pad cap is {PAD_TCP_MAX}"
        );
    }
}
