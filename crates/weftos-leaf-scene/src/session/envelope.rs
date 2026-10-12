//! The mesh envelopes a leaf wraps its frames in. Built by hand (fixed
//! shape, no JSON library) so the pad firmware and the host emulator
//! produce byte-identical envelopes and the parent decodes one format.

use alloc::string::String;

use super::SessionFrame;
use crate::codec::CodecError;

const TIMESTAMP: &str = "2026-05-14T00:00:00Z";

/// The `mesh.subscribe` control envelope for the leaf's own push topic.
pub fn subscribe_envelope(id: &str, topic: &str) -> String {
    let mut out = String::with_capacity(320);
    out.push_str("{\"source_node\":\"");
    out.push_str(id);
    out.push_str("\",\"dest_node\":\"daemon\",\"message\":{\"id\":\"leaf-sub-1\",\"from\":0,");
    out.push_str("\"target\":{\"Topic\":\"mesh.subscribe\"},\"payload\":{\"Json\":{\"topic\":\"");
    out.push_str(topic);
    out.push_str("\"}},\"timestamp\":\"");
    out.push_str(TIMESTAMP);
    out.push_str("\"},\"hop_count\":0,\"envelope_id\":\"leaf-env-sub-1\"}");
    out
}

/// An `ipc.publish` envelope carrying `message` (a JSON text) to `topic`.
/// `message` is escaped into a JSON string, as the parent expects.
pub fn publish_envelope(id: &str, topic: &str, message: &str) -> String {
    let mut out = String::with_capacity(message.len() + 400);
    out.push_str("{\"source_node\":\"");
    out.push_str(id);
    out.push_str("\",\"dest_node\":\"daemon\",\"message\":{\"id\":\"leaf-input\",\"from\":0,");
    out.push_str("\"target\":{\"Topic\":\"ipc.publish\"},\"payload\":{\"Json\":{\"topic\":\"");
    out.push_str(topic);
    out.push_str("\",\"message\":\"");
    for c in message.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
    out.push_str("\"}},\"timestamp\":\"");
    out.push_str(TIMESTAMP);
    out.push_str("\"},\"hop_count\":0,\"envelope_id\":\"leaf-env-input\"}");
    out
}

/// The `.input` topic of a leaf.
pub fn input_topic(id: &str) -> String {
    let mut s = String::from("mesh.leaf.");
    s.push_str(id);
    s.push_str(".input");
    s
}

/// A session frame, ready to sign and journal: `(topic, envelope text)`.
pub fn session_publish(id: &str, frame: &SessionFrame) -> Result<(String, String), CodecError> {
    let topic = input_topic(id);
    let message = frame.to_carrier(id)?;
    let env = publish_envelope(id, &topic, &message);
    Ok((topic, env))
}
