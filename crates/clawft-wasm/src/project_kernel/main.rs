//! Persistent project kernel vertical slice. No native launcher or tool engine.
//! Reuses the actual chain, governance and protocol sources, avoiding the
//! native project bootstrap's Tokio, OS key handling and threaded anchor path.
#![allow(unexpected_cfgs, dead_code)]
mod abi;
mod forward;
mod identity;
mod storage;
mod supervisor;

pub use clawft_kernel::governance;
// The native source imports `Arc` for its threaded paths, which this target omits.
#[allow(unused_imports)]
#[path = "../../../clawft-kernel/src/chain.rs"]
pub mod chain;
// Native source, linted under the native build; here only its portable half compiles.
#[allow(clippy::derivable_impls)]
#[path = "../../../clawft-kernel/src/chain_subscribe.rs"]
pub mod chain_subscribe;
// ADR-129 checkpoint signing: real ML-DSA-65 (pure Rust) in the guest too.
#[path = "../../../clawft-kernel/src/chain_pq.rs"]
pub mod chain_pq;
#[path = "../../../clawft-kernel/src/governance_overlay.rs"]
pub mod governance_overlay;
#[path = "../../../clawft-kernel/src/parent_policy.rs"]
pub mod parent_policy;
// These constants are the only overlay-runtime dependencies of the pure
// merge module. The guest implements its own persistence, never native boot.
mod overlay_runtime {
    pub const DEFAULT_RISK_THRESHOLD: f64 = 0.7;
}
mod overlay_trust {
    pub const VERSION_PIN_FILE: &str = "parent-policy.version";
}
#[path = "../../../clawft-rpc/src/mesh_local.rs"]
mod mesh_local;
mod handshake {
    #[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub struct ProtoRange {
        pub current: u32,
        pub min: u32,
    }
}

type Error = Box<dyn std::error::Error + Send + Sync>;
type Result<T> = std::result::Result<T, Error>;

#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "weftos_project_v1")]
unsafe extern "C" {
    fn exchange(op: i32, input: *const u8, input_len: i32, output: *mut u8, capacity: i32) -> i32;
}
fn bridge(op: i32, input: &[u8]) -> Result<Vec<u8>> {
    if input.len() > abi::MAX_FRAME {
        return Err("frame too large".into());
    }
    #[cfg(target_arch = "wasm32")]
    {
        let mut out = vec![0; abi::MAX_FRAME];
        // Both slices are live, non-overlapping and bounded for the call.
        let n = unsafe {
            exchange(
                op,
                input.as_ptr(),
                input.len() as i32,
                out.as_mut_ptr(),
                out.len() as i32,
            )
        };
        if n < 0 || n as usize > out.len() {
            return Err("invalid host response".into());
        }
        out.truncate(n as usize);
        Ok(out)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = op;
        Err("project guest requires the WASM runner".into())
    }
}

fn main() {
    // WASI stderr is closed by default. A failed guest exits nonzero and the
    // runner reports failure; no signing material crosses an error boundary.
    if supervisor::run().is_err() {
        std::process::exit(1);
    }
}
