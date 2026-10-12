//! # weftos-display-client
//!
//! ADR-117 phase D1: an app's handle on a display endpoint. Open a
//! session, send [`SceneOp`]s, get back object-level events.
//!
//! ```no_run
//! use weftos_display_client::{DisplayGrant, DisplaySession, DisplayTarget, ObjectEvent};
//! use weftos_display_client::scene::{px, HitShape, InputRegion, Layer, Node, NodeId, Primitive, Rgba, SceneOp, Style, Transform, CursorHint};
//!
//! let (target, _sim) = DisplayTarget::sim();
//! let mut s = DisplaySession::open(target, DisplayGrant::dev("example"))?;
//!
//! let mut button = Node::new(NodeId::from_raw(1), Layer::Widget, Primitive::Rect { w: px(120), h: px(48), radius_q8: 0 });
//! button.transform = Transform::translate(px(40), px(40));
//! button.style = Style::filled(Rgba::opaque(0x22, 0x55, 0x99));
//! button.input = Some(InputRegion { shape: HitShape::Aabb { w: px(120), h: px(48) }, cursor_hint: CursorHint::Pointer, capture: true });
//! s.apply(&[SceneOp::Insert(button)])?;
//!
//! while let Some(ev) = s.poll_event() {
//!     if let ObjectEvent::Tap { node } = ev { println!("tapped {node:?}"); }
//! }
//! # Ok::<(), weftos_display_client::DisplayError>(())
//! ```
//!
//! ## Shape
//!
//! | Module | Contents |
//! |---|---|
//! | [`session`] | [`DisplaySession`]: `open`, `apply`, `poll_event`, `size`, `capabilities`, `close` |
//! | [`target`] | [`DisplayTarget`]: `LeafSim`, `Window`, `Leaf` |
//! | [`event`] | [`ObjectEvent`]: `Tap`, `Drag`, `Sensor` (`ptt.down` / `ptt.up`) |
//! | [`limits`] | [`SessionLimits`]: 4,096 nodes, op rate, image budget |
//! | [`grant`] | [`DisplayGrant`] and the [`GrantGate`] seam for D2 |
//! | [`caps`] | [`DisplaySize`], [`DisplayCapabilities`] |
//! | [`backend`] | the leaf seam: simulator, desktop window, real leaf |
//!
//! The scene vocabulary is `weftos-leaf-scene`, re-exported as
//! [`scene`] so an app needs one dependency.

pub mod backend;
pub mod caps;
pub mod error;
pub mod event;
pub mod gesture;
pub mod grant;
pub mod keys;
pub mod limits;
pub mod session;
pub mod target;
#[cfg(feature = "wire-leaf")]
pub mod wire_leaf;

/// The scene vocabulary (`SceneOp`, `Node`, `NodeId`, …).
pub use weftos_leaf_scene as scene;
pub use weftos_leaf_scene::{DisplayId, NodeId, SceneOp};

pub use backend::sim::LeafSim;
pub use caps::{DisplayCapabilities, DisplaySize, InputKinds, VideoCapability};
pub use error::DisplayError;
pub use event::{ObjectEvent, SensorValue, SENSOR_PTT_DOWN, SENSOR_PTT_UP};
pub use grant::{AdmitAll, Arbitration, DisplayGrant, GrantGate};
pub use keys::{KEY_BACKSPACE, KEY_ESCAPE, KEY_RETURN, SENSOR_KEY_CHAR, SENSOR_KEY_NAMED};
pub use limits::SessionLimits;
pub use session::{DisplaySession, SessionOptions};
pub use target::{DisplayTarget, LeafEndpoint, WindowConfig};
