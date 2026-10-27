//! Blob requests of a session (ADR-122 phase B5): the same checks as every
//! other op (the hello-authenticated token and uid, the manifest, governance,
//! the project rule), then the [`BlobStore`](crate::blob::BlobStore).
//!
//! - **put** (`blob_begin`, `blob_chunk`): the manifest must declare an output
//!   of class `blob` or schema `blob.v1`; governance is asked once, at
//!   `blob_begin` (`cogbus.blob.put`, topic `blob/<media type>`).
//! - **get** (`blob_get`): no declaration is needed (a reader holds a handle
//!   it was given), but the blob must be the instance's own, its project's, or
//!   one handed to it in a call, a reply or a reading; governance is asked on
//!   the first slice (`cogbus.blob.get`, topic `blob/<hash>`).

use base64::Engine;
use base64::engine::general_purpose::STANDARD as B64;
use clawft_types::cog_topics::{BlobHandle, TopicClass, find_handles};
use serde_json::{Value, json};

use super::{Broker, bad};
use crate::audit::now_ms;
use crate::blob::{BlobError, CHUNK};
use crate::policy::{BusAction, BusRequest};
use crate::proto::ErrorCode;
use crate::types::{BusError, Session, refuse};

fn map(e: BlobError) -> BusError {
    let (code, m) = match e {
        BlobError::BadRequest(m) => (ErrorCode::BadRequest, m),
        BlobError::Quota(m) => (ErrorCode::QuotaExceeded, m),
        BlobError::TooLarge(m) => (ErrorCode::TooLarge, m),
        BlobError::NotFound(m) => (ErrorCode::NotFound, m),
        BlobError::Denied(m) => (ErrorCode::CrossProject, m),
        BlobError::Io(m) => (ErrorCode::Unavailable, format!("content store: {m}")),
    };
    bad(code, m)
}

fn handle_json(h: &BlobHandle) -> Value {
    serde_json::to_value(h).unwrap_or(Value::Null)
}

impl Broker {
    fn store(&self, who: &str, action: &str) -> Result<&crate::blob::BlobStore, BusError> {
        self.blobs
            .as_deref()
            .ok_or_else(|| refuse(who, action, "-", bad(ErrorCode::Unsupported, "this node has no content store")))
    }

    /// Hashes of the handles carried anywhere in `body`.
    pub(super) fn handles_in(&self, body: &Value) -> Vec<String> {
        if self.blobs.is_none() {
            return Vec::new();
        }
        find_handles(body).into_iter().map(|h| h.hash).collect()
    }

    /// `from` passes `hashes` to instance `to`.
    pub(super) fn grant_handles(&self, hashes: &[String], from: (Option<&str>, &str), to: (Option<&str>, &str)) {
        if let Some(b) = &self.blobs {
            b.grant(hashes, from, to);
        }
    }

    /// Start an upload.
    pub fn blob_begin(
        &self,
        s: &Session,
        media_type: &str,
        size: u64,
        ttl_ms: Option<u64>,
    ) -> Result<Value, BusError> {
        let me = s.instance_id.as_str();
        let topic = format!("blob/{media_type}");
        let no = |e| refuse(me, "blob.put", &topic, e);
        let store = self.store(me, "blob.put")?;
        let (project, cog) = {
            let g = self.lock();
            let spec = self.spec_of(&g, s, "blob.put")?;
            let outputs = s.declared.as_ref().map_or(&spec.outputs, |d| &d.0);
            if !outputs.iter().any(|o| o.class == TopicClass::Blob || o.schema == "blob.v1") {
                return Err(no(bad(
                    ErrorCode::Undeclared,
                    "no declared output has class blob or schema blob.v1",
                )));
            }
            (spec.project_id.clone(), spec.cog_id.clone())
        };
        let req = BusRequest::new(BusAction::BlobPut, me, &cog, project.as_deref());
        self.ext.gate(&req.on(&topic, "blob.v1"))?;
        let upload = store
            .begin(now_ms(), me, project.as_deref(), media_type, size, ttl_ms)
            .map_err(|e| no(map(e)))?;
        self.ext.noted(s, BusAction::BlobPut, &topic);
        Ok(json!({ "upload": upload, "chunk": CHUNK }))
    }

    /// The next slice of an upload; the handle on the last.
    pub fn blob_chunk(
        &self,
        s: &Session,
        upload: u64,
        offset: u64,
        data: &str,
        last: bool,
    ) -> Result<Value, BusError> {
        let me = s.instance_id.as_str();
        let no = |e| refuse(me, "blob.put", "-", e);
        let store = self.store(me, "blob.put")?;
        if data.len() > CHUNK / 3 * 4 + 8 {
            return Err(no(bad(ErrorCode::TooLarge, format!("a chunk is at most {CHUNK} bytes"))));
        }
        let bytes = B64.decode(data).map_err(|e| no(bad(ErrorCode::BadRequest, format!("data is not base64: {e}"))))?;
        match store.chunk(now_ms(), me, upload, offset, &bytes, last).map_err(|e| no(map(e)))? {
            Some(h) => Ok(json!({ "handle": handle_json(&h) })),
            None => Ok(json!({})),
        }
    }

    /// A slice of a blob.
    pub fn blob_get(
        &self,
        s: &Session,
        hash: &str,
        offset: u64,
        len: Option<u32>,
    ) -> Result<Value, BusError> {
        let me = s.instance_id.as_str();
        let topic = format!("blob/{hash}");
        let no = |e| refuse(me, "blob.get", &topic, e);
        let store = self.store(me, "blob.get")?;
        if offset == 0 {
            let cog = {
                let g = self.lock();
                self.spec_of(&g, s, "blob.get")?.cog_id.clone()
            };
            let req = BusRequest::new(BusAction::BlobGet, me, &cog, s.project_id.as_deref());
            self.ext.gate(&req.on(&topic, "blob.v1"))?;
        }
        let want = len.map_or(CHUNK, |l| l as usize);
        let (h, bytes, eof) = match store.get(now_ms(), me, s.project_id.as_deref(), hash, offset, want) {
            Ok(got) => got,
            // A handle from another node's reading resolves there (B6e).
            Err(BlobError::NotFound(m)) => return Err(self.mesh_blob_miss(s, hash).unwrap_or_else(|| no(bad(ErrorCode::NotFound, m)))),
            Err(e) => return Err(no(map(e))),
        };
        if offset == 0 {
            self.ext.noted(s, BusAction::BlobGet, &topic);
        }
        Ok(json!({ "handle": handle_json(&h), "offset": offset, "data": B64.encode(&bytes), "eof": eof }))
    }

    /// Drop expired blobs; the server calls this on a slow timer. Returns
    /// how many went.
    pub fn sweep_blobs(&self) -> usize {
        self.blobs.as_ref().map_or(0, |b| b.sweep(now_ms()))
    }
}
