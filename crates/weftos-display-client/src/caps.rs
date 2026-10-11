//! What a display reports about itself (ADR-117 §1): size, the
//! renderer's capability mask, media support and input kinds.

use weftos_leaf_renderer::CapabilityMask;
use weftos_leaf_scene::BitmapFormat;

/// Display size in whole pixels. (Scene coordinates are Q24.8; this
/// type is deliberately distinct from `weftos_leaf_scene::Size`.)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplaySize {
    pub width: u32,
    pub height: u32,
}

impl DisplaySize {
    /// The CrowPanel edge pad and the simulator default.
    pub const PAD: DisplaySize = DisplaySize {
        width: 800,
        height: 480,
    };
}

/// Bounded video into one media node (ADR-117 §6). `None` until D5.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoCapability {
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u8,
}

/// Which input the display produces.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct InputKinds {
    /// Pointer / touch events resolved to objects.
    pub touch: bool,
    /// Named buttons that arrive as `ObjectEvent::Sensor` (`"ptt"`).
    pub buttons: Vec<String>,
}

/// The display's service record as the client sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DisplayCapabilities {
    /// What the renderer can draw natively.
    pub render: CapabilityMask,
    /// Bitmap formats a `Primitive::Bitmap` may use.
    pub image_formats: Vec<BitmapFormat>,
    /// Video support, when the leaf has it.
    pub video: Option<VideoCapability>,
    pub input: InputKinds,
}

impl DisplayCapabilities {
    /// What the simulator backends report: the sim's rich render mask,
    /// the two raw bitmap formats the renderer decodes, no video,
    /// touch plus a push-to-talk button.
    pub fn sim(render: CapabilityMask) -> Self {
        Self {
            render,
            image_formats: vec![BitmapFormat::Raw8888, BitmapFormat::Raw565],
            video: None,
            input: InputKinds {
                touch: true,
                buttons: vec!["ptt".to_string()],
            },
        }
    }
}
