//! Archive a finished session: validate, build the manifest, copy to a temp
//! directory, verify checksums, then rename into place.

use crate::checksum::sha256_file;
use crate::error::DatasetError;
use crate::evidence::summarize_evidence_file;
use crate::layout::{SessionLayout, validate_session_id};
use crate::manifest::{FileEntry, MANIFEST_FILE, Manifest, ManifestInputs};
use crate::senml::summarize_readings_file;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

/// Project directory under the destination root.
pub const PROJECT_DIR: &str = "ruview-demo";

#[derive(Debug, Clone)]
pub struct ArchiveRequest {
    pub session_dir: PathBuf,
    /// Session id; inferred from the evidence file name when `None`.
    pub session: Option<String>,
    pub dest_root: PathBuf,
    pub layout: SessionLayout,
    /// Explicit consent. Used only when the session has no consent file.
    pub consent: Option<Value>,
    /// Protocol text. Used only when the session has no protocol file.
    pub protocol: Option<String>,
    pub overwrite: bool,
}

impl ArchiveRequest {
    pub fn new(session_dir: impl Into<PathBuf>, dest_root: impl Into<PathBuf>) -> Self {
        Self {
            session_dir: session_dir.into(),
            session: None,
            dest_root: dest_root.into(),
            layout: SessionLayout::default(),
            consent: None,
            protocol: None,
            overwrite: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileCheck {
    pub name: String,
    pub expected_sha256: String,
    pub actual_sha256: Option<String>,
    pub expected_bytes: u64,
    pub actual_bytes: Option<u64>,
    pub ok: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Verification {
    pub ok: bool,
    pub files: Vec<FileCheck>,
}

#[derive(Debug, Clone)]
pub struct ArchiveOutcome {
    pub manifest: Manifest,
    pub dest_dir: PathBuf,
    /// Result of re-reading the copies before they were renamed into place.
    pub verification: Verification,
}

pub fn dataset_dir(dest_root: &Path, session: &str) -> PathBuf {
    dest_root.join(PROJECT_DIR).join("datasets").join(session)
}

fn read_json(path: &Path) -> Result<Value, DatasetError> {
    let bytes = fs::read(path).map_err(|e| DatasetError::io(path, e))?;
    serde_json::from_slice(&bytes).map_err(|source| DatasetError::Json {
        path: path.to_path_buf(),
        source,
    })
}

fn entry(role: &str, path: &Path) -> Result<FileEntry, DatasetError> {
    let (sha256, bytes) = sha256_file(path)?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string();
    Ok(FileEntry {
        role: role.into(),
        name,
        bytes,
        sha256,
    })
}

fn consent_value(
    consent_file: Option<&PathBuf>,
    explicit: Option<&Value>,
) -> Result<Value, DatasetError> {
    let v = match (consent_file, explicit) {
        (Some(p), _) => read_json(p)?,
        (None, Some(v)) => v.clone(),
        (None, None) => return Err(DatasetError::ConsentMissing),
    };
    let empty = match &v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::Array(a) => a.is_empty(),
        _ => false,
    };
    if empty {
        Err(DatasetError::ConsentMissing)
    } else {
        Ok(v)
    }
}

fn protocol_value(
    file: Option<&PathBuf>,
    explicit: Option<&String>,
) -> Result<Option<Value>, DatasetError> {
    match file {
        Some(p) if p.extension().is_some_and(|e| e == "json") => Ok(Some(read_json(p)?)),
        Some(p) => Ok(Some(Value::String(
            fs::read_to_string(p).map_err(|e| DatasetError::io(p, e))?,
        ))),
        None => Ok(explicit.map(|s| Value::String(s.clone()))),
    }
}

/// Archive one finished session. Nothing is published unless every copied
/// file re-reads with the checksum recorded from the source.
pub fn archive_session(req: &ArchiveRequest) -> Result<ArchiveOutcome, DatasetError> {
    let found = req
        .layout
        .discover(&req.session_dir, req.session.as_deref())?;
    validate_session_id(&found.id)?;
    let evidence = summarize_evidence_file(&found.evidence)?;
    let readings = found
        .readings
        .as_deref()
        .map(summarize_readings_file)
        .transpose()?;
    let consent = consent_value(found.consent.as_ref(), req.consent.as_ref())?;
    let protocol = protocol_value(found.protocol.as_ref(), req.protocol.as_ref())?;

    let dest = dataset_dir(&req.dest_root, &found.id);
    if dest.exists() && !req.overwrite {
        return Err(DatasetError::DestinationExists(dest));
    }

    let mut files = vec![entry("evidence", &found.evidence)?];
    for p in &found.csi {
        files.push(entry("csi", p)?);
    }
    for (role, p) in [
        ("readings", &found.readings),
        ("protocol", &found.protocol),
        ("consent", &found.consent),
    ] {
        if let Some(p) = p {
            files.push(entry(role, p)?);
        }
    }
    let sources: Vec<PathBuf> = std::iter::once(found.evidence.clone())
        .chain(found.csi.iter().cloned())
        .chain(
            [&found.readings, &found.protocol, &found.consent]
                .into_iter()
                .flatten()
                .cloned(),
        )
        .collect();

    let manifest = Manifest::build(ManifestInputs {
        session: found.id.clone(),
        layout: req.layout.clone(),
        files,
        evidence,
        readings,
        protocol,
        consent,
    });

    let parent = dest
        .parent()
        .expect("dataset dir has a parent")
        .to_path_buf();
    fs::create_dir_all(&parent).map_err(|e| DatasetError::io(&parent, e))?;
    let tmp = parent.join(format!(".tmp-{}-{}", found.id, std::process::id()));
    let result = stage_and_publish(&tmp, &dest, &sources, &manifest, req.overwrite);
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    let verification = result?;
    Ok(ArchiveOutcome {
        manifest,
        dest_dir: dest,
        verification,
    })
}

fn stage_and_publish(
    tmp: &Path,
    dest: &Path,
    sources: &[PathBuf],
    manifest: &Manifest,
    overwrite: bool,
) -> Result<Verification, DatasetError> {
    if tmp.exists() {
        fs::remove_dir_all(tmp).map_err(|e| DatasetError::io(tmp, e))?;
    }
    fs::create_dir(tmp).map_err(|e| DatasetError::io(tmp, e))?;
    for src in sources {
        let to = tmp.join(src.file_name().expect("source is a file"));
        fs::copy(src, &to).map_err(|e| DatasetError::io(&to, e))?;
    }
    let mpath = tmp.join(MANIFEST_FILE);
    let json = serde_json::to_vec_pretty(manifest).expect("manifest serialises");
    fs::write(&mpath, json).map_err(|e| DatasetError::io(&mpath, e))?;

    let verification = verify_archive(tmp)?;
    if !verification.ok {
        let bad: Vec<&str> = verification
            .files
            .iter()
            .filter(|f| !f.ok)
            .map(|f| f.name.as_str())
            .collect();
        return Err(DatasetError::VerificationFailed(format!(
            "checksum mismatch: {bad:?}"
        )));
    }
    if dest.exists() {
        if !overwrite {
            return Err(DatasetError::DestinationExists(dest.to_path_buf()));
        }
        fs::remove_dir_all(dest).map_err(|e| DatasetError::io(dest, e))?;
    }
    fs::rename(tmp, dest).map_err(|e| DatasetError::io(dest, e))?;
    Ok(verification)
}

/// Re-read an archived dataset directory and compare every file with the
/// size and SHA-256 recorded in its `manifest.json`.
pub fn verify_archive(dir: &Path) -> Result<Verification, DatasetError> {
    let manifest: Manifest = {
        let p = dir.join(MANIFEST_FILE);
        let bytes = fs::read(&p).map_err(|e| DatasetError::io(&p, e))?;
        serde_json::from_slice(&bytes).map_err(|source| DatasetError::Json { path: p, source })?
    };
    let mut files = Vec::new();
    for f in &manifest.files {
        let actual = sha256_file(&dir.join(&f.name)).ok();
        let ok = actual
            .as_ref()
            .is_some_and(|(h, n)| *h == f.sha256 && *n == f.bytes);
        files.push(FileCheck {
            name: f.name.clone(),
            expected_sha256: f.sha256.clone(),
            actual_sha256: actual.as_ref().map(|a| a.0.clone()),
            expected_bytes: f.bytes,
            actual_bytes: actual.map(|a| a.1),
            ok,
        });
    }
    Ok(Verification {
        ok: files.iter().all(|f| f.ok),
        files,
    })
}
