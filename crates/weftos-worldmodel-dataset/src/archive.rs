//! Archive a finished session: validate, build the manifest, copy to a temp
//! directory, verify checksums, then rename into place.

use crate::checksum::sha256_file;
use crate::error::DatasetError;
use crate::evidence::summarize_evidence_file;
use crate::layout::{SessionLayout, validate_session_id};
use crate::manifest::{FileEntry, MANIFEST_FILE, Manifest, ManifestInputs};
use crate::senml::summarize_readings_file;
use crate::store_upload::{StoreTarget, StoreUpload, upload};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

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
    /// Also upload to a store with `weaver store put` after the copy verifies.
    pub store: Option<StoreTarget>,
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
            store: None,
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
    /// True only when every file matches and the manifest counts agree.
    pub ok: bool,
    pub files: Vec<FileCheck>,
    /// Manifest statements that disagree with a recount of the archived
    /// evidence and readings files. Empty when they agree.
    #[serde(default)]
    pub manifest_problems: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct ArchiveOutcome {
    pub manifest: Manifest,
    pub dest_dir: PathBuf,
    /// Result of re-reading the copies before they were renamed into place.
    pub verification: Verification,
    /// Present when the request carried a store target.
    pub store_upload: Option<StoreUpload>,
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
    archive_inner(req, &mut |_| {})
}

/// Size and modification time of a source file, taken before it is read.
#[derive(Debug, Clone)]
struct Snapshot {
    path: PathBuf,
    len: u64,
    modified: Option<SystemTime>,
}

impl Snapshot {
    fn take(path: &Path) -> Result<Self, DatasetError> {
        let md = fs::metadata(path).map_err(|e| DatasetError::io(path, e))?;
        Ok(Self {
            path: path.to_path_buf(),
            len: md.len(),
            modified: md.modified().ok(),
        })
    }

    /// Fail when the file's size or mtime is no longer what was recorded.
    fn ensure_unchanged(&self) -> Result<(), DatasetError> {
        let now =
            Self::take(&self.path).map_err(|_| DatasetError::SourceChanged(self.path.clone()))?;
        if now.len == self.len && now.modified == self.modified {
            Ok(())
        } else {
            Err(DatasetError::SourceChanged(self.path.clone()))
        }
    }
}

/// `between` runs after the hash pass and before the copy pass. Tests use it
/// to change a source file at exactly that point.
fn archive_inner(
    req: &ArchiveRequest,
    between: &mut dyn FnMut(&[PathBuf]),
) -> Result<ArchiveOutcome, DatasetError> {
    let found = req
        .layout
        .discover(&req.session_dir, req.session.as_deref())?;
    validate_session_id(&found.id)?;
    let sources: Vec<PathBuf> = std::iter::once(found.evidence.clone())
        .chain(found.csi.iter().cloned())
        .chain(
            [&found.readings, &found.protocol, &found.consent]
                .into_iter()
                .flatten()
                .cloned(),
        )
        .collect();
    let snapshots = sources
        .iter()
        .map(|p| Snapshot::take(p))
        .collect::<Result<Vec<_>, _>>()?;
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
    between(&sources);

    let manifest = Manifest::build(ManifestInputs {
        session: found.id.clone(),
        layout: req.layout.clone(),
        files,
        evidence,
        readings,
        protocol,
        consent,
        finished: found.finished.is_some(),
    });

    let parent = dest
        .parent()
        .expect("dataset dir has a parent")
        .to_path_buf();
    fs::create_dir_all(&parent).map_err(|e| DatasetError::io(&parent, e))?;
    let tmp = parent.join(format!(".tmp-{}-{}", found.id, std::process::id()));
    let result = stage_and_publish(&tmp, &dest, &sources, &snapshots, &manifest, req.overwrite);
    if result.is_err() {
        let _ = fs::remove_dir_all(&tmp);
    }
    let verification = result?;
    let store_upload = match &req.store {
        Some(t) => {
            let names: Vec<String> = manifest.files.iter().map(|f| f.name.clone()).collect();
            Some(upload(t, &found.id, &dest, &names)?)
        }
        None => None,
    };
    Ok(ArchiveOutcome {
        manifest,
        dest_dir: dest,
        verification,
        store_upload,
    })
}

fn stage_and_publish(
    tmp: &Path,
    dest: &Path,
    sources: &[PathBuf],
    snapshots: &[Snapshot],
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
    // A recorder that is still writing would give a copy that matches no hash.
    for snap in snapshots {
        snap.ensure_unchanged()?;
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
    let manifest_problems = recount_problems(dir, &manifest);
    Ok(Verification {
        ok: files.iter().all(|f| f.ok) && manifest_problems.is_empty(),
        files,
        manifest_problems,
    })
}

/// Recount the archived evidence and readings files and list every manifest
/// statement that disagrees. A copy with matching bytes but an edited
/// manifest fails here.
fn recount_problems(dir: &Path, m: &Manifest) -> Vec<String> {
    let mut problems = Vec::new();
    let file_of = |role: &str| {
        m.files
            .iter()
            .find(|f| f.role == role)
            .map(|f| dir.join(&f.name))
    };
    let Some(path) = file_of("evidence") else {
        return vec!["manifest lists no evidence file".into()];
    };
    match summarize_evidence_file(&path) {
        Err(e) => problems.push(format!("evidence no longer validates: {e}")),
        Ok(s) => {
            let mut check = |what: &str, same: bool| {
                if !same {
                    problems.push(format!("manifest {what} disagrees with the evidence file"));
                }
            };
            check("evidence.lines", m.evidence.lines == s.lines);
            check("evidence.by_type", m.evidence.by_type == s.by_type);
            check(
                "evidence.proof_total",
                m.evidence.proof_total == s.proof_total,
            );
            check(
                "evidence.unknown_types",
                m.evidence.unknown_types == s.unknown_types,
            );
            check("room", m.room == s.region);
            check(
                "time_span.evidence",
                m.time_span.evidence.min_t_ns == s.min_t_ns
                    && m.time_span.evidence.max_t_ns == s.max_t_ns,
            );
            check("shell_measure", m.shell_measure == s.shell_measures);
            check("node_poses", m.node_poses == s.node_poses);
        }
    }
    match (file_of("readings"), &m.readings) {
        (None, None) => {}
        (None, Some(_)) => {
            problems.push("manifest has readings counts but no readings file".into())
        }
        (Some(_), None) => problems.push("manifest lists a readings file but no counts".into()),
        (Some(p), Some(stats)) => match summarize_readings_file(&p) {
            Err(e) => problems.push(format!("readings no longer validate: {e}")),
            Ok(r) => {
                if stats.packs != r.packs || stats.records != r.records || stats.proof != r.proof {
                    problems
                        .push("manifest readings counts disagree with the readings file".into());
                }
                let span = m
                    .time_span
                    .readings
                    .as_ref()
                    .map(|s| (s.min_t_ns, s.max_t_ns));
                if span != r.min_t_ns.zip(r.max_t_ns) {
                    problems.push(
                        "manifest time_span.readings disagrees with the readings file".into(),
                    );
                }
            }
        },
    }
    problems
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn session() -> (tempfile::TempDir, ArchiveRequest, tempfile::TempDir) {
        let src = tempfile::tempdir().unwrap();
        let fixtures = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/session");
        for e in fs::read_dir(fixtures).unwrap() {
            let e = e.unwrap();
            fs::copy(e.path(), src.path().join(e.file_name())).unwrap();
        }
        let dest = tempfile::tempdir().unwrap();
        let mut req = ArchiveRequest::new(src.path(), dest.path());
        req.consent = Some(serde_json::json!({"given": true}));
        (src, req, dest)
    }

    fn csi(src: &Path) -> PathBuf {
        src.join("demo-session-001.csi.bin")
    }

    #[test]
    fn source_growing_between_hash_and_copy_is_refused() {
        let (src, req, dest) = session();
        let mut grow = |_: &[PathBuf]| {
            let mut f = fs::OpenOptions::new()
                .append(true)
                .open(csi(src.path()))
                .unwrap();
            f.write_all(b"more").unwrap();
        };
        match archive_inner(&req, &mut grow) {
            Err(DatasetError::SourceChanged(p)) => assert_eq!(p, csi(src.path())),
            other => panic!("expected SourceChanged: {other:?}"),
        }
        // Nothing published, no temp directory left.
        let datasets = dest.path().join("ruview-demo/datasets");
        assert!(!datasets.join("demo-session-001").exists());
        assert_eq!(fs::read_dir(datasets).unwrap().count(), 0);
    }

    #[test]
    fn mtime_only_change_is_refused() {
        let (src, req, _dest) = session();
        let mut touch = |_: &[PathBuf]| {
            let f = fs::OpenOptions::new()
                .write(true)
                .open(csi(src.path()))
                .unwrap();
            f.set_modified(SystemTime::now() + std::time::Duration::from_secs(5))
                .unwrap();
        };
        assert!(matches!(
            archive_inner(&req, &mut touch),
            Err(DatasetError::SourceChanged(_))
        ));
    }

    #[test]
    fn untouched_sources_pass_the_stability_check() {
        let (_src, req, _dest) = session();
        assert!(archive_inner(&req, &mut |_| {}).unwrap().verification.ok);
    }
}
