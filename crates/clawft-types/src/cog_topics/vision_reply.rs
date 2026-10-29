//! `vision.v1` reply (ADR-125 sections 2 and 4). The reply cannot see the
//! request, so request-dependent rules (a `taxonomy` only when no labels were
//! named, a mask format) are not checked here.
//!
//! Unlike most schemas, unknown fields are refused: a field `v1` does not
//! name could carry identity data about a face (ADR-125 section 6).

use serde::{Deserialize, Serialize};

use super::blob::BlobHandle;
use super::vision::*;
use super::{Check, fail, in_range};

/// The model or framework that answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisionProvider {
    /// Model or framework, 1..=128 chars.
    pub model: String,
    /// Its version, 1..=128 chars.
    pub version: String,
}

/// One OCR line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OcrLine {
    /// The text, 1..=1024 chars.
    pub text: String,
    /// 0..=1.
    pub confidence: f64,
    /// `[x, y, w, h]`, normalised.
    pub bbox: [f64; 4],
}

/// One code found.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodeHit {
    /// `[a-z0-9_-]`, 1..=32 chars.
    pub symbology: String,
    /// Decoded payload, at most 4096 chars.
    pub payload: String,
    /// `[x, y, w, h]`, normalised.
    pub bbox: [f64; 4],
}

/// One face box. No identity, landmarks, attributes or embedding.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FaceBox {
    /// `[x, y, w, h]`, normalised.
    pub bbox: [f64; 4],
    /// 0..=1.
    pub score: f64,
}

/// One scored label.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelScore {
    /// 1..=64 chars.
    pub label: String,
    /// 0..=1.
    pub score: f64,
}

/// One segmentation instance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MaskInstance {
    /// `[x, y, w, h]`, normalised.
    pub bbox: [f64; 4],
    /// 0..=1.
    pub score: f64,
    /// Mask blob at the image size.
    pub mask: BlobHandle,
}

/// The `embedding` object of an `embed` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Embedding {
    /// A registered kind.
    pub kind: String,
    /// Must equal the kind's dimension.
    pub dim: u32,
    /// Must equal the kind's distance.
    pub distance: String,
    /// `dim` finite values, inline. Exactly one of `vector` and `vector_blob`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector: Option<Vec<f64>>,
    /// `application/x-weftos-f32le`, size `dim * 4`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vector_blob: Option<BlobHandle>,
}

/// `vision.v1` reply.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VisionReply {
    /// As the request.
    pub task: VisionTask,
    /// Who answered.
    pub provider: VisionProvider,
    /// Decoded image width, 1..=65535.
    pub width: u32,
    /// Decoded image height, 1..=65535.
    pub height: u32,
    /// Provider time, queueing excluded.
    pub took_ms: u32,
    /// `ocr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<Vec<OcrLine>>,
    /// `codes`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codes: Option<Vec<CodeHit>>,
    /// `faces`: faces found, at least `boxes.len()`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    /// `faces`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub boxes: Option<Vec<FaceBox>>,
    /// `labels`, highest score first.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub labels: Option<Vec<LabelScore>>,
    /// `labels`: the provider's taxonomy, 1..=64 chars.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taxonomy: Option<String>,
    /// `embed`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub embedding: Option<Embedding>,
    /// `point`: the phrase answered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phrase: Option<String>,
    /// `point`: `[x, y]` in 0..=1.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub points: Option<Vec<[f64; 2]>>,
    /// `point`: the model's own text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<String>,
    /// `segment`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instances: Option<Vec<MaskInstance>>,
}

fn check_bbox(b: &[f64; 4]) -> Check {
    for (n, v) in ["x", "y", "w", "h"].iter().zip(b) {
        in_range(&format!("bbox {n}"), *v, 0.0, 1.0)?;
    }
    if b[0] + b[2] > 1.0 + VISION_BBOX_EPSILON || b[1] + b[3] > 1.0 + VISION_BBOX_EPSILON {
        return fail("bbox extends past the image (x + w and y + h must be <= 1)");
    }
    Ok(())
}

fn check_count(name: &str, n: usize, max: usize) -> Check {
    if n > max { fail(format!("{name} has more than {max} entries")) } else { Ok(()) }
}

impl Embedding {
    fn validate(&self) -> Check {
        let Some(k) = embedding_kind(&self.kind) else {
            return fail(format!("unregistered embedding kind {:?}", self.kind));
        };
        if self.dim != k.dim {
            return fail(format!("embedding dim {} is not {} for {}", self.dim, k.dim, k.name));
        }
        if self.distance != k.distance {
            return fail(format!("embedding distance {:?} is not {} for {}", self.distance, k.distance, k.name));
        }
        match (&self.vector, &self.vector_blob) {
            (Some(_), Some(_)) => fail("give the vector as vector or vector_blob, not both"),
            (None, None) => fail("vector or vector_blob is required"),
            (Some(v), None) => {
                if v.len() != k.dim as usize {
                    return fail(format!("vector has {} values, kind needs {}", v.len(), k.dim));
                }
                if v.iter().any(|x| !x.is_finite() || x.abs() > f64::from(f32::MAX)) {
                    return fail("vector values must be finite f32");
                }
                Ok(())
            }
            (None, Some(b)) => {
                b.validate()?;
                if b.media_type != VISION_VECTOR_MEDIA_TYPE {
                    return fail(format!("vector_blob.media_type must be {VISION_VECTOR_MEDIA_TYPE}"));
                }
                if b.size != u64::from(k.dim) * 4 {
                    return fail(format!("vector_blob size must be {}", u64::from(k.dim) * 4));
                }
                Ok(())
            }
        }
    }
}

impl VisionReply {
    /// Check the common fields, then the fields of the reply's task.
    pub fn validate(&self) -> Check {
        text_len("provider.model", &self.provider.model, VISION_MAX_PROVIDER_FIELD)?;
        text_len("provider.version", &self.provider.version, VISION_MAX_PROVIDER_FIELD)?;
        for (n, v) in [("width", self.width), ("height", self.height)] {
            if !(1..=VISION_MAX_SIDE).contains(&v) {
                return fail(format!("{n} must be 1..={VISION_MAX_SIDE}"));
            }
        }
        if u64::from(self.width) * u64::from(self.height) > VISION_MAX_PIXELS {
            return fail(format!("width * height is more than {VISION_MAX_PIXELS} pixels"));
        }
        use VisionTask as T;
        check_task_fields(
            self.task,
            &[
                ("lines", self.lines.is_some(), T::Ocr, true),
                ("codes", self.codes.is_some(), T::Codes, true),
                ("count", self.count.is_some(), T::Faces, true),
                ("boxes", self.boxes.is_some(), T::Faces, true),
                ("labels", self.labels.is_some(), T::Labels, true),
                ("taxonomy", self.taxonomy.is_some(), T::Labels, false),
                ("embedding", self.embedding.is_some(), T::Embed, true),
                ("phrase", self.phrase.is_some(), T::Point, true),
                ("points", self.points.is_some(), T::Point, true),
                ("raw", self.raw.is_some(), T::Point, false),
                ("instances", self.instances.is_some(), T::Segment, true),
            ],
        )?;
        self.validate_results()?;
        check_body_size(self)
    }

    fn validate_results(&self) -> Check {
        for l in self.lines.iter().flatten() {
            text_len("line text", &l.text, VISION_MAX_OCR_TEXT)?;
            in_range("confidence", l.confidence, 0.0, 1.0)?;
            check_bbox(&l.bbox)?;
        }
        check_count("lines", self.lines.as_ref().map_or(0, Vec::len), VISION_MAX_OCR_LINES)?;
        for c in self.codes.iter().flatten() {
            let ok = !c.symbology.is_empty()
                && c.symbology.len() <= VISION_MAX_SYMBOLOGY
                && c.symbology.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
            if !ok {
                return fail(format!("bad symbology {:?}", c.symbology));
            }
            if c.payload.chars().count() > VISION_MAX_CODE_PAYLOAD {
                return fail(format!("payload longer than {VISION_MAX_CODE_PAYLOAD} chars"));
            }
            if c.payload.chars().any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r')) {
                return fail("payload has a control character other than tab or newline");
            }
            check_bbox(&c.bbox)?;
        }
        check_count("codes", self.codes.as_ref().map_or(0, Vec::len), VISION_MAX_RESULTS)?;
        for f in self.boxes.iter().flatten() {
            check_bbox(&f.bbox)?;
            in_range("score", f.score, 0.0, 1.0)?;
        }
        check_count("boxes", self.boxes.as_ref().map_or(0, Vec::len), VISION_MAX_RESULTS)?;
        if let (Some(n), Some(b)) = (self.count, &self.boxes)
            && (n as usize) < b.len()
        {
            return fail("count is less than the number of boxes");
        }
        if self.count.is_some_and(|n| n > VISION_MAX_FACE_COUNT) {
            return fail(format!("count is more than {VISION_MAX_FACE_COUNT}"));
        }
        self.validate_labels_and_rest()
    }

    fn validate_labels_and_rest(&self) -> Check {
        if let Some(ls) = &self.labels {
            check_count("labels", ls.len(), VISION_MAX_RESULTS)?;
            for l in ls {
                text_len("label", &l.label, VISION_MAX_LABEL)?;
                in_range("score", l.score, 0.0, 1.0)?;
            }
            if ls.windows(2).any(|w| w[0].score < w[1].score) {
                return fail("labels must be sorted highest score first");
            }
        }
        if let Some(t) = &self.taxonomy {
            text_len("taxonomy", t, VISION_MAX_TAXONOMY)?;
        }
        if let Some(e) = &self.embedding {
            e.validate()?;
        }
        if let Some(p) = &self.phrase {
            text_len("phrase", p, VISION_MAX_PHRASE)?;
        }
        if let Some(ps) = &self.points {
            check_count("points", ps.len(), VISION_MAX_RESULTS)?;
            for p in ps {
                in_range("point x", p[0], 0.0, 1.0)?;
                in_range("point y", p[1], 0.0, 1.0)?;
            }
        }
        if let Some(r) = &self.raw {
            if r.chars().count() > VISION_MAX_RAW {
                return fail(format!("raw longer than {VISION_MAX_RAW} chars"));
            }
            if r.chars().any(|ch| ch.is_control() && !matches!(ch, '\t' | '\n' | '\r')) {
                return fail("raw has a control character other than tab or newline");
            }
        }
        if let Some(is) = &self.instances {
            check_count("instances", is.len(), VISION_MAX_RESULTS)?;
            for i in is {
                check_bbox(&i.bbox)?;
                in_range("score", i.score, 0.0, 1.0)?;
                i.mask.validate()?;
                if i.mask.media_type != "image/png" && i.mask.media_type != VISION_RLE_MEDIA_TYPE {
                    return fail(format!("mask media_type must be image/png or {VISION_RLE_MEDIA_TYPE}"));
                }
                if i.mask.size > VISION_MAX_MASK_BLOB {
                    return fail(format!("mask is larger than {VISION_MAX_MASK_BLOB} bytes"));
                }
            }
        }
        Ok(())
    }
}
