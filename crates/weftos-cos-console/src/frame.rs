//! Frame export and session helpers around `weftos-display-client`.
//!
//! The console draws through a real [`DisplaySession`]. For tests and
//! the demo, [`open_sim`] opens one on an in-process [`LeafSim`], and
//! [`render_png`] renders what the leaf holds (its `snapshot()`) to a
//! PNG through the simulator surface, full-frame, so frames never show
//! a stale partial repaint.

use std::path::Path;

use embedded_graphics_simulator::OutputSettingsBuilder;
use weftos_display_client::{DisplayError, DisplayGrant, DisplaySession, DisplayTarget, LeafSim, SessionLimits, SessionOptions};
use weftos_leaf_renderer::render_damage;
use weftos_leaf_scene::{DamageSet, Rect, Scene, SceneOp, SceneStore};
use weftos_leaf_sim::SimSurface;

use crate::theme::{BLACK, DISPLAY, SCREEN_H, SCREEN_W};

/// The console's grant subject.
pub const SUBJECT: &str = "cos-console";

/// A session on a fresh simulated 800×480 leaf, with the leaf handle
/// for injecting touches and taking snapshots.
pub fn open_sim() -> Result<(DisplaySession, LeafSim), DisplayError> {
    let (target, sim) = DisplayTarget::sim();
    let session = DisplaySession::open(target, DisplayGrant::dev(SUBJECT))?;
    Ok((session, sim))
}

/// Like [`open_sim`] but without the op-rate bound, for tests that
/// replay a long conversation faster than real time.
pub fn open_sim_unmetered() -> Result<(DisplaySession, LeafSim), DisplayError> {
    let (target, sim) = DisplayTarget::sim();
    let opts = SessionOptions {
        limits: SessionLimits {
            max_ops_per_sec: u32::MAX,
            ..SessionLimits::d1()
        },
        ..SessionOptions::default()
    };
    let session = DisplaySession::open_with(target, DisplayGrant::dev(SUBJECT), opts)?;
    Ok((session, sim))
}

/// Render a leaf snapshot to `path` as a PNG.
pub fn render_png(scene: &Scene, path: impl AsRef<Path>) -> Result<(), String> {
    let mut store = SceneStore::new();
    store.set_viewport(DISPLAY, Rect::from_px(0, 0, SCREEN_W, SCREEN_H));
    let _ = store.set_bg(DISPLAY, BLACK);
    let _ = store.apply_op(DISPLAY, &SceneOp::Replace(scene.clone()));
    let mut surface = SimSurface::new(SCREEN_W as u32, SCREEN_H as u32, "frame");
    surface.set_clear_color(BLACK);
    render_damage(&store, DISPLAY, &DamageSet::full(), &mut surface).map_err(|e| format!("{e:?}"))?;
    let settings = OutputSettingsBuilder::new().scale(1).build();
    surface
        .display()
        .to_rgb_output_image(&settings)
        .save_png(path.as_ref())
        .map_err(|e| e.to_string())
}
