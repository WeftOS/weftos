//! Guard: the workspace `ort` requirement must be an exact pin.
//!
//! `ort = "2.0.0-rc.12"` is a prerelease requirement, so a routine
//! `cargo update` moves it to rc.15, whose `Session::builder` takes an
//! `&Environment`. That broke every `onnx` build (voice-onnx, voice-tts,
//! voice-talk, kernel `onnx-embeddings`) while the default-feature gates stayed
//! green, because `ort` is optional. The pin and the callers move together.

use std::path::Path;

fn workspace_manifest() -> String {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    std::fs::read_to_string(root).expect("read workspace Cargo.toml")
}

#[test]
fn workspace_ort_requirement_is_an_exact_pin() {
    let manifest = workspace_manifest();
    let line = manifest
        .lines()
        .find(|l| l.trim_start().starts_with("ort = "))
        .expect("workspace Cargo.toml has an `ort` dependency");
    assert!(
        line.contains("version = \"="),
        "workspace `ort` must be pinned with `=` (found: {line}); see crates/clawft-voice-onnx/tests/ort_pin.rs"
    );
}
