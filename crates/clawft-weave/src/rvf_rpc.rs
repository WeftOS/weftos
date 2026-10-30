//! RVF-framed RPC protocol.
//!
//! Maps the existing JSON `Request`/`Response` protocol to RVF Meta (0x07)
//! segments. The payload is JSON-encoded (same as the current line-delimited
//! JSON protocol), but wrapped in RVF segments with content-hash integrity.
//!
//! ## Convention
//!
//! - **Request**  -> Meta segment, no SEALED flag, payload = JSON `Request`.
//! - **Response** -> Meta segment, SEALED flag set, payload = JSON `Response`.
//!
//! The SEALED flag distinguishes responses from requests on the wire.

// Public API used by both daemon (server) and client sides; not all
// functions are called from both code paths in the same binary.
#![allow(dead_code, unused_imports)]

use anyhow::{anyhow, ensure};
use rvf_types::{SegmentFlags, SegmentType};

use crate::protocol::{Request, Response};
use crate::rvf_codec::RvfFrame;

/// The segment type used for all RPC frames.
const RPC_SEG_TYPE: u8 = SegmentType::Meta as u8;

/// Encode an RPC request as an RVF frame's components.
///
/// Returns `(seg_type, payload_bytes, flags, segment_id)` suitable for
/// passing directly to [`RvfFrameWriter::write_frame`].
pub fn encode_request(req: &Request, segment_id: u64) -> (u8, Vec<u8>, SegmentFlags, u64) {
    let payload = serde_json::to_vec(req).expect("Request serialization must not fail");
    (RPC_SEG_TYPE, payload, SegmentFlags::empty(), segment_id)
}

/// Decode an RPC request from a received RVF frame.
///
/// Verifies that the segment type is Meta and the SEALED flag is **not** set.
pub fn decode_request(frame: &RvfFrame) -> Result<Request, anyhow::Error> {
    ensure!(
        frame.header.seg_type == RPC_SEG_TYPE,
        "expected Meta segment (0x{:02X}), got 0x{:02X}",
        RPC_SEG_TYPE,
        frame.header.seg_type
    );
    let flags = SegmentFlags::from_raw(frame.header.flags);
    ensure!(
        !flags.contains(SegmentFlags::SEALED),
        "request frame must not have the SEALED flag"
    );
    serde_json::from_slice(&frame.payload)
        .map_err(|e| anyhow!("failed to deserialize RPC request: {e}"))
}

/// Encode an RPC response as an RVF frame's components.
///
/// Returns `(seg_type, payload_bytes, flags, segment_id)` suitable for
/// passing directly to [`RvfFrameWriter::write_frame`].
pub fn encode_response(resp: &Response, segment_id: u64) -> (u8, Vec<u8>, SegmentFlags, u64) {
    let payload = serde_json::to_vec(resp).expect("Response serialization must not fail");
    (
        RPC_SEG_TYPE,
        payload,
        SegmentFlags::empty().with(SegmentFlags::SEALED),
        segment_id,
    )
}

/// Decode an RPC response from a received RVF frame.
///
/// Verifies that the segment type is Meta and the SEALED flag **is** set.
pub fn decode_response(frame: &RvfFrame) -> Result<Response, anyhow::Error> {
    ensure!(
        frame.header.seg_type == RPC_SEG_TYPE,
        "expected Meta segment (0x{:02X}), got 0x{:02X}",
        RPC_SEG_TYPE,
        frame.header.seg_type
    );
    let flags = SegmentFlags::from_raw(frame.header.flags);
    ensure!(
        flags.contains(SegmentFlags::SEALED),
        "response frame must have the SEALED flag"
    );
    serde_json::from_slice(&frame.payload)
        .map_err(|e| anyhow!("failed to deserialize RPC response: {e}"))
}

/// Check if a frame is an RPC request (Meta type, no SEALED flag).
pub fn is_request(frame: &RvfFrame) -> bool {
    frame.header.seg_type == RPC_SEG_TYPE
        && !SegmentFlags::from_raw(frame.header.flags).contains(SegmentFlags::SEALED)
}

/// Check if a frame is an RPC response (Meta type, SEALED flag set).
pub fn is_response(frame: &RvfFrame) -> bool {
    frame.header.seg_type == RPC_SEG_TYPE
        && SegmentFlags::from_raw(frame.header.flags).contains(SegmentFlags::SEALED)
}

// ── Tests ────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Request, Response};

    /// Build an RvfFrame from the tuple returned by encode_* functions.
    ///
    /// Uses `rvf_wire::write_segment` then `rvf_wire::read_segment` to
    /// produce a realistic frame with a valid content hash.
    fn make_frame(seg_type: u8, payload: &[u8], flags: SegmentFlags, segment_id: u64) -> RvfFrame {
        let segment_bytes = rvf_wire::write_segment(seg_type, payload, flags, segment_id);
        let (header, raw_payload) = rvf_wire::read_segment(&segment_bytes)
            .expect("write_segment output must be valid");
        RvfFrame {
            header,
            payload: raw_payload.to_vec(),
        }
    }

    #[test]
    fn request_encode_decode_roundtrip() {
        let req = Request::with_params("kernel.status", serde_json::json!({"verbose": true}));

        let (seg_type, payload, flags, segment_id) = encode_request(&req, 10);
        let frame = make_frame(seg_type, &payload, flags, segment_id);
        let decoded = decode_request(&frame).expect("decode_request failed");

        assert_eq!(decoded.method, "kernel.status");
        assert_eq!(decoded.params["verbose"], true);
    }

    #[test]
    fn response_encode_decode_roundtrip() {
        let resp = Response::success(serde_json::json!({"state": "running"}))
            .with_id(Some("abc-123".into()));

        let (seg_type, payload, flags, segment_id) = encode_response(&resp, 20);
        let frame = make_frame(seg_type, &payload, flags, segment_id);
        let decoded = decode_response(&frame).expect("decode_response failed");

        assert!(decoded.ok);
        assert_eq!(decoded.result.unwrap()["state"], "running");
        assert_eq!(decoded.id.unwrap(), "abc-123");
    }

    #[test]
    fn request_has_no_sealed_flag() {
        let req = Request::new("agent.spawn");
        let (_seg_type, _payload, flags, _segment_id) = encode_request(&req, 1);
        assert!(!flags.contains(SegmentFlags::SEALED));
    }

    #[test]
    fn response_has_sealed_flag() {
        let resp = Response::success(serde_json::json!(null));
        let (_seg_type, _payload, flags, _segment_id) = encode_response(&resp, 2);
        assert!(flags.contains(SegmentFlags::SEALED));
    }

    #[test]
    fn is_request_is_response() {
        let req = Request::new("test.ping");
        let (st, pl, fl, id) = encode_request(&req, 100);
        let req_frame = make_frame(st, &pl, fl, id);

        let resp = Response::success(serde_json::json!("pong"));
        let (st, pl, fl, id) = encode_response(&resp, 101);
        let resp_frame = make_frame(st, &pl, fl, id);

        assert!(is_request(&req_frame));
        assert!(!is_response(&req_frame));

        assert!(is_response(&resp_frame));
        assert!(!is_request(&resp_frame));
    }
}
