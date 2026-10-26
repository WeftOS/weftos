//! `infer.v1`: the request body of a service call on `svc.infer/<node>`
//! (ADR-122 section 12). Unlike reading schemas it carries no envelope:
//! the broker stamps the caller, the call id and the clock on the call.

use serde::{Deserialize, Serialize};

use super::blob::BlobHandle;
use super::{Check, fail};

/// Lowest priority.
pub const MIN_PRIORITY: u8 = 0;
/// Highest priority.
pub const MAX_PRIORITY: u8 = 9;
/// Priority of a request that names none.
pub const DEFAULT_PRIORITY: u8 = 4;
/// Longest `text` or `system`, in bytes.
pub const MAX_TEXT: usize = 32 * 1024;
/// Longest inline image, base64 characters. Bigger images are blobs (B5).
pub const MAX_IMAGE_B64: usize = 40 * 1024;
/// Largest image the service reads from a blob, bytes.
pub const MAX_IMAGE_BLOB: u64 = 4 * 1024 * 1024;

fn default_priority() -> u8 {
    DEFAULT_PRIORITY
}

/// `infer.v1`: run `model` on the input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InferRequest {
    /// A model id of the service's catalog, 1..=128 chars.
    pub model: String,
    /// 0 (background) ..= 9 (interactive); equal priorities run in arrival order.
    #[serde(default = "default_priority")]
    pub priority: u8,
    /// The prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// System prompt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub system: Option<String>,
    /// One small inline image, base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_b64: Option<String>,
    /// `image/png` or `image/jpeg`; required with `image_b64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_mime: Option<String>,
    /// One image held by the broker as a blob (ADR-122 section 13): a PNG or
    /// JPEG of at most [`MAX_IMAGE_BLOB`] bytes, for a vision model. Not with
    /// `image_b64`. The handle travelling in the call is what lets the service
    /// read the blob.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_blob: Option<BlobHandle>,
    /// Most tokens to generate, 1..=4096.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
}

impl InferRequest {
    /// Check lengths and ranges.
    pub fn validate(&self) -> Check {
        if self.model.is_empty() || self.model.len() > 128 {
            return fail("model must be 1..=128 chars");
        }
        if self.priority > MAX_PRIORITY {
            return fail(format!("priority must be {MIN_PRIORITY}..={MAX_PRIORITY}"));
        }
        if self.text.is_none() && self.image_b64.is_none() && self.image_blob.is_none() {
            return fail("text, image_b64 or image_blob is required");
        }
        if let Some(b) = &self.image_blob {
            if self.image_b64.is_some() {
                return fail("give an image as image_b64 or image_blob, not both");
            }
            b.validate()?;
            if b.media_type != "image/png" && b.media_type != "image/jpeg" {
                return fail("image_blob.media_type must be image/png or image/jpeg");
            }
            if b.size > MAX_IMAGE_BLOB {
                return fail(format!("image_blob is larger than {MAX_IMAGE_BLOB} bytes"));
            }
        }
        for (name, v) in [("text", &self.text), ("system", &self.system)] {
            if v.as_ref().is_some_and(|s| s.len() > MAX_TEXT) {
                return fail(format!("{name} longer than {MAX_TEXT} bytes"));
            }
        }
        match (&self.image_b64, &self.image_mime) {
            (Some(i), Some(m)) => {
                if i.len() > MAX_IMAGE_B64 {
                    return fail(format!("image_b64 longer than {MAX_IMAGE_B64} chars"));
                }
                if m != "image/png" && m != "image/jpeg" {
                    return fail("image_mime must be image/png or image/jpeg");
                }
            }
            (None, None) => {}
            _ => return fail("image_b64 and image_mime go together"),
        }
        if self.max_tokens.is_some_and(|n| !(1..=4096).contains(&n)) {
            return fail("max_tokens must be 1..=4096");
        }
        Ok(())
    }
}
