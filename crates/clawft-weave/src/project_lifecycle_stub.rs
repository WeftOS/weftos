//! What `project.start|stop|restart|status|ensure_running|stop_all` and
//! `project.token.refresh` answer in a build without the project supervisor
//! (non-unix, or without the `exochain` and `placement` features): the routes
//! exist, so a client gets a typed refusal instead of `unknown method`.
//!
//! `lib.rs` aliases this module as `project_lifecycle_rpc` in those builds;
//! it is compiled in every build so its behaviour is tested by the default
//! test run. That the aliased build compiles is checked with
//! `cargo check -p clawft-weave --no-default-features --features exochain,ecc,mesh`
//! (plain `--no-default-features` fails on older, unrelated debt; `cargo test`
//! cannot run that feature set, because the crate's self dev-dependency turns
//! the defaults back on).

use crate::rpc_ext::{ExtCall, ExtFuture};

/// The `error_kind` every route answers with.
pub const KIND: &str = "not_user_daemon";

/// Every lifecycle method: this build cannot supervise project kernels.
pub fn handle(_call: ExtCall) -> ExtFuture {
    Box::pin(async {
        clawft_rpc::Response::error_with_kind(KIND, "project supervision is not available in this build")
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::capability::CallerCapabilities;
    use crate::rpc_ext::{ExtCtx, KernelRef};

    #[tokio::test]
    async fn every_lifecycle_route_answers_not_user_daemon_without_the_supervisor() {
        let kcfg = clawft_types::config::KernelConfig {
            chain: Some(clawft_types::config::ChainConfig::isolated_in(&tempfile::tempdir().unwrap().keep())),
            ..Default::default()
        };
        let kernel = clawft_kernel::boot::Kernel::boot(
            clawft_types::config::Config::default(),
            kcfg,
            Arc::new(clawft_platform::NativePlatform::new()),
        )
        .await
        .expect("kernel boots");
        let kernel: KernelRef = Arc::new(tokio::sync::RwLock::new(kernel));
        for method in [
            "project.start",
            "project.ensure_running",
            "project.stop",
            "project.restart",
            "project.status",
            "project.stop_all",
            "project.token.refresh",
        ] {
            let call = ExtCall {
                method: method.to_owned(),
                params: serde_json::json!({"id": "01JB8Z3Q0V6X9KQ4M2N7T5R1WD"}),
                ctx: ExtCtx {
                    kernel: Arc::clone(&kernel),
                    auth: Some("admin".into()),
                    project: None,
                    verified_project: None,
                    caps: CallerCapabilities::from_scopes(["admin"]),
                    connection: None,
                },
            };
            let r = handle(call).await;
            assert!(!r.ok, "{method}");
            assert_eq!(r.error_kind.as_deref(), Some(KIND), "{method}");
        }
    }
}
