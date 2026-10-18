# ADR-038: Tauri 2.0 as Desktop Application Shell

**Date**: 2026-04-03
**Status**: Superseded by egui shell (2026-04-28)
**Updated**: 2026-10-18 — both Tauri shells (`gui/` with `gui/src-tauri`, and `clawft-ui/src-tauri`) were removed from the tree; they were unbuilt stubs carrying four Dependabot alerts (see `docs/security/dependabot-triage-2026-10-18.md`). Paths below refer to the removed code.
**Deciders**: K8 GUI prototype decision, Sprint 11 Symposium Track 4 (UI/UX Design)

> **Superseded note (2026-04-28, WEFT-242)**: Tauri 2.0 is no longer the desktop shell. The canon GUI is `clawft-gui-egui` (egui/eframe), shipped as both a native binary (`weft-gui-egui`) and a WASM build that the VSCode panel (`extensions/vscode-weft-panel/`) hosts inside Cursor / VS Code. The dual-channel Tauri `invoke` + `kernel://snapshot` event model has been replaced by substrate RPCs (`read`, `subscribe`, `publish`, `notify`) flowing through the egui shell. ADR-005 (xterm.js console), ADR-007 (Zustand + Tauri events), and ADR-013 (JSON block descriptor) are all superseded together as one stack. This ADR is retained for historical context only.

## Context

WeftOS needs a desktop GUI for kernel management, knowledge graph visualization, agent supervision, and interactive console access. The two dominant options for Rust-backed desktop applications are Electron (Chromium + Node.js) and Tauri (native WebView + Rust backend). Electron provides a consistent Chromium environment across platforms but bundles a full browser engine (~150 MB). Tauri uses the platform's native WebView (WebView2 on Windows, WebKitGTK on Linux, WebKit on macOS) with a Rust backend, resulting in significantly smaller binaries.

The K8 GUI prototype implemented a Tauri 2.0 wrapper with Dashboard, Admin Forms, Knowledge Graph visualization (Cytoscape.js), and Component Generator views. ADR-007 established the Zustand + Tauri events state management model, which is Tauri-specific.

## Decision

The desktop GUI uses Tauri 2.0 as the native shell, configured in `gui/src-tauri/` as a separate package excluded from the Cargo workspace:

```toml
# Root Cargo.toml
exclude = ["gui/src-tauri"]

# gui/src-tauri/Cargo.toml
[package]
name = "weftos-gui"
version = "0.1.0"
description = "WeftOS K8 GUI -- Tauri 2.0 desktop shell"
edition = "2024"
rust-version = "1.93"

[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-shell = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
```

**Communication model** (dual-channel, per ADR-007):
- **Tauri `invoke()`**: Request/response commands from React frontend to Rust backend. Commands like `kernel_status`, `spawn_agent`, `list_processes` are defined as `#[tauri::command]` functions in `gui/src-tauri/src/lib.rs` that return `CmdResponse<T>` envelopes.
- **Tauri events** (`kernel://snapshot`): Real-time state push from Rust backend to React frontend. The backend pushes kernel state snapshots (process table, metrics, chain events) every ~500ms or on kernel event bus subscription. The frontend's Zustand StateStore subscribes to these events.

**Frontend**: React renders the GUI, with Zustand for state management. `$state` path references in block descriptors resolve against the Zustand store (ADR-007). Cytoscape.js handles Knowledge Graph visualization.

**Workspace exclusion**: The `gui/src-tauri` crate is excluded from `[workspace]` to avoid coupling GUI build requirements (Tauri CLI, platform WebView SDKs) to the kernel build. This means separate build tooling for the GUI.

**Backend integration**: Tauri commands are currently thin wrappers (marked `// TODO: Wire to real kernel via ServiceApi`) that will route through `ServiceApi` (ADR-035) once the adapter is complete.

## Consequences

### Positive
- Tauri 2.0 produces significantly smaller binaries than Electron (~10 MB vs ~150 MB) by using native WebView instead of bundled Chromium
- Rust backend means GUI commands can call kernel APIs with zero FFI overhead -- same process, same memory space
- The `CmdResponse<T>` envelope provides a uniform success/error contract for all Tauri commands
- Workspace exclusion means kernel developers do not need Tauri build tooling installed to build the core crates
- Tauri 2.0's plugin system (`tauri-plugin-shell`) provides controlled shell access from the frontend

### Negative
- Platform-dependent WebView behavior: WebView2 (Windows), WebKitGTK (Linux), and WebKit (macOS) have rendering differences -- CSS and JavaScript features must be tested across all three
- WebKitGTK on Linux requires system package installation (`libwebkit2gtk-4.1-dev`), adding a dependency not managed by `cargo`
- The dual-channel model (invoke + events) and Zustand integration are Tauri-specific; migrating away from Tauri would require rewriting the communication layer
- Separate build tooling for `gui/src-tauri` means CI must maintain a distinct build step for the GUI

### Neutral
- The GUI is versioned independently (`0.1.0`) from the workspace (`0.3.1`), reflecting its prototype status
- Tauri 2.0 is a major version with breaking changes from Tauri 1.x; the project started on 2.0 and has no migration burden
- The `lib` crate type (`staticlib`, `cdylib`, `rlib`) in `gui/src-tauri/Cargo.toml` supports multiple linking strategies for platform-specific Tauri builds
