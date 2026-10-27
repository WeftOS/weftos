//! `clawft-app` — WeftOS app manifest parser + local app registry.
//!
//! Implements the subset of [ADR-015][adr] required for milestone M1.5:
//! TOML manifest schema, structural validation rules 1–9, a JSON-backed
//! [`registry::AppRegistry`], the [`install`] pipeline (ADR-015 §Install
//! and OOB first-boot seeding, WEFT-440), and the lifecycle types the
//! desktop compositor will consume when launching apps.
//!
//! Out of scope (lands in sibling crates / later milestones):
//!
//! * Surface description parsing — [`manifest::SurfaceRef`] is just a
//!   string; ADR-016 / the `clawft-surface` crate owns the tree IR.
//! * Live ontology-adapter host registry — rule 6 uses a static
//!   [`adapter_catalog::AdapterCatalog`] stub until ADR-017 /
//!   `clawft-adapter` (or substrate-exported catalogue) replaces it
//!   (WEFT-413). Rule 10 (missing-adapter install-disabled) remains
//!   environmental and host-owned.
//! * Governance — [`lifecycle::governance::CapturePrivacyGate`] (ADR-012)
//!   denies capture without a grant; [`lifecycle::governance::NoopGate`]
//!   always grants. Wired through
//!   `clawft_substrate::Substrate::subscribe_adapter` (WEFT-429).
//! * Full ADR-015 §Lifecycle teardown hooks (surface drop, topic
//!   unsubscribe, affordance revoke) — compositor M1.6+. Until then
//!   [`lifecycle::LifecycleTeardownTombstone`] is emitted on
//!   uninstall-while-enabled so consumers can subscribe (WEFT-412).
//!
//! [adr]: https://github.com/WeftOS/weftos/blob/development-0.7.0/.planning/symposiums/compositional-ui/adrs/adr-015-app-manifest.md

pub mod adapter_catalog;
pub mod install;
pub mod lifecycle;
pub mod manifest;
pub mod registry;
pub mod validation;

pub use adapter_catalog::{AdapterCapability, AdapterCatalog, AdapterChannel};
pub use install::{
    EnsureKind, EnsureOutcome, InstallError, OOB_INSTALLED_AT, ensure_from_toml,
    ensure_from_toml_at, ensure_manifest, ensure_manifest_at, install_from_toml, oob,
};
pub use lifecycle::{
    AppLaunchRequest, AppLaunchResult, LaunchError, LifecycleTeardownTombstone, SessionConfig,
    TeardownReason, TeardownTombstoneBus,
    governance::{
        AdapterOpenResult, CapturePrivacyGate, Gate, NoopGate, StrictGate, infer_capture_permission,
        is_capture, permission_covered,
    },
};
pub use manifest::{AppManifest, EntryPoint, Input, Mode, Permission, SurfaceRef};
pub use registry::{
    AppRegistry, InstalledApp, QuarantineEvent, RegistryError, UninstallResult,
};
pub use validation::{ValidationError, validate, validate_with_catalog};
