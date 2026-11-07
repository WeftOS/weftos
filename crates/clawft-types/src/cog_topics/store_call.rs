//! `store.v1`: the request body of a service call on `svc.store/<node>` (ADR-126 S1, ADR-122
//! section 12). It carries no envelope: the broker stamps the caller, and the daemon turns the
//! caller into the store principal `cog:<instance id>`, so a body names no identity.

use serde::{Deserialize, Serialize};

use super::{Check, fail};

/// Longest object key, bytes (S3's limit).
pub const MAX_KEY: usize = 1024;
/// Longest inline `text`, bytes. A call body is capped at 64 KiB by the broker.
pub const MAX_TEXT: usize = 48 * 1024;
/// Longest inline `b64`, characters (36 KiB of data).
pub const MAX_B64: usize = 48 * 1024;

/// `store.v1`: one object operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreCall {
    /// `put`, `get`, `head`, `list` or `delete`.
    pub op: String,
    /// The object key; required by every op except `list`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    /// `put`: the content as UTF-8 text. Exactly one of `text`, `b64`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    /// `put`: the content as base64.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub b64: Option<String>,
    /// `list`: only keys under this prefix.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
    /// `put`: refuse if the key exists.
    #[serde(default, skip_serializing_if = "is_false")]
    pub if_none_match: bool,
    /// The store (`weftos://…`); default: the project store of the cog's project, else the node's.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store: Option<String>,
}

fn is_false(b: &bool) -> bool {
    !*b
}

impl StoreCall {
    /// Check the op against the fields it allows.
    pub fn validate(&self) -> Check {
        let keyed = matches!(self.op.as_str(), "put" | "get" | "head" | "delete");
        if !keyed && self.op != "list" {
            return fail(format!("op {:?}: put, get, head, list or delete", self.op));
        }
        match (&self.key, keyed) {
            (None, true) => return fail(format!("{} needs a key", self.op)),
            (Some(_), false) => return fail("list takes a prefix, not a key"),
            (Some(k), true) if k.is_empty() || k.len() > MAX_KEY => return fail(format!("key must be 1..={MAX_KEY} bytes")),
            _ => {}
        }
        if self.op == "put" {
            match (&self.text, &self.b64) {
                (Some(t), None) if t.len() > MAX_TEXT => return fail(format!("text is over {MAX_TEXT} bytes")),
                (None, Some(b)) if b.len() > MAX_B64 => return fail(format!("b64 is over {MAX_B64} characters")),
                (Some(_), None) | (None, Some(_)) => {}
                _ => return fail("put needs exactly one of text, b64"),
            }
        } else if self.text.is_some() || self.b64.is_some() || self.if_none_match {
            return fail("text, b64 and if_none_match belong to put");
        }
        if self.prefix.is_some() && self.op != "list" {
            return fail("prefix belongs to list");
        }
        Ok(())
    }
}
