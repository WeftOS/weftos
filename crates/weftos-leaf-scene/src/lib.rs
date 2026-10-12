//! # weftos-leaf-scene
//!
//! Phase A of the WeftOS vector-first leaf display. This crate carries
//! the **scene graph**, **wire format envelopes**, **damage
//! computation**, and **hit-test** logic that every subsequent leaf
//! renderer and surface backend consumes.
//!
//! Canonical design doc: `docs/design/vector-leaf-display.md`.
//!
//! ## Shape at a glance
//!
//! ```text
//!   host ─► CBOR(SceneEnvelope) ─► [codec::decode] ─► SceneStore::apply ─► DamageSet
//!                                                                  │
//!                                                                  ▼
//!                                                          (renderer, Phase B)
//!
//!   leaf ─► CBOR(InputEnvelope) ◄─ [hit_test on touch event] ◄─ SceneStore::hit_test
//! ```
//!
//! ## What's in this crate
//!
//! | Module       | Contents                                                            |
//! |--------------|---------------------------------------------------------------------|
//! | [`id`]       | [`NodeId`], [`DisplayId`], producer-side `path_to_id` hashing       |
//! | [`geometry`] | Q24.8 fixed-point, [`Rect`], [`Point`], [`Size`], [`Transform`]     |
//! | [`color`]    | [`Rgba`], [`Rgb`], lerp helpers                                     |
//! | [`primitive`]| [`Primitive`], [`Style`], [`Layer`], [`BlendMode`], hit-shape types |
//! | [`node`]     | [`Node`] + AABB computation                                          |
//! | [`scene`]    | [`Scene`] snapshot type                                             |
//! | [`tween`]    | Active-tween table + coalescing                                     |
//! | [`damage`]   | [`DamageSet`] (8-rect budget + 50% threshold)                       |
//! | [`op`]       | [`SceneOp`] (the unit of mutation)                                  |
//! | [`envelope`] | [`SceneEnvelope`], [`InputEnvelope`], [`WIRE_VERSION`]              |
//! | [`store`]    | [`SceneStore`] runtime — apply, tick, hit_test, to_snapshot         |
//! | [`codec`]    | CBOR encode/decode + version-byte rejection                         |
//! | [`session`]  | Display-session frames and the leaf-side session state machine      |
//!
//! ## What's **not** in this crate
//!
//! - Glyph caching, font rasterization → `weftos-leaf-renderer` (Phase B).
//! - Surface backends (DPI, SimSurface, CanvasSurface) → Phase B / C / D.
//! - Mesh transport, Noise handshake → unchanged in `clawft-edge-pad` /
//!   `clawft-weave`.
//! - `LeafServices` announce caps → stays in `weftos-leaf-types`.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

pub mod codec;
pub mod color;
pub mod damage;
pub mod envelope;
pub mod geometry;
pub mod id;
pub mod node;
pub mod op;
pub mod primitive;
pub mod scene;
pub mod session;
pub mod store;
pub mod tween;

// Re-exports for ergonomics. Producers and renderers typically use
// these top-level symbols rather than reaching into submodules.
pub use color::{Rgb, Rgba};
pub use damage::DamageSet;
pub use envelope::{InputEnvelope, InputEvent, SceneEnvelope, WIRE_VERSION};
pub use geometry::{from_px_q8, px, Point, Rect, Size, Transform};
pub use id::{path_to_id, DisplayId, NodeId};
pub use node::Node;
pub use op::SceneOp;
pub use primitive::{
    BitmapFormat, BlendMode, BuiltinFont, CursorHint, EaseCurve, FontFace, FontStyle, HitShape,
    InputRegion, KerningHint, Layer, PathCmd, Primitive, Style,
};
pub use scene::Scene;
pub use store::{DisplayState, SceneStore};
pub use tween::{ActiveTween, AnimatableProperty, PropertyValue, TweenTable};

// Compile-time guards: every public type should be Send + Sync. The
// browser / desktop sim / future host-side preview all want to share a
// SceneStore between producer and renderer tasks. See design doc
// Appendix B for the rationale.
const _: fn() = || {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<NodeId>();
    assert_send_sync::<DisplayId>();
    assert_send_sync::<Rgba>();
    assert_send_sync::<Rect>();
    assert_send_sync::<Point>();
    assert_send_sync::<Size>();
    assert_send_sync::<Transform>();
    assert_send_sync::<Style>();
    assert_send_sync::<Primitive>();
    assert_send_sync::<Node>();
    assert_send_sync::<Scene>();
    assert_send_sync::<SceneOp>();
    assert_send_sync::<SceneEnvelope>();
    assert_send_sync::<InputEnvelope>();
    assert_send_sync::<DamageSet>();
    assert_send_sync::<SceneStore>();
    assert_send_sync::<ActiveTween>();
    assert_send_sync::<TweenTable>();
};
