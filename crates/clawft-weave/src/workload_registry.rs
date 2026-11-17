//! Node-local workload catalog behind the `workload.*` RPC family
//! (ADR-099 section 1, card mesh-placement-06).
//!
//! A workload is `(kind, spec, package/manifest, ...)`; it is **not** an
//! `AppManifest` agent. This catalog records the workloads this node has
//! accepted under a governance Permit, keyed by name, with the content
//! hash of their manifest artifact. It is the seam the placement engine
//! and runtime adapters (cards 09 and 12) attach to; it does not fetch,
//! verify (card 07) or run anything.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tracing::warn;

const MAX_NAME: usize = 64;
const MAX_KIND: usize = 64;
const MAX_VERSION: usize = 64;

/// Lifecycle state of a catalogued workload. Only `Installed` is reachable
/// until runtime adapters land (card 09); the enum is non_exhaustive so
/// `Loaded` / `Running` / `Degraded` can be added without a wire break.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkloadState {
    /// Accepted into the node catalog; not loaded or running.
    Installed,
}

/// One catalogued workload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkloadRecord {
    /// Operator-chosen unique name on this node.
    pub name: String,
    /// Open workload kind (`cog`, `inference`, ...).
    pub kind: String,
    /// Content hash of the manifest artifact, `sha256:<hex>` or `blake3:<hex>`.
    pub manifest_hash: String,
    /// Optional version label from the package.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Current lifecycle state.
    pub state: WorkloadState,
    /// Node that holds this record.
    pub node_id: String,
    /// When the install was permitted.
    pub installed_at: DateTime<Utc>,
}

/// Validated `workload.install` parameters.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstallRequest {
    /// Unique name.
    pub name: String,
    /// Workload kind.
    pub kind: String,
    /// Manifest artifact hash.
    pub manifest_hash: String,
    /// Optional version label.
    #[serde(default)]
    pub version: Option<String>,
}

fn valid_name(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c.is_ascii_digit())
        && s.len() <= MAX_NAME
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
}

fn valid_kind(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= MAX_KIND
        && s.split('.').all(|seg| {
            !seg.is_empty()
                && seg
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        })
}

fn valid_hash(s: &str) -> bool {
    let Some((algo, hex)) = s.split_once(':') else {
        return false;
    };
    matches!(algo, "sha256" | "blake3")
        && hex.len() == 64
        && hex.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c))
}

impl InstallRequest {
    /// Boundary validation (card rule: validate input at boundaries).
    pub fn validate(&self) -> Result<(), String> {
        if !valid_name(&self.name) {
            return Err(format!(
                "invalid workload name '{}': 1-{MAX_NAME} chars of a-z 0-9 - _ . starting \
                 alphanumeric",
                self.name
            ));
        }
        if !valid_kind(&self.kind) {
            return Err(format!(
                "invalid workload kind '{}': dotted lowercase segments of a-z 0-9 -",
                self.kind
            ));
        }
        if !valid_hash(&self.manifest_hash) {
            return Err(format!(
                "invalid manifest_hash '{}': expected sha256:<64 hex> or blake3:<64 hex>",
                self.manifest_hash
            ));
        }
        if let Some(v) = &self.version
            && (v.is_empty() || v.len() > MAX_VERSION || v.chars().any(|c| c.is_control() || c.is_whitespace()))
        {
            return Err(format!("invalid version label '{v}'"));
        }
        Ok(())
    }
}

/// Check a bare workload name from an RPC parameter.
pub fn validate_name(name: &str) -> Result<(), String> {
    if valid_name(name) {
        Ok(())
    } else {
        Err(format!("invalid workload name '{name}'"))
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct WorkloadsFile {
    version: u32,
    workloads: Vec<WorkloadRecord>,
}

/// Thread-safe node-local catalog, optionally persisted to JSON.
#[derive(Debug, Default)]
pub struct WorkloadRegistry {
    records: RwLock<BTreeMap<String, WorkloadRecord>>,
    persist_path: Option<PathBuf>,
    /// Serialises persists so the snapshot taken under it is always newer
    /// than the one written before it (no lost update on disk).
    persist_lock: Mutex<()>,
}

impl WorkloadRegistry {
    /// Empty in-memory catalog.
    pub fn new() -> Self {
        Self::default()
    }

    /// Catalog persisted at `path`, rehydrated when the file parses.
    pub fn with_persist_path(path: impl AsRef<Path>) -> Self {
        let path = path.as_ref().to_path_buf();
        let mut map = BTreeMap::new();
        if let Ok(data) = std::fs::read_to_string(&path) {
            match serde_json::from_str::<WorkloadsFile>(&data) {
                Ok(file) => {
                    for rec in file.workloads {
                        map.insert(rec.name.clone(), rec);
                    }
                }
                Err(e) => warn!(error = %e, path = %path.display(), "bad workloads file; starting empty"),
            }
        }
        Self {
            records: RwLock::new(map),
            persist_path: Some(path),
            persist_lock: Mutex::new(()),
        }
    }

    /// All records, sorted by name.
    pub fn list(&self) -> Vec<WorkloadRecord> {
        self.records.read().expect("workload registry poisoned").values().cloned().collect()
    }

    /// One record by name.
    pub fn get(&self, name: &str) -> Option<WorkloadRecord> {
        self.records.read().expect("workload registry poisoned").get(name).cloned()
    }

    /// True when `name` is catalogued.
    pub fn contains(&self, name: &str) -> bool {
        self.records.read().expect("workload registry poisoned").contains_key(name)
    }

    /// Insert a new record; refuses a duplicate name.
    pub fn insert(&self, record: WorkloadRecord) -> Result<(), String> {
        {
            let mut map = self.records.write().expect("workload registry poisoned");
            if map.contains_key(&record.name) {
                return Err(format!("workload '{}' is already installed", record.name));
            }
            map.insert(record.name.clone(), record);
        }
        self.persist();
        Ok(())
    }

    /// Remove a record by name.
    pub fn remove(&self, name: &str) -> Option<WorkloadRecord> {
        let removed = self.records.write().expect("workload registry poisoned").remove(name);
        if removed.is_some() {
            self.persist();
        }
        removed
    }

    fn persist(&self) {
        let Some(path) = &self.persist_path else {
            return;
        };
        // Snapshot only after taking the persist lock: a snapshot taken before
        // it can be stale by the time it is written, and a later mutation's
        // file would then be overwritten by an older one.
        let _guard = self.persist_lock.lock().expect("workload persist lock poisoned");
        let file = WorkloadsFile {
            version: 1,
            workloads: self.list(),
        };
        let json = match serde_json::to_string_pretty(&file) {
            Ok(j) => j,
            Err(e) => return warn!(error = %e, "failed to encode workloads"),
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        // Per-process tmp name (the persist lock serialises writers within this
        // process; the pid keeps a second daemon on the same path off our file).
        let tmp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let write = || -> std::io::Result<()> {
            use std::io::Write as _;
            let mut f = std::fs::File::create(&tmp)?;
            f.write_all(json.as_bytes())?;
            // Data must be durable before the rename makes it the live file.
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, path)
        };
        if let Err(e) = write() {
            warn!(error = %e, path = %path.display(), "failed to persist workloads");
            let _ = std::fs::remove_file(&tmp);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn req(name: &str, kind: &str, hash: &str) -> InstallRequest {
        InstallRequest {
            name: name.into(),
            kind: kind.into(),
            manifest_hash: hash.into(),
            version: None,
        }
    }

    fn record(name: &str) -> WorkloadRecord {
        WorkloadRecord {
            name: name.into(),
            kind: "cog".into(),
            manifest_hash: HASH.into(),
            version: Some("1.0.0".into()),
            state: WorkloadState::Installed,
            node_id: "n-test".into(),
            installed_at: Utc::now(),
        }
    }

    #[test]
    fn validation_accepts_well_formed() {
        assert!(req("anomaly-detect", "cog", HASH).validate().is_ok());
        assert!(req("coder.daily", "inference", HASH).validate().is_ok());
        assert!(req("x", "accelerator-job.v2", &HASH.replace("sha256", "blake3")).validate().is_ok());
    }

    #[test]
    fn validation_rejects_bad_fields() {
        for (n, k, h) in [
            ("", "cog", HASH),
            ("Upper", "cog", HASH),
            ("-lead", "cog", HASH),
            ("../etc", "cog", HASH),
            (&"a".repeat(65), "cog", HASH),
            ("ok", "", HASH),
            ("ok", "Cog", HASH),
            ("ok", "cog..x", HASH),
            ("ok", "cog", "0123"),
            ("ok", "cog", "md5:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
            ("ok", "cog", "sha256:0123456789ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef"),
            ("ok", "cog", "sha256:abc"),
        ] {
            assert!(req(n, k, h).validate().is_err(), "should reject {n:?} {k:?} {h:?}");
        }
        let mut r = req("ok", "cog", HASH);
        r.version = Some("1 0".into());
        assert!(r.validate().is_err());
    }

    #[test]
    fn unknown_install_fields_are_rejected() {
        let v = serde_json::json!({"name":"a","kind":"cog","manifest_hash":HASH,"secret":"x"});
        assert!(serde_json::from_value::<InstallRequest>(v).is_err());
    }

    #[test]
    fn insert_refuses_duplicates_and_remove_works() {
        let r = WorkloadRegistry::new();
        r.insert(record("a")).unwrap();
        assert!(r.insert(record("a")).is_err());
        r.insert(record("b")).unwrap();
        assert_eq!(r.list().iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), ["a", "b"]);
        assert!(r.remove("a").is_some());
        assert!(r.remove("a").is_none());
        assert!(!r.contains("a"));
    }

    #[test]
    fn persists_and_rehydrates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("workloads.json");
        {
            let r = WorkloadRegistry::with_persist_path(&path);
            r.insert(record("keep")).unwrap();
            r.insert(record("drop")).unwrap();
            r.remove("drop");
        }
        let again = WorkloadRegistry::with_persist_path(&path);
        let names: Vec<_> = again.list().into_iter().map(|x| x.name).collect();
        assert_eq!(names, ["keep"]);
        assert_eq!(again.get("keep").unwrap().state, WorkloadState::Installed);
    }

    /// WEFT card 46ea52d3: concurrent mutations must all be on disk afterwards.
    /// Many short rounds, because the lost update needs a narrow interleaving
    /// (snapshot taken, then another mutation persists, then the stale
    /// snapshot is written last).
    #[test]
    fn concurrent_mutations_all_persist() {
        for round in 0..150 {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("workloads.json");
            let reg = std::sync::Arc::new(WorkloadRegistry::with_persist_path(&path));
            let start = std::sync::Arc::new(std::sync::Barrier::new(8));
            let handles: Vec<_> = (0..8)
                .map(|t| {
                    let (reg, start) = (reg.clone(), start.clone());
                    std::thread::spawn(move || {
                        start.wait();
                        for i in 0..4 {
                            reg.insert(record(&format!("w{t}-{i}"))).unwrap();
                        }
                    })
                })
                .collect();
            for h in handles {
                h.join().unwrap();
            }
            let on_disk = WorkloadRegistry::with_persist_path(&path);
            assert_eq!(on_disk.list().len(), 32, "round {round}: disk state lost updates");
            assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1, "stray tmp file");
        }
    }
}
