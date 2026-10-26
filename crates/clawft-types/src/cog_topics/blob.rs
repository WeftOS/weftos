//! `blob.v1`: a handle to bytes in the node's content store (ADR-122
//! section 3, phase B5). It rides as a reading body, or nested anywhere in a
//! reading, a service call or a service reply, so a payload over the 64 KiB
//! body limit travels as a handle and the receiver fetches it from the broker.

use serde::{Deserialize, Serialize};

use super::{Check, fail, is_lower_hex};

/// Longest media type, bytes.
pub const MAX_MEDIA_TYPE: usize = 127;

/// `blob.v1`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlobHandle {
    /// BLAKE3 of the bytes, lower-case hex.
    pub hash: String,
    /// Length in bytes, at least 1.
    pub size: u64,
    /// `type/subtype`, e.g. `image/png`.
    pub media_type: String,
    /// When the store may drop the bytes (Unix milliseconds).
    pub expires_ms: u64,
}

/// `type/subtype` made of `[a-z0-9][a-z0-9.+-]*` on both sides.
pub fn valid_media_type(s: &str) -> bool {
    let part = |p: &str| {
        let b = p.as_bytes();
        !b.is_empty()
            && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
            && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b".+-".contains(c))
    };
    s.len() <= MAX_MEDIA_TYPE && s.split_once('/').is_some_and(|(t, st)| part(t) && part(st))
}

impl BlobHandle {
    /// Check the hash, size and media type.
    pub fn validate(&self) -> Check {
        if !is_lower_hex(&self.hash, 64) {
            return fail("hash must be 64 lower-case hex chars (blake3)");
        }
        if self.size == 0 {
            return fail("size must be at least 1");
        }
        if !valid_media_type(&self.media_type) {
            return fail(format!("bad media_type {:?}", self.media_type));
        }
        if self.expires_ms == 0 {
            return fail("expires_ms must be set");
        }
        Ok(())
    }
}

/// Every `blob.v1` handle nested in `v` (depth at most 6, at most 32 found).
/// An object counts when all four fields are present and it validates.
pub fn find_handles(v: &serde_json::Value) -> Vec<BlobHandle> {
    fn walk(v: &serde_json::Value, depth: u8, out: &mut Vec<BlobHandle>) {
        if depth > 6 || out.len() >= 32 {
            return;
        }
        match v {
            serde_json::Value::Object(m) => {
                if m.contains_key("hash")
                    && m.contains_key("media_type")
                    && let Ok(h) = serde_json::from_value::<BlobHandle>(v.clone())
                    && h.validate().is_ok()
                {
                    out.push(h);
                    return;
                }
                m.values().for_each(|c| walk(c, depth + 1, out));
            }
            serde_json::Value::Array(a) => a.iter().for_each(|c| walk(c, depth + 1, out)),
            _ => {}
        }
    }
    let mut out = Vec::new();
    walk(v, 0, &mut out);
    out
}
