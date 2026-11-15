//! Error and result types for the ServiceApi-shaped lattice facade.

use weftos_worldmodel_core::WorldModelError;

/// Errors from the ServiceApi-shaped lattice facade.
#[derive(Debug, Clone, PartialEq)]
pub enum LatticeServiceError {
    /// Unknown service name.
    ServiceNotFound(String),
    /// Unknown method on the lattice service.
    MethodNotFound(String),
    /// Parameter decoding / schema error.
    BadParams(String),
    /// Underlying world-model error.
    WorldModel(WorldModelError),
}

impl std::fmt::Display for LatticeServiceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ServiceNotFound(s) => write!(f, "service not found: {s}"),
            Self::MethodNotFound(m) => write!(f, "method not found: {m}"),
            Self::BadParams(m) => write!(f, "bad params: {m}"),
            Self::WorldModel(e) => write!(f, "world model: {e}"),
        }
    }
}

impl std::error::Error for LatticeServiceError {}

impl From<WorldModelError> for LatticeServiceError {
    fn from(e: WorldModelError) -> Self {
        Self::WorldModel(e)
    }
}

/// Result alias for facade calls.
pub type LatticeServiceResult<T> = Result<T, LatticeServiceError>;
