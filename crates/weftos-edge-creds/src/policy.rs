//! Names, input validation and the per-lease session policy.
//!
//! The bucket is derived from the authenticated node id, never from the
//! request (ADR-126 section 2: names are derived, never chosen by hand).
//! S1's `weftos-store` owns the canonical name mapping; [`node_bucket`] is the
//! one-line ADR-126 section 2 rule for nodes and should be replaced by the
//! S1 function when S1 lands.

use serde_json::json;

/// Access a lease grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum NameError {
    #[error("invalid node id")]
    NodeId,
    #[error("invalid object key")]
    Key,
}

/// 1..=48 chars of `[a-z0-9-]`, not starting or ending with `-`.
pub fn validate_node_id(id: &str) -> Result<(), NameError> {
    let ok = !id.is_empty()
        && id.len() <= 48
        && !id.starts_with('-')
        && !id.ends_with('-')
        && id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-');
    if ok { Ok(()) } else { Err(NameError::NodeId) }
}

/// The longest single path segment RustFS accepts (a file name on its disk).
pub const MAX_SEGMENT_BYTES: usize = 255;

/// Object key rules: no traversal, no empty or dot segments, no control
/// characters, no backslash, at most 1024 bytes, no path segment over 255
/// bytes (RustFS 1.0.4 refuses one with `InvalidArgument`, measured 2026-11-08),
/// relative.
pub fn validate_key(key: &str) -> Result<(), NameError> {
    if key.is_empty() || key.len() > 1024 || key.starts_with('/') {
        return Err(NameError::Key);
    }
    if key.chars().any(|c| c.is_control() || c == '\\') {
        return Err(NameError::Key);
    }
    if key.split('/').any(|seg| seg.is_empty() || seg == "." || seg == ".." || seg.len() > MAX_SEGMENT_BYTES) {
        return Err(NameError::Key);
    }
    Ok(())
}

/// ADR-126 section 2: node store is bucket `n-<node id>`.
pub fn node_bucket(node_id: &str) -> Result<String, NameError> {
    validate_node_id(node_id)?;
    Ok(format!("n-{node_id}"))
}

/// The RustFS user the parent holds for a node. Its long-lived key never
/// leaves the parent; the edge only ever sees sessions assumed from it, so
/// disabling this user ends every session (S0: next request refused).
pub fn principal_for(node_id: &str) -> String {
    format!("node-{node_id}")
}

const OBJECT_ACTIONS: &[&str] = &[
    "s3:GetObject",
    "s3:PutObject",
    "s3:DeleteObject",
    "s3:AbortMultipartUpload",
    "s3:ListMultipartUploadParts",
];

/// Session policy for a whole-bucket STS lease: object actions on
/// `n-<id>/*` and listing on `n-<id>`. Nothing else.
pub fn bucket_session_policy(bucket: &str, access: Access) -> String {
    let object_actions: Vec<&str> = match access {
        Access::Write => OBJECT_ACTIONS.to_vec(),
        Access::Read => vec!["s3:GetObject"],
    };
    json!({
        "Version": "2012-10-17",
        "Statement": [
            {"Effect": "Allow", "Action": object_actions, "Resource": [format!("arn:aws:s3:::{bucket}/*")]},
            {"Effect": "Allow", "Action": ["s3:ListBucket", "s3:GetBucketLocation"], "Resource": [format!("arn:aws:s3:::{bucket}")]},
        ]
    })
    .to_string()
}

/// Session policy for a presigned URL: one action on one object.
pub fn object_session_policy(bucket: &str, key: &str, action: &str) -> String {
    json!({
        "Version": "2012-10-17",
        "Statement": [
            {"Effect": "Allow", "Action": [action], "Resource": [format!("arn:aws:s3:::{bucket}/{key}")]},
        ]
    })
    .to_string()
}
