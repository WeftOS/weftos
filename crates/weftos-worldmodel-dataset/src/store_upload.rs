//! Optional second destination: upload the archived files to an ADR-126 store
//! with `weaver store put`, one object per file.
//!
//! The principal comes only from the request (`--as`); nothing is read from the
//! environment. The step runs after the filesystem copy has verified, so a
//! store failure never loses the plain copy.

use crate::error::DatasetError;
use crate::manifest::MANIFEST_FILE;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::Path;
use std::process::Command;

/// Section name added to the manifest copy.
pub const STORE_UPLOAD_SECTION: &str = "store_upload";
/// Bytes of stderr kept per failed object.
const STDERR_TAIL_BYTES: usize = 400;

/// Where and as whom to upload.
#[derive(Debug, Clone)]
pub struct StoreTarget {
    pub store_name: String,
    /// Object keys are `<key_prefix>/<session>/<file>`; may be empty.
    pub key_prefix: String,
    /// The `--as` value, for example `local:cog:archiver` or `weftos:<id>`.
    pub principal: String,
    /// `weaver` binary to run; `weaver` on PATH when `None`.
    pub weaver_bin: Option<String>,
    /// Print and record the commands without running them.
    pub dry_run: bool,
}

impl StoreTarget {
    pub fn new(
        store_name: impl Into<String>,
        key_prefix: impl Into<String>,
        principal: impl Into<String>,
    ) -> Self {
        Self {
            store_name: store_name.into(),
            key_prefix: key_prefix.into(),
            principal: principal.into(),
            weaver_bin: None,
            dry_run: false,
        }
    }

    fn bin(&self) -> &str {
        self.weaver_bin.as_deref().unwrap_or("weaver")
    }

    fn key(&self, session: &str, file: &str) -> String {
        let prefix = self.key_prefix.trim_matches('/');
        if prefix.is_empty() {
            format!("{session}/{file}")
        } else {
            format!("{prefix}/{session}/{file}")
        }
    }

    /// The argument list after the binary name.
    fn args(&self, key: &str, path: &Path) -> Vec<String> {
        vec![
            "store".into(),
            "put".into(),
            self.store_name.clone(),
            key.into(),
            "--as".into(),
            self.principal.clone(),
            "--file".into(),
            path.display().to_string(),
        ]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ObjectStatus {
    Ok,
    Failed,
    /// Not attempted because an earlier object failed.
    Skipped,
    /// Dry run: the command was recorded, not run.
    DryRun,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ObjectResult {
    pub key: String,
    pub file: String,
    pub status: ObjectStatus,
    /// Last bytes of stderr (or the spawn error) for a failed object.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stderr_tail: Option<String>,
}

/// What the upload did. `objects` holds every archived file in order and, last,
/// the manifest. The copy of this value inside the on-store manifest lists the
/// archived files only, since the manifest cannot record its own upload.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoreUpload {
    pub store_name: String,
    pub key_prefix: String,
    pub principal: String,
    pub dry_run: bool,
    pub ok: bool,
    pub objects: Vec<ObjectResult>,
    /// Full command lines (dry run only).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commands: Vec<Vec<String>>,
}

fn tail(bytes: &[u8]) -> String {
    let start = bytes.len().saturating_sub(STDERR_TAIL_BYTES);
    String::from_utf8_lossy(&bytes[start..]).trim().to_string()
}

fn put(target: &StoreTarget, key: &str, path: &Path) -> Result<(), String> {
    let out = Command::new(target.bin())
        .args(target.args(key, path))
        .output()
        .map_err(|e| format!("cannot run {}: {e}", target.bin()))?;
    if out.status.success() {
        Ok(())
    } else {
        let t = tail(&out.stderr);
        Err(format!(
            "{}: {}",
            out.status,
            if t.is_empty() { "(no stderr)" } else { &t }
        ))
    }
}

/// Upload the archived `files` from `dir`, then the manifest copy last. On a
/// real run the manifest on disk gains a `store_upload` section first; the
/// verified file checksums are unchanged because `manifest.json` is not in
/// its own file list.
pub fn upload(
    target: &StoreTarget,
    session: &str,
    dir: &Path,
    files: &[String],
) -> Result<StoreUpload, DatasetError> {
    for (what, v) in [
        ("store name", &target.store_name),
        ("principal", &target.principal),
    ] {
        if v.trim().is_empty() {
            return Err(DatasetError::VerificationFailed(format!(
                "store upload needs a non-empty {what}"
            )));
        }
    }
    let mut res = StoreUpload {
        store_name: target.store_name.clone(),
        key_prefix: target.key_prefix.clone(),
        principal: target.principal.clone(),
        dry_run: target.dry_run,
        ok: true,
        objects: Vec::new(),
        commands: Vec::new(),
    };
    let names: Vec<&str> = files.iter().map(String::as_str).collect();
    if target.dry_run {
        for name in names.iter().copied().chain([MANIFEST_FILE]) {
            let key = target.key(session, name);
            let mut cmd = vec![target.bin().to_string()];
            cmd.extend(target.args(&key, &dir.join(name)));
            res.commands.push(cmd);
            res.objects.push(ObjectResult {
                key,
                file: name.into(),
                status: ObjectStatus::DryRun,
                stderr_tail: None,
            });
        }
        return Ok(res);
    }
    let mut failed = false;
    for name in &names {
        res.objects
            .push(run_one(target, session, dir, name, &mut failed));
    }
    res.ok = !failed;
    // Record the outcome in the manifest copy, then upload it last.
    write_section(dir, &res)?;
    if failed {
        res.objects.push(ObjectResult {
            key: target.key(session, MANIFEST_FILE),
            file: MANIFEST_FILE.into(),
            status: ObjectStatus::Skipped,
            stderr_tail: None,
        });
        return Ok(res);
    }
    let last = run_one(target, session, dir, MANIFEST_FILE, &mut failed);
    res.objects.push(last);
    res.ok = !failed;
    Ok(res)
}

fn run_one(
    target: &StoreTarget,
    session: &str,
    dir: &Path,
    name: &str,
    failed: &mut bool,
) -> ObjectResult {
    let key = target.key(session, name);
    let (status, stderr_tail) = if *failed {
        (ObjectStatus::Skipped, None)
    } else {
        match put(target, &key, &dir.join(name)) {
            Ok(()) => (ObjectStatus::Ok, None),
            Err(e) => {
                *failed = true;
                (ObjectStatus::Failed, Some(e))
            }
        }
    };
    ObjectResult {
        key,
        file: name.into(),
        status,
        stderr_tail,
    }
}

fn write_section(dir: &Path, res: &StoreUpload) -> Result<(), DatasetError> {
    let p = dir.join(MANIFEST_FILE);
    let bytes = fs::read(&p).map_err(|e| DatasetError::io(&p, e))?;
    let mut v: Value = serde_json::from_slice(&bytes).map_err(|source| DatasetError::Json {
        path: p.clone(),
        source,
    })?;
    v[STORE_UPLOAD_SECTION] = serde_json::to_value(res).expect("upload result serialises");
    let json = serde_json::to_vec_pretty(&v).expect("manifest serialises");
    fs::write(&p, json).map_err(|e| DatasetError::io(&p, e))
}
