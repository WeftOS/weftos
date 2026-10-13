//! Error type shared by the core and the UniFFI surface.

#[derive(Debug, thiserror::Error)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
pub enum NodeError {
    #[error("io: {0}")]
    Io(String),
    #[error("identity: {0}")]
    Identity(String),
    #[error("codec: {0}")]
    Codec(String),
    #[error("pairing: {0}")]
    Pairing(String),
}

impl NodeError {
    pub(crate) fn io(e: impl std::fmt::Display) -> Self {
        Self::Io(e.to_string())
    }
}
