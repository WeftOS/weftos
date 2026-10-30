//! Shared error type for the Android edge core.

use std::fmt;

/// Edge-core failures surfaced to Kotlin via UniFFI or to Rust callers.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
pub enum EdgeError {
    Io { message: String },
    Identity { message: String },
    Pair { message: String },
    Upload { message: String },
    InvalidArg { message: String },
}

impl EdgeError {
    /// Builds the I/O failure variant from any displayable error.
    pub fn io(e: impl fmt::Display) -> Self {
        Self::Io {
            message: e.to_string(),
        }
    }
    /// Builds the identity load/generate failure variant from any displayable error.
    pub fn identity(e: impl fmt::Display) -> Self {
        Self::Identity {
            message: e.to_string(),
        }
    }
    /// Builds the pairing failure variant from any displayable error.
    pub fn pair(e: impl fmt::Display) -> Self {
        Self::Pair {
            message: e.to_string(),
        }
    }
    /// Builds the upload failure variant from any displayable error.
    pub fn upload(e: impl fmt::Display) -> Self {
        Self::Upload {
            message: e.to_string(),
        }
    }
    /// Builds the invalid argument variant from any displayable error.
    pub fn invalid(e: impl fmt::Display) -> Self {
        Self::InvalidArg {
            message: e.to_string(),
        }
    }
}

impl fmt::Display for EdgeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { message } => write!(f, "io: {message}"),
            Self::Identity { message } => write!(f, "identity: {message}"),
            Self::Pair { message } => write!(f, "pair: {message}"),
            Self::Upload { message } => write!(f, "upload: {message}"),
            Self::InvalidArg { message } => write!(f, "invalid: {message}"),
        }
    }
}

impl std::error::Error for EdgeError {}

impl From<std::io::Error> for EdgeError {
    fn from(value: std::io::Error) -> Self {
        Self::io(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_strings() {
        assert_eq!(EdgeError::io("m").to_string(), "io: m");
        assert_eq!(EdgeError::identity("m").to_string(), "identity: m");
        assert_eq!(EdgeError::pair("m").to_string(), "pair: m");
        assert_eq!(EdgeError::upload("m").to_string(), "upload: m");
        assert_eq!(EdgeError::invalid("m").to_string(), "invalid: m");
    }

    #[test]
    fn error_from_io_error_is_io_variant() {
        let e: EdgeError = std::io::Error::other("boom").into();
        assert!(matches!(e, EdgeError::Io { ref message } if message == "boom"));
    }
}
