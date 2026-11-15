//! Workload-kind registry (cog-boundary audit M3; ADR-099 section 2).
//!
//! A workload kind owns the pieces of the placement path that depend on
//! what is being placed: manifest body validation, how a verified package
//! becomes a [`WorkloadSpec`], and which adapter routes can run it. The
//! rest of the path (signature checks, seeding, gating, scoring, dispatch)
//! is kind-agnostic and looks the kind up here instead of assuming `cog`.
//!
//! Registration is explicit: [`KindRegistry::builtin`] builds a registry
//! holding the in-tree kinds (`cog`), and callers add more with
//! [`KindRegistry::register`]. There is no global mutable state.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use clawft_types::placement::engine::WorkloadSpec;

#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use crate::workload_ctl::PlaneError;
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use crate::workload_pkg::TrustAnchors;
use crate::workload_pkg::{ManifestEnvelope, ManifestError};
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
use crate::workload_runtime::VerifiedWorkload;

mod cog;
mod computer;
mod health;
mod project;

#[cfg(test)]
mod tests;
#[cfg(test)]
mod computer_tests;
#[cfg(all(test, feature = "workload-runtime", feature = "mesh", unix, feature = "native"))]
mod project_tests;

pub use cog::CogKind;
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
pub use computer::{CAP_COMPUTER_PREFIX, computer_workload};
pub use computer::{
    Access as ComputerAccess, Allowlist as ComputerAllowlist, ComputerBody, ComputerKind, KIND_COMPUTER,
    Verdict as ComputerVerdict,
};
pub use health::{Health, HealthSample, HealthSpec, SampleState, judge_process};
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix, feature = "native"))]
pub use project::{ProjectFacts, ProjectPrepareError, prepare_project};
pub use project::{KIND_PROJECT, ProjectKind, project_healthy};
#[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
pub use cog::load_cog_shaped;

/// Longest kind id accepted from a manifest (and the most unauthenticated
/// kind text ever echoed into an error or chain event).
pub const MAX_KIND_LEN: usize = 32;

/// No kind is registered under this id (the id is truncated to
/// [`MAX_KIND_LEN`] characters).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown workload kind {0:?}")]
pub struct UnknownKind(pub String);

/// A kind id was registered twice.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("workload kind {0:?} is already registered")]
pub struct DuplicateKind(pub String);

/// What a workload kind contributes to validation and placement.
pub trait WorkloadKind: Send + Sync {
    /// The manifest `kind` string this implementation handles.
    fn id(&self) -> &'static str;

    /// Validate the envelope's kind-specific body (shape and limits; not
    /// signatures or file contents, which are kind-agnostic).
    fn validate(&self, envelope: &ManifestEnvelope) -> Result<(), ManifestError>;

    /// How often instances of this kind are polled and how many bad polls
    /// in a row make one unhealthy (ADR-099 section 7).
    fn health(&self) -> HealthSpec {
        HealthSpec::default()
    }

    /// Judge one poll of an instance of this kind. The default is the
    /// process-backed judgment ([`judge_process`]).
    fn judge(&self, sample: &HealthSample) -> Health {
        judge_process(sample)
    }

    /// Whether the placer may move an instance of this kind to another
    /// node when its node is lost. A kind with state that cannot move (warm
    /// inference caches, hardware-bound work) says `false`: the loss is
    /// raised as an alert instead of being rescheduled.
    fn migratable(&self) -> bool {
        true
    }

    /// Whether the supervisor restarts an instance of this kind that has
    /// been unhealthy for `miss_limit` polls. A kind whose failure a restart
    /// cannot fix (a desktop that went quiet) says `false`: the instance is
    /// marked unhealthy and the reason is raised as an alert instead.
    fn restart_on_unhealthy(&self) -> bool {
        true
    }

    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    /// Verify the signed package in `package_dir` against `anchors` (with
    /// `kinds` as the registry the manifest is checked against) and load it
    /// as a runnable workload. Must refuse a manifest whose kind is not
    /// [`id`](Self::id).
    fn load(
        &self,
        package_dir: &Path,
        anchors: &TrustAnchors,
        kinds: &KindRegistry,
    ) -> Result<VerifiedWorkload, PlaneError>;

    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    /// The placement requirements for a loaded workload of this kind.
    fn spec(&self, workload: &VerifiedWorkload) -> Result<WorkloadSpec, String>;

    #[cfg(all(feature = "workload-runtime", feature = "mesh", unix))]
    /// Adapter route names that can run this kind (`native`, `container`,
    /// `emulated`, `remote.api`, ...).
    fn adapters(&self) -> &'static [&'static str];
}

/// Kinds by id.
#[derive(Clone, Default)]
pub struct KindRegistry {
    kinds: BTreeMap<&'static str, Arc<dyn WorkloadKind>>,
}

impl KindRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The in-tree kinds: `cog`, `computer` and `project`.
    pub fn builtin() -> Self {
        let mut r = Self::new();
        r.register(Arc::new(CogKind))
            .expect("builtin kind ids are distinct");
        r.register(Arc::new(ComputerKind))
            .expect("builtin kind ids are distinct");
        r.register(Arc::new(ProjectKind))
            .expect("builtin kind ids are distinct");
        r
    }

    /// Add a kind; refuses a second registration of the same id.
    pub fn register(&mut self, kind: Arc<dyn WorkloadKind>) -> Result<(), DuplicateKind> {
        let id = kind.id();
        if self.kinds.contains_key(id) {
            return Err(DuplicateKind(id.to_string()));
        }
        self.kinds.insert(id, kind);
        Ok(())
    }

    /// The kind registered under `id`.
    pub fn get(&self, id: &str) -> Option<&Arc<dyn WorkloadKind>> {
        self.kinds.get(id)
    }

    /// Like [`get`](Self::get) with a structured error for an unknown id.
    pub fn require(&self, id: &str) -> Result<&Arc<dyn WorkloadKind>, UnknownKind> {
        self.get(id)
            .ok_or_else(|| UnknownKind(id.chars().take(MAX_KIND_LEN).collect()))
    }

    /// Registered ids in order.
    pub fn ids(&self) -> Vec<&'static str> {
        self.kinds.keys().copied().collect()
    }
}

impl std::fmt::Debug for KindRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_set().entries(self.kinds.keys()).finish()
    }
}

/// Validate an envelope through the registry: the kind must be
/// registered and its body must pass that kind's validation.
pub fn validate_envelope(
    registry: &KindRegistry,
    envelope: &ManifestEnvelope,
) -> Result<(), ManifestError> {
    registry.require(&envelope.kind)?.validate(envelope)
}

/// The `kind` a package directory's manifest declares, if the manifest is
/// a bounded regular file that parses far enough to say and the kind is a
/// valid token of at most [`MAX_KIND_LEN`] characters. `None` leaves the
/// failure to package verification, which reports it precisely. The result
/// is unauthenticated: it only selects which registered kind verifies the
/// package, and that kind re-checks the verified envelope.
pub fn peek_manifest_kind(package_dir: &Path) -> Option<String> {
    use crate::workload_pkg::verify::read_bounded;
    use crate::workload_pkg::{MANIFEST_FILE, MAX_MANIFEST_BYTES};
    let bytes = read_bounded(
        &package_dir.join(MANIFEST_FILE),
        MANIFEST_FILE,
        MAX_MANIFEST_BYTES as u64,
    )
    .ok()?;
    peek_kind_in(&bytes)
}

/// [`peek_manifest_kind`] over manifest bytes the caller already read, so
/// one read serves the peek and the verification that follows.
pub fn peek_kind_in(manifest: &[u8]) -> Option<String> {
    use crate::workload_pkg::manifest::valid_token;
    let doc: serde_json::Value = serde_json::from_slice(manifest).ok()?;
    let kind = doc.get("kind")?.as_str()?;
    valid_token(kind, MAX_KIND_LEN).then(|| kind.to_string())
}
