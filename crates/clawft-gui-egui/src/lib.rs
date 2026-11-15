//! ClawFT GUI — egui shell, usable as native binary or compiled to
//! `wasm32-unknown-unknown` for embedding inside a Cursor / VSCode
//! webview.
//!
//! - Native: see `main.rs` (`weft-gui-egui` bin) / `bin/demo_lab.rs`.
//! - Wasm: call `weft_start(canvas_id)` from JavaScript via
//!   `wasm-bindgen`; the extension host loads the generated JS/wasm
//!   pair from `extensions/vscode-weft-panel/webview/wasm/`.

pub mod app;
pub mod apps;
pub mod blocks;
pub mod canon;
pub mod canon_demos;
pub mod explorer;
pub mod live;
pub mod ontology;
pub mod shell;
pub mod surface_host;
pub mod theming;
pub mod wasm_time;

pub use app::ClawftApp;

// ── Native entry point ───────────────────────────────────────────────

#[cfg(not(target_arch = "wasm32"))]
pub fn run() -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([640.0, 400.0])
            .with_title("ClawFT — egui spike"),
        ..Default::default()
    };
    eframe::run_native(
        "ClawFT egui spike",
        native_options,
        Box::new(|cc| Ok(Box::new(ClawftApp::new(cc)))),
    )
}

// ── Wasm entry point ────────────────────────────────────────────────

#[cfg(target_arch = "wasm32")]
use wasm_bindgen::prelude::*;

/// Mount the egui shell on the given canvas element id.
///
/// Called from `extensions/vscode-weft-panel/webview/wasm/weft_start.js`:
///
/// ```js
/// import init, { weft_start } from "./clawft_gui_egui.js";
/// await init();
/// await weft_start("weft-canvas");
/// ```
#[cfg(target_arch = "wasm32")]
#[wasm_bindgen]
pub async fn weft_start(canvas_id: String) -> Result<(), wasm_bindgen::JsValue> {
    console_error_panic_hook::set_once();

    let window = web_sys::window().ok_or_else(|| JsValue::from_str("no window"))?;
    let document = window
        .document()
        .ok_or_else(|| JsValue::from_str("no document"))?;
    let canvas = document
        .get_element_by_id(&canvas_id)
        .ok_or_else(|| JsValue::from_str("canvas not found"))?
        .dyn_into::<web_sys::HtmlCanvasElement>()
        .map_err(|_| JsValue::from_str("element is not a canvas"))?;

    let web_options = eframe::WebOptions::default();
    eframe::WebRunner::new()
        .start(
            canvas,
            web_options,
            Box::new(|cc| Ok(Box::new(ClawftApp::new(cc)))),
        )
        .await?;
    Ok(())
}

#[cfg(test)]
pub(crate) mod snap_support;
#[cfg(test)]
mod snap_tests;
