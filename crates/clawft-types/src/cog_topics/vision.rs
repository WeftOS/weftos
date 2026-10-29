//! `vision.v1`: the request body of a service call on `svc.vision.<task>/<node>`
//! (ADR-125 sections 2-4). The reply is [`VisionReply`](super::VisionReply) in
//! `vision_reply.rs`. Like `infer.v1` it carries no envelope: the broker
//! stamps the caller, the call id and the clock on the call.
//!
//! Fields that belong to another task are refused, so a caller that misspells
//! the task does not get a silently different call.

use serde::{Deserialize, Serialize};

use super::blob::BlobHandle;
use super::{Check, fail, in_range};

/// Longest inline image, base64 characters. Bigger images are blobs.
pub const VISION_MAX_IMAGE_B64: usize = 40 * 1024;
/// Largest image the service reads from a blob, bytes (32 MiB).
pub const VISION_MAX_IMAGE_BLOB: u64 = 32 * 1024 * 1024;
/// Largest request or reply body as JSON, bytes. Enforced by the broker.
pub const VISION_MAX_BODY: usize = 64 * 1024;
/// Most results of any kind in a request's `max_results` or a reply array.
pub const VISION_MAX_RESULTS: usize = 256;
/// Most OCR lines in a reply.
pub const VISION_MAX_OCR_LINES: usize = 1024;
/// Most `languages` in an OCR request.
pub const VISION_MAX_LANGUAGES: usize = 8;
/// Longest language tag, chars.
pub const VISION_MAX_LANGUAGE_TAG: usize = 16;
/// Most entries in a request's `labels`.
pub const VISION_MAX_REQUEST_LABELS: usize = 64;
/// Longest label (request or reply), chars.
pub const VISION_MAX_LABEL: usize = 64;
/// Longest `phrase` or `prompt`, chars.
pub const VISION_MAX_PHRASE: usize = 256;
/// Longest OCR line text, chars.
pub const VISION_MAX_OCR_TEXT: usize = 1024;
/// Longest code payload, chars.
pub const VISION_MAX_CODE_PAYLOAD: usize = 4096;
/// Longest code symbology, chars.
pub const VISION_MAX_SYMBOLOGY: usize = 32;
/// Longest `raw` model text in a point reply, chars.
pub const VISION_MAX_RAW: usize = 4096;
/// Longest `taxonomy`, chars.
pub const VISION_MAX_TAXONOMY: usize = 64;
/// Longest provider `model` or `version`, chars.
pub const VISION_MAX_PROVIDER_FIELD: usize = 128;
/// Largest decoded image a provider will read, in pixels (64 megapixels).
/// Providers check the header against this before decoding.
pub const VISION_MAX_PIXELS: u64 = 64_000_000;
/// Largest decoded image side a provider will read, pixels.
pub const VISION_MAX_SIDE: u32 = 16384;
/// Largest mask blob in a segment reply, bytes (64 MiB).
pub const VISION_MAX_MASK_BLOB: u64 = 64 * 1024 * 1024;
/// Largest face `count` in a reply.
pub const VISION_MAX_FACE_COUNT: u32 = 65535;
/// Tolerance for `x + w <= 1` and `y + h <= 1`.
pub const VISION_BBOX_EPSILON: f64 = 1e-9;
/// Media type of an inline f32 little-endian embedding blob.
pub const VISION_VECTOR_MEDIA_TYPE: &str = "application/x-weftos-f32le";
/// Media type of an RLE mask blob.
pub const VISION_RLE_MEDIA_TYPE: &str = "application/x-weftos-rle";

/// Image media types a request may carry.
pub const VISION_IMAGE_TYPES: [&str; 6] = [
    "image/png",
    "image/jpeg",
    "image/webp",
    "image/heic",
    "image/tiff",
    "image/bmp",
];

/// The seven vision tasks; one provider kind each.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VisionTask {
    /// Text lines with boxes.
    Ocr,
    /// Barcodes and QR codes.
    Codes,
    /// Face boxes (no identity).
    Faces,
    /// Classification labels.
    Labels,
    /// Image embedding.
    Embed,
    /// Point at a phrase.
    Point,
    /// Segment by prompt.
    Segment,
}

/// A registered embedding kind (ADR-125 section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingKind {
    /// The kind string carried in requests and replies.
    pub name: &'static str,
    /// Vector length.
    pub dim: u32,
    /// `cosine` or `euclidean`.
    pub distance: &'static str,
}

/// Registered embedding kinds. Adding one is a `vision.v1` change.
pub const EMBEDDING_KINDS: [EmbeddingKind; 2] = [
    EmbeddingKind { name: "apple-featureprint-r2", dim: 768, distance: "euclidean" },
    EmbeddingKind { name: "siglip2-so400m-p16-384", dim: 1152, distance: "cosine" },
];

/// Look up a registered embedding kind.
pub fn embedding_kind(name: &str) -> Option<&'static EmbeddingKind> {
    EMBEDDING_KINDS.iter().find(|k| k.name == name)
}

/// True if `t` is an image type a request may carry.
pub fn valid_image_type(t: &str) -> bool {
    VISION_IMAGE_TYPES.contains(&t)
}

/// Shared check: each `(name, present, owner task, required)` field must be
/// absent unless `task` is its owner, and present if required for `task`.
pub(super) fn check_task_fields(task: VisionTask, fields: &[(&str, bool, VisionTask, bool)]) -> Check {
    for &(name, present, owner, required) in fields {
        if present && owner != task {
            return fail(format!("{name} does not belong to task {task:?}"));
        }
        if !present && owner == task && required {
            return fail(format!("task {task:?} requires {name}"));
        }
    }
    Ok(())
}

/// Char-count check, `1..=max`, with no control characters.
pub(super) fn text_len(name: &str, s: &str, max: usize) -> Check {
    let n = s.chars().count();
    if n == 0 || n > max {
        return fail(format!("{name} must be 1..={max} chars"));
    }
    if s.chars().any(char::is_control) {
        return fail(format!("{name} has a control character"));
    }
    Ok(())
}

/// The whole message as JSON must fit the broker's body cap.
pub(super) fn check_body_size<T: Serialize>(msg: &T) -> Check {
    let n = serde_json::to_vec(msg).map_err(|e| super::CogTopicError(e.to_string()))?.len();
    if n > VISION_MAX_BODY {
        return fail(format!("message is {n} bytes as JSON, more than {VISION_MAX_BODY}"));
    }
    Ok(())
}

/// `vision.v1` request: run `task` on one image. Unknown fields are refused,
/// unlike most schemas (ADR-125 section 2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisionRequest {
    /// Which task; must match the kind called.
    pub task: VisionTask,
    /// The image as a blob. Exactly one of `image` and `image_b64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<BlobHandle>,
    /// A small inline image, base64, at most [`VISION_MAX_IMAGE_B64`] chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_b64: Option<String>,
    /// Media type of `image_b64`; required with it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_mime: Option<String>,
    /// Most results returned, 1..=256.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_results: Option<u32>,
    /// Drop results scored below this, 0..=1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_score: Option<f64>,
    /// `ocr`: BCP-47 tags.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub languages: Option<Vec<String>>,
    /// `labels`: zero-shot labels to score.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<String>>,
    /// `embed`: the registered kind the caller needs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding_kind: Option<String>,
    /// `point`: what to point at; required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrase: Option<String>,
    /// `segment`: what to segment; required.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// `segment`: `png` (default) or `rle`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mask_format: Option<String>,
}

impl VisionRequest {
    /// Check the image, the common caps and the task's own fields.
    pub fn validate(&self) -> Check {
        self.validate_image()?;
        if self.max_results.is_some_and(|n| !(1..=VISION_MAX_RESULTS as u32).contains(&n)) {
            return fail(format!("max_results must be 1..={VISION_MAX_RESULTS}"));
        }
        if let Some(s) = self.min_score {
            in_range("min_score", s, 0.0, 1.0)?;
        }
        use VisionTask as T;
        check_task_fields(
            self.task,
            &[
                ("languages", self.languages.is_some(), T::Ocr, false),
                ("labels", self.labels.is_some(), T::Labels, false),
                ("embedding_kind", self.embedding_kind.is_some(), T::Embed, false),
                ("phrase", self.phrase.is_some(), T::Point, true),
                ("prompt", self.prompt.is_some(), T::Segment, true),
                ("mask_format", self.mask_format.is_some(), T::Segment, false),
            ],
        )?;
        self.validate_task_fields()?;
        check_body_size(self)
    }

    fn validate_image(&self) -> Check {
        match (&self.image, &self.image_b64) {
            (Some(_), Some(_)) => return fail("give an image as image or image_b64, not both"),
            (None, None) => return fail("image or image_b64 is required"),
            _ => {}
        }
        if let Some(b) = &self.image {
            if self.image_mime.is_some() {
                return fail("image_mime goes with image_b64, not image");
            }
            b.validate()?;
            if !valid_image_type(&b.media_type) {
                return fail(format!("image.media_type {:?} is not an allowed image type", b.media_type));
            }
            if b.size > VISION_MAX_IMAGE_BLOB {
                return fail(format!("image is larger than {VISION_MAX_IMAGE_BLOB} bytes"));
            }
        }
        match (&self.image_b64, &self.image_mime) {
            (Some(i), Some(m)) => {
                if i.len() > VISION_MAX_IMAGE_B64 {
                    return fail(format!("image_b64 longer than {VISION_MAX_IMAGE_B64} chars"));
                }
                if super::b64_len(i).is_none_or(|n| n == 0) {
                    return fail("image_b64 must be non-empty standard padded base64");
                }
                if !valid_image_type(m) {
                    return fail(format!("image_mime {m:?} is not an allowed image type"));
                }
            }
            (None, None) => {}
            _ => return fail("image_b64 and image_mime go together"),
        }
        Ok(())
    }

    fn validate_task_fields(&self) -> Check {
        if let Some(l) = &self.languages {
            if l.is_empty() || l.len() > VISION_MAX_LANGUAGES {
                return fail(format!("languages must have 1..={VISION_MAX_LANGUAGES} entries"));
            }
            for t in l {
                text_len("language tag", t, VISION_MAX_LANGUAGE_TAG)?;
                if !t.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
                    return fail(format!("language tag {t:?} must be letters, digits and '-'"));
                }
            }
        }
        if let Some(l) = &self.labels {
            if l.is_empty() || l.len() > VISION_MAX_REQUEST_LABELS {
                return fail(format!("labels must have 1..={VISION_MAX_REQUEST_LABELS} entries"));
            }
            for (i, t) in l.iter().enumerate() {
                text_len("label", t, VISION_MAX_LABEL)?;
                if l[..i].contains(t) {
                    return fail(format!("label {t:?} is listed twice"));
                }
            }
        }
        if let Some(k) = &self.embedding_kind
            && embedding_kind(k).is_none()
        {
            return fail(format!("unregistered embedding_kind {k:?}"));
        }
        if let Some(p) = &self.phrase {
            text_len("phrase", p, VISION_MAX_PHRASE)?;
        }
        if let Some(p) = &self.prompt {
            text_len("prompt", p, VISION_MAX_PHRASE)?;
        }
        if self.mask_format.as_deref().is_some_and(|f| f != "png" && f != "rle") {
            return fail("mask_format must be png or rle");
        }
        Ok(())
    }
}
