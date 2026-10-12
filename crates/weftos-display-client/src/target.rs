//! Where a session draws.

use weftos_leaf_scene::DisplayId;

use crate::backend::sim::LeafSim;
use crate::caps::DisplaySize;

/// A real leaf's display service, named per ADR-114.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeafEndpoint {
    /// `weftos://<mesh>/nodes/<leaf node id>/services/display[-<n>]`.
    pub name: String,
    /// The leaf's certified public key, hex, as `weaver leaf` uses it.
    pub leaf_id: String,
    pub display: DisplayId,
    /// The daemon's Unix socket. `None` uses `$WEFTOS_RUNTIME_DIR/kernel.sock`,
    /// else `~/.clawft/kernel.sock`.
    pub daemon: Option<std::path::PathBuf>,
}

/// Desktop window settings for [`DisplayTarget::Window`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowConfig {
    pub title: String,
    pub width: u32,
    pub height: u32,
    /// Integer zoom of the window; 1 is pixel-for-pixel.
    pub scale: u32,
}

impl Default for WindowConfig {
    fn default() -> Self {
        Self {
            title: "WeftOS display".to_string(),
            width: DisplaySize::PAD.width,
            height: DisplaySize::PAD.height,
            scale: 1,
        }
    }
}

/// The three backends. `Window` needs the `window` feature; without it
/// `open` returns `DisplayError::NotYet` rather than failing to compile,
/// so lane B code can name every target today. `Leaf` goes through the
/// daemon and fails with a `Backend` error when it cannot be reached.
#[derive(Clone)]
pub enum DisplayTarget {
    /// In-process simulated leaf; keep a clone of the handle to drive it.
    LeafSim(LeafSim),
    /// The simulated leaf in a desktop window (mouse = touch, space = ptt).
    Window(WindowConfig),
    /// A real leaf over the certified-leaf channel.
    Leaf(LeafEndpoint),
}

impl DisplayTarget {
    /// A fresh 800×480 simulated leaf, with its handle for tests.
    pub fn sim() -> (Self, LeafSim) {
        let sim = LeafSim::default();
        (Self::LeafSim(sim.clone()), sim)
    }

    /// A default desktop window.
    pub fn window() -> Self {
        Self::Window(WindowConfig::default())
    }
}
