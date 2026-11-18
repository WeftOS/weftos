//! How this daemon relates to the mesh, for the handshake (ADR-103 P3-U).
//!
//! The mesh glue ([`crate::mesh_local_glue`]) writes into a [`MeshStateCell`]
//! as the service link comes and goes; `kernel.handshake` reads the daemon's
//! global cell. Tests use their own cells so they never share state.

use std::sync::{Mutex, OnceLock};

use clawft_rpc::handshake::MeshHandshake;

/// Shared, cheaply readable mesh state.
#[derive(Debug, Default)]
pub struct MeshStateCell {
    inner: Mutex<Option<MeshHandshake>>,
}

impl MeshStateCell {
    /// An empty cell (mesh mode not decided yet).
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the state.
    pub fn set(&self, state: MeshHandshake) {
        *self.inner.lock().expect("inner lock poisoned") = Some(state);
    }

    /// Edit the state in place (no-op while undecided).
    pub fn update(&self, f: impl FnOnce(&mut MeshHandshake)) {
        if let Some(s) = self.inner.lock().expect("inner lock poisoned").as_mut() {
            f(s);
        }
    }

    /// Current state.
    pub fn get(&self) -> Option<MeshHandshake> {
        self.inner.lock().expect("inner lock poisoned").clone()
    }

    /// True when the daemon runs as a client of the mesh service.
    pub fn is_service(&self) -> bool {
        self.get().is_some_and(|s| s.mode == "service")
    }
}

/// The daemon's own cell, read by `kernel.handshake`.
pub fn global() -> &'static std::sync::Arc<MeshStateCell> {
    static CELL: OnceLock<std::sync::Arc<MeshStateCell>> = OnceLock::new();
    CELL.get_or_init(|| std::sync::Arc::new(MeshStateCell::new()))
}

/// State for a daemon whose mesh is not served by the service.
pub fn plain(mode: &str) -> MeshHandshake {
    MeshHandshake { mode: mode.to_owned(), ..MeshHandshake::default() }
}
