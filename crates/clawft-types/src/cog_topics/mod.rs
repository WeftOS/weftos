//! Cog topic schemas (Ventuno cog plan 4.2): the registered schema ids a
//! `cog.toml` `[[outputs]]` / `[[inputs]]` declaration may name, and a typed
//! message per schema with a `validate()`.
//!
//! Every message carries the common [`Envelope`] (`source_id`, `node_id`,
//! `t_ns`, `seq`) flattened beside its own fields. Messages are
//! forward-compatible: unknown fields are ignored, so new optional fields stay
//! in `v1`. The golden vectors live in `contracts/cogs/vectors/<schema>/` and
//! are the conformance fixtures for producers and consumers.
//! `spatial.evidence.v1` is registered here but defined by
//! `contracts/sensors` (ADR-111); it has no typed message in this module.

mod blob;
mod data;
mod event;
mod infer;
mod mcu;
mod media;
#[cfg(test)]
mod tests;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub use blob::{BlobHandle, MAX_MEDIA_TYPE, find_handles, valid_media_type};
pub use data::{CanFrame, MAX_STORE_VECTORS, StoreVector, Telemetry, TelemetryItem, Transcript};
pub use event::{EventMsg, MAX_DETAIL, MAX_KIND};
pub use data::{SenmlMsg, SenmlRecord};
pub use mcu::{MAX_METHOD, MAX_PARAMS, McuCall};
pub use infer::{DEFAULT_PRIORITY, InferRequest, MAX_IMAGE_B64, MAX_PRIORITY, MAX_TEXT, MIN_PRIORITY};
pub use media::{Detection, Detections, FrameRef, IqCs16, PcmS16le, SpectrumPsd};

/// Registered schema ids, in the order the plan lists them, then
/// `store.vector.v1` (ADR-122 section 9), `infer.v1` and `mcu.call.v1` (section 12), and
/// `blob.v1` (section 3, B5), then `event.v1` (plan 5.1).
pub const SCHEMA_IDS: [&str; 15] = [
    "senml.v1",
    "spatial.evidence.v1",
    "pcm.s16le.v1",
    "video.frame-ref.v1",
    "detections.v1",
    "can.frame.v1",
    "iq.cs16.v1",
    "spectrum.psd.v1",
    "transcript.v1",
    "telemetry.v1",
    "store.vector.v1",
    "infer.v1",
    "mcu.call.v1",
    "blob.v1",
    "event.v1",
];

/// True if `id` is a registered schema id.
pub fn is_registered(id: &str) -> bool {
    SCHEMA_IDS.contains(&id)
}

/// Longest topic kind accepted.
pub const MAX_TOPIC_LEN: usize = 64;

/// Why a message or declaration was rejected.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct CogTopicError(pub String);

pub(crate) type Check = Result<(), CogTopicError>;

pub(crate) fn fail(msg: impl Into<String>) -> Check {
    Err(CogTopicError(msg.into()))
}

/// Topic kind grammar: `[a-z][a-z0-9-]*(\.[a-z0-9-]+)*`, at most 64 chars.
pub fn valid_topic_kind(s: &str) -> bool {
    if s.is_empty() || s.len() > MAX_TOPIC_LEN {
        return false;
    }
    let mut segs = s.split('.');
    let first = segs.next().unwrap_or_default();
    let seg_ok = |g: &str| {
        !g.is_empty()
            && g.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    };
    first.as_bytes().first().is_some_and(u8::is_ascii_lowercase)
        && seg_ok(first)
        && segs.all(seg_ok)
}

/// Size class of an output (plan 4.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TopicClass {
    /// Up to 4 KB, one broker message.
    Reading,
    /// Sustained ordered bytes.
    Stream,
    /// Written once, passed by content hash.
    Blob,
}

impl TopicClass {
    /// Parse the `class` string of an `[[outputs]]` table.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "reading" => Some(Self::Reading),
            "stream" => Some(Self::Stream),
            "blob" => Some(Self::Blob),
            _ => None,
        }
    }
}

/// What a slow reader does (plan 4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InputPolicy {
    /// Drop the oldest unread message.
    #[default]
    DropOldest,
    /// Backpressure the writer.
    Block,
    /// Keep every n-th message.
    Sample,
}

impl InputPolicy {
    /// Parse the `policy` string of an `[[inputs]]` table.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "drop-oldest" => Some(Self::DropOldest),
            "block" => Some(Self::Block),
            "sample" => Some(Self::Sample),
            _ => None,
        }
    }
}

/// Fields every message carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// The installed sensor; stable if the part moves nodes.
    pub source_id: String,
    /// Node that produced the message: `hex(SHA-256(pubkey)[..16])`.
    pub node_id: String,
    /// Capture clock, nanoseconds.
    pub t_ns: u64,
    /// Per-source counter, increasing by one.
    pub seq: u64,
}

impl Envelope {
    /// Check `source_id` and `node_id` shapes.
    pub fn validate(&self) -> Check {
        let s = self.source_id.as_bytes();
        let ok = !s.is_empty()
            && s.len() <= 64
            && (s[0].is_ascii_lowercase() || s[0].is_ascii_digit())
            && s.iter()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b));
        if !ok {
            return fail(format!("bad source_id {:?}", self.source_id));
        }
        if !is_lower_hex(&self.node_id, 32) {
            return fail("node_id must be 32 lower-case hex chars");
        }
        Ok(())
    }
}

pub(crate) fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Decoded byte length of standard padded base64, or `None` if malformed.
pub(crate) fn b64_len(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    if !b.len().is_multiple_of(4) {
        return None;
    }
    let pad = b.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 {
        return None;
    }
    let body = &b[..b.len() - pad];
    body.iter()
        .all(|c| c.is_ascii_alphanumeric() || *c == b'+' || *c == b'/')
        .then(|| b.len() / 4 * 3 - pad)
}

pub(crate) fn in_range(name: &str, v: f64, lo: f64, hi: f64) -> Check {
    if v.is_finite() && (lo..=hi).contains(&v) {
        Ok(())
    } else {
        fail(format!("{name} must be within {lo}..={hi}"))
    }
}

fn parse<T: DeserializeOwned + Serialize>(
    v: &serde_json::Value,
    validate: impl Fn(&T) -> Check,
) -> Result<serde_json::Value, CogTopicError> {
    let t: T = serde_json::from_value(v.clone()).map_err(|e| CogTopicError(e.to_string()))?;
    validate(&t)?;
    serde_json::to_value(&t).map_err(|e| CogTopicError(e.to_string()))
}

/// Parse `msg` as the typed message of `schema`, validate it, and return its
/// re-serialized form (equal to `msg` for a conforming message).
/// `spatial.evidence.v1` is validated by `weftos-spatial-core`, not here.
pub fn check_message(
    schema: &str,
    msg: &serde_json::Value,
) -> Result<serde_json::Value, CogTopicError> {
    match schema {
        "senml.v1" => parse::<SenmlMsg>(msg, SenmlMsg::validate),
        "pcm.s16le.v1" => parse::<PcmS16le>(msg, PcmS16le::validate),
        "video.frame-ref.v1" => parse::<FrameRef>(msg, FrameRef::validate),
        "detections.v1" => parse::<Detections>(msg, Detections::validate),
        "can.frame.v1" => parse::<CanFrame>(msg, CanFrame::validate),
        "iq.cs16.v1" => parse::<IqCs16>(msg, IqCs16::validate),
        "spectrum.psd.v1" => parse::<SpectrumPsd>(msg, SpectrumPsd::validate),
        "transcript.v1" => parse::<Transcript>(msg, Transcript::validate),
        "telemetry.v1" => parse::<Telemetry>(msg, Telemetry::validate),
        "store.vector.v1" => parse::<StoreVector>(msg, StoreVector::validate),
        "infer.v1" => parse::<InferRequest>(msg, InferRequest::validate),
        "mcu.call.v1" => parse::<McuCall>(msg, McuCall::validate),
        "blob.v1" => parse::<BlobHandle>(msg, BlobHandle::validate),
        "event.v1" => parse::<EventMsg>(msg, EventMsg::validate),
        "spatial.evidence.v1" => Err(CogTopicError(
            "spatial.evidence.v1 is validated by weftos-spatial-core".into(),
        )),
        other => Err(CogTopicError(format!("unregistered schema {other:?}"))),
    }
}
