//! Error type for the dataset archiver.

use std::path::PathBuf;

/// Everything that can stop an archive run. Validation errors carry the
/// 1-based line number of the first bad line.
#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    #[error("io error on {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("invalid session id {0:?}: use 1-128 chars of A-Z a-z 0-9 . _ - and no leading dot")]
    InvalidSession(String),
    #[error("cannot infer the session id: {0}")]
    SessionNotInferable(String),
    #[error("required evidence file not found: {0}")]
    MissingEvidence(PathBuf),
    #[error("bad layout template {0:?}: {1}")]
    BadLayout(String, String),
    #[error("{file}:{line}: evidence line rejected: {reason}")]
    Evidence {
        file: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("{file}:{line}: readings line rejected: {reason}")]
    Readings {
        file: PathBuf,
        line: usize,
        reason: String,
    },
    #[error("evidence file {0} holds no records")]
    EmptyEvidence(PathBuf),
    #[error("evidence spans more than one region: {0:?}")]
    MultipleRegions(Vec<String>),
    #[error(
        "consent is absent: provide consent.json next to the session or an explicit consent value"
    )]
    ConsentMissing,
    #[error("destination already exists: {0} (set overwrite to replace it)")]
    DestinationExists(PathBuf),
    #[error("{path} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        #[source]
        source: serde_json::Error,
    },
    #[error("copy verification failed: {0}")]
    VerificationFailed(String),
}

impl DatasetError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }
}
