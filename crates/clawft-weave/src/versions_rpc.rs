//! `versions.list`, `versions.tree` and `versions.deps` (ADR-121 O1).
//!
//! Read-only queries over the version index under `<runtime>/versions`
//! ([`clawft_versions`]). They read the append-only files and touch nothing
//! else; the files are written by the node's own install path and by
//! `weaver versions import-catalog`, never by an RPC.
//!
//! Params: `kind` (default `agents`) and, for `tree` and `deps`, `name`.
//! A missing `name`, a malformed argument or an object that was never recorded
//! is an error response, never an empty result.

use std::path::Path;

use clawft_rpc::Response;
use clawft_versions::VersionsIndex;
use serde_json::{Value, json};

use crate::rpc_ext::{ExtCall, ExtFuture};

/// Kind queried when the caller names none.
pub const DEFAULT_KIND: &str = "agents";

/// Route handler for the three methods.
pub fn handle(call: ExtCall) -> ExtFuture {
    Box::pin(async move {
        let dir = clawft_rpc::runtime_dir().join("versions");
        let (method, params) = (call.method, call.params);
        let out = tokio::task::spawn_blocking(move || query(&dir, &method, &params)).await;
        match out {
            Ok(Ok(v)) => Response::success(v),
            Ok(Err(e)) => Response::error(e),
            Err(e) => Response::error(format!("versions query failed: {e}")),
        }
    })
}

fn param<'a>(params: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match params.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(format!("{key} must be a string")),
    }
}

/// Run one query against the index at `dir`.
pub fn query(dir: &Path, method: &str, params: &Value) -> Result<Value, String> {
    let kind = param(params, "kind")?.unwrap_or(DEFAULT_KIND);
    let index = VersionsIndex::new(dir);
    let name = || param(params, "name")?.ok_or_else(|| "name is required".to_owned());
    let e = |e: clawft_versions::VersionsError| e.to_string();
    match method {
        "versions.list" => {
            let records = index.list(kind).map_err(e)?;
            Ok(json!({ "kind": kind, "versions": records }))
        }
        "versions.tree" => {
            let name = name()?;
            let tree = index.tree(kind, name).map_err(e)?;
            Ok(json!({ "kind": kind, "object": name, "tree": tree }))
        }
        "versions.deps" => {
            let name = name()?;
            let view = index.deps(kind, name).map_err(e)?;
            Ok(json!({ "kind": kind, "deps": view }))
        }
        other => Err(format!("unknown method {other}")),
    }
}

#[cfg(test)]
#[path = "versions_rpc_tests.rs"]
mod tests;
