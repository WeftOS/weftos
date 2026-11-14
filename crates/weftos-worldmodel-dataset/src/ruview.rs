//! RuView recording metadata: the `<session>.csi.meta.json` sidecar the
//! sensing server writes when a recording stops, and the protocol fallbacks
//! that use it.
//!
//! Source: `recording.rs` in RuView's sensing server (`RecordingSession`).

use crate::error::DatasetError;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Mirror of RuView's `RecordingSession`, carried into the manifest as
/// `csi_recording`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CsiRecording {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub label: Option<String>,
    pub started_at: String,
    #[serde(default)]
    pub ended_at: Option<String>,
    pub frame_count: u64,
    pub file_size_bytes: u64,
    pub file_path: String,
}

/// Read and parse the sidecar.
pub fn read_csi_meta(path: &Path) -> Result<CsiRecording, DatasetError> {
    let bytes = fs::read(path).map_err(|e| DatasetError::io(path, e))?;
    serde_json::from_slice(&bytes).map_err(|source| DatasetError::Json {
        path: path.to_path_buf(),
        source,
    })
}

/// Check the sidecar against the session id and the CSI file about to be
/// copied. `csi` holds every CSI file found for the session.
pub fn check_csi_meta(
    meta: &CsiRecording,
    session: &str,
    csi: &[PathBuf],
) -> Result<(), DatasetError> {
    if meta.id != session {
        return Err(DatasetError::CsiMetaMismatch(format!(
            "sidecar id {:?} is not the session id {session:?}",
            meta.id
        )));
    }
    // RuView stores the path it wrote; only the file name is meaningful here.
    let listed = meta
        .file_path
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or_default();
    let wanted = [listed.to_string(), format!("{session}.csi.jsonl")];
    let found = csi.iter().find(|p| {
        let n = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
        wanted.iter().any(|w| w == n)
    });
    let Some(path) = found else {
        return Err(DatasetError::CsiMetaMismatch(format!(
            "sidecar describes {listed:?} but no such CSI file was found"
        )));
    };
    let len = fs::metadata(path)
        .map_err(|e| DatasetError::io(path, e))?
        .len();
    if len != meta.file_size_bytes {
        return Err(DatasetError::CsiMetaMismatch(format!(
            "sidecar file_size_bytes is {} but {} is {len} bytes",
            meta.file_size_bytes,
            path.display()
        )));
    }
    Ok(())
}

/// Class keywords RuView's adaptive-classifier guide expects in recording
/// names, mapped to the class they stand for.
const CLASS_KEYWORDS: [(&str, &str); 8] = [
    ("empty", "empty"),
    ("absent", "empty"),
    ("still", "still"),
    ("sitting", "still"),
    ("walking", "walking"),
    ("moving", "walking"),
    ("active", "active"),
    ("exercise", "active"),
];

/// The first class keyword among the name's alphanumeric tokens, if any.
pub fn class_keyword(session: &str) -> Option<&'static str> {
    session
        .split(|c: char| !c.is_ascii_alphanumeric())
        .find_map(|tok| {
            let tok = tok.to_ascii_lowercase();
            CLASS_KEYWORDS
                .iter()
                .find(|(k, _)| *k == tok)
                .map(|(_, class)| *class)
        })
}

/// Protocol when neither a file nor an explicit value exists: the sidecar's
/// non-empty `label`, then a class keyword in the session name, else `None`.
pub fn fallback_protocol(meta: Option<&CsiRecording>, session: &str) -> Option<Value> {
    meta.and_then(|m| m.label.as_deref())
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .or_else(|| class_keyword(session))
        .map(|s| Value::String(s.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keywords_match_whole_tokens_only() {
        assert_eq!(class_keyword("room-Empty-20261114_101500"), Some("empty"));
        assert_eq!(class_keyword("train_sitting_01"), Some("still"));
        assert_eq!(class_keyword("moving.v2"), Some("walking"));
        assert_eq!(class_keyword("exercise"), Some("active"));
        assert_eq!(class_keyword("stillwater-2026"), None);
        assert_eq!(class_keyword("demo-session-001"), None);
    }

    #[test]
    fn label_beats_keyword_and_blank_label_is_ignored() {
        let mut m = CsiRecording {
            id: "walking-1".into(),
            name: "walking".into(),
            label: Some("seated".into()),
            started_at: String::new(),
            ended_at: None,
            frame_count: 0,
            file_size_bytes: 0,
            file_path: String::new(),
        };
        let v = |s: &str| Some(Value::String(s.into()));
        assert_eq!(fallback_protocol(Some(&m), "walking-1"), v("seated"));
        m.label = Some("  ".into());
        assert_eq!(fallback_protocol(Some(&m), "walking-1"), v("walking"));
        assert_eq!(fallback_protocol(None, "demo-1"), None);
    }
}
