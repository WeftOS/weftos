//! The mesh object store (ADR-126, phase S1).
//!
//! - [`StoreName`]: the `weftos://` name of a store and the bucket derived from it.
//! - [`Access`], [`Principal`], [`Grant`], [`GrantTable`]: who may do what. A name grants
//!   nothing; a grant is (principal, store, read | write | admin).
//! - [`ObjectBackend`] / [`AdminBackend`]: the data plane and the control plane of whatever
//!   holds the bytes. [`MemBackend`] is for tests, [`FsBackend`] keeps buckets in a local
//!   directory. A RustFS (S3 + admin API) backend implements the same two traits.
//! - [`StoreService`]: creates stores, applies grants (into the backend's policies and key
//!   state), and runs every governed request through exposure, grant and audit.
//!
//! No kernel dependency: the daemon supplies an [`ExposureGate`], an [`AuditSink`] and a
//! [`ChainSink`].

#[cfg(test)]
mod prop_tests;
#[cfg(test)]
mod testrng;
pub mod access;
pub mod audit;
pub mod backend;
pub mod drive;
pub mod follower;
pub mod fs;
pub mod grant_seq;
pub mod mem;
pub mod name;
pub mod overlay;
pub mod policy;
pub mod promote;
#[cfg(feature = "rustfs")]
pub mod integrity;
#[cfg(feature = "rustfs")]
pub mod rustfs;
pub mod service;
pub mod share;
pub mod share_fetch;
#[cfg(feature = "rustfs")]
pub mod sigv4;
#[doc(hidden)]
pub mod testing;
pub mod worktree_backend;

pub use access::{Access, Grant, GrantTable, Op, Principal, PrincipalKind};
pub use audit::{AuditEvent, AuditSink, Outcome, Source};
pub use backend::{AccessKey, AdminBackend, BackendError, DefaultRetention, LifecycleRule, LockConfig, LockMode, LockedPut, ObjectBackend, ObjectMeta, ObjectVersion, Part, PutOpts, Retention};
pub use fs::FsBackend;
pub use mem::MemBackend;
pub use name::StoreName;
#[cfg(feature = "rustfs")]
pub use rustfs::{Retry, RustfsBackend, RustfsConfig};
pub use service::{ChainSink, ExposureGate, StoreInfo, StoreService};

/// Errors of the store layer.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// A store name, bucket or object key that cannot be used.
    #[error("bad name: {0}")]
    BadName(String),
    /// The store is not known to this daemon.
    #[error("unknown store {0}")]
    UnknownStore(String),
    /// The store already exists.
    #[error("store {0} already exists")]
    Exists(String),
    /// Exposure, grant or policy said no.
    #[error("refused: {0}")]
    Refused(String),
    /// The backend failed.
    #[error(transparent)]
    Backend(#[from] BackendError),
    /// Grant state could not be read or written.
    #[error("state: {0}")]
    State(String),
}
