//! The chief of staff's installation brain (ADR-121 decision 9): an RVF store
//! (the repo's `clawft_core` RVF backend) bound to this installation, and the
//! metadata every entry carries
//! (`agents/schemas/brain-metadata.schema.json`, `weftos.brain-metadata/1`).
//!
//! Entries are scoped to the installation: nothing here promotes an entry
//! to another project. A note that does not satisfy the schema is refused
//! before anything is written.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use clawft_core::embeddings::hash_embedder::HashEmbedder;
use clawft_core::embeddings::rvf_backend::RvfStore;
use serde_json::{Value, json};

use crate::CosError;
use crate::config::BrainConfig;

pub const SCHEMA_ID: &str = "weftos.brain-metadata/1";
const KINDS: &[&str] = &["decision", "finding", "note", "handoff", "learning"];
const CONFIDENCE: &[&str] = &["low", "medium", "high"];
const AUTHOR: &str = "chief-of-staff";
/// Fields the schema requires; a test keeps this equal to the schema file.
pub const REQUIRED: &[&str] = &["schema", "kind", "summary", "why", "where", "installation", "package", "author", "recorded_at", "scope"];
const ALLOWED: &[&str] = &[
    "schema", "kind", "summary", "why", "where", "installation", "package", "author", "instance", "card", "recorded_at", "supersedes",
    "confidence", "scope",
];

/// What the caller supplies for one entry; the rest of the metadata comes
/// from the brain's binding.
#[derive(Debug, Clone, Default)]
pub struct NewEntry {
    pub kind: String,
    pub summary: String,
    pub why: String,
    pub where_: Vec<String>,
    pub card: Option<String>,
    pub instance: Option<String>,
    pub confidence: Option<String>,
    pub supersedes: Vec<String>,
}

/// What a write returns.
#[derive(Debug, Clone)]
pub struct Recorded {
    pub id: String,
    pub path: PathBuf,
}

pub struct Brain {
    store: Mutex<RvfStore>,
    path: PathBuf,
    installation: String,
    package: String,
    embedder: HashEmbedder,
}

fn is_package_digest(s: &str) -> bool {
    s.strip_prefix("sha256:").is_some_and(|h| h.len() == 64 && h.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()))
}

/// `sha256:` digest of the chief-of-staff package from `agents/catalog.json`.
fn catalog_digest(catalog: &Path) -> Result<String, CosError> {
    let text = std::fs::read_to_string(catalog).map_err(|e| CosError::Config(format!("catalog {}: {e}", catalog.display())))?;
    let v: Value = serde_json::from_str(&text).map_err(|e| CosError::Config(format!("catalog {}: {e}", catalog.display())))?;
    v["agents"]
        .as_array()
        .and_then(|a| a.iter().find(|x| x["id"] == AUTHOR))
        .and_then(|x| x["digest"].as_str())
        .map(str::to_string)
        .ok_or_else(|| CosError::Config(format!("catalog {} has no digest for {AUTHOR}", catalog.display())))
}

fn find_catalog(from: &Path) -> Option<PathBuf> {
    from.ancestors().map(|d| d.join("agents/catalog.json")).find(|p| p.exists())
}

/// A name usable in a file name: the installation id's last path segment.
fn file_stem(installation: &str) -> String {
    let last = installation.rsplit('/').next().unwrap_or(installation);
    let s: String = last.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' }).collect();
    if s.is_empty() { "local".into() } else { s }
}

impl Brain {
    /// Bind the brain from config. Refuses (never an empty brain) when the
    /// package digest cannot be established.
    pub fn open(cfg: &BrainConfig, cwd: &Path) -> Result<Self, CosError> {
        let installation = cfg.installation.clone().unwrap_or_else(|| "local".into());
        let package = match &cfg.package {
            Some(p) => p.clone(),
            None => {
                let catalog = cfg
                    .catalog
                    .clone()
                    .or_else(|| find_catalog(cwd))
                    .ok_or_else(|| CosError::Config("brain: no package digest (set brain.package or brain.catalog; agents/catalog.json not found)".into()))?;
                catalog_digest(&catalog)?
            }
        };
        if !is_package_digest(&package) {
            return Err(CosError::Config(format!("brain.package {package:?} is not a sha256:<64 hex> digest")));
        }
        let path = cfg.path.clone().unwrap_or_else(|| {
            dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join(".weftos/brains/chief-of-staff")
                .join(format!("{}.rvf", file_stem(&installation)))
        });
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| CosError::Config(format!("brain dir {}: {e}", parent.display())))?;
        }
        let store = RvfStore::open(&path).map_err(|e| CosError::Config(format!("brain {}: {e}", path.display())))?;
        Ok(Self { store: Mutex::new(store), path, installation, package, embedder: HashEmbedder::default_dimension() })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn installation(&self) -> &str {
        &self.installation
    }

    pub fn len(&self) -> usize {
        self.store.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The metadata object for `entry`, validated against the schema rules.
    pub fn metadata(&self, entry: &NewEntry, now_secs: u64) -> Result<Value, CosError> {
        let mut m = json!({
            "schema": SCHEMA_ID,
            "kind": entry.kind,
            "summary": entry.summary.trim(),
            "why": entry.why.trim(),
            "where": entry.where_,
            "installation": self.installation,
            "package": self.package,
            "author": AUTHOR,
            "recorded_at": now_secs,
            "scope": "installation",
        });
        if let Some(c) = &entry.card {
            m["card"] = json!(c);
        }
        if let Some(i) = &entry.instance {
            m["instance"] = json!(i);
        }
        if let Some(c) = &entry.confidence {
            m["confidence"] = json!(c);
        }
        if !entry.supersedes.is_empty() {
            m["supersedes"] = json!(entry.supersedes);
        }
        validate(&m)?;
        Ok(m)
    }

    /// Write one entry: metadata validated, text embedded, store persisted.
    pub fn record(&self, entry: &NewEntry) -> Result<Recorded, CosError> {
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let meta = self.metadata(entry, now)?;
        let text = format!("{}\n{}", entry.summary, entry.why);
        let embedding = self.embedder.compute_embedding(&text);
        let mut h = 0xcbf29ce484222325u64;
        for b in text.bytes().chain(now.to_le_bytes()) {
            h = (h ^ u64::from(b)).wrapping_mul(0x100000001b3);
        }
        let id = format!("cos-{}-{now}-{:08x}", entry.kind, h as u32);
        let mut store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.ingest(id.clone(), embedding, meta).map_err(|e| CosError::Config(format!("brain write: {e}")))?;
        store.compact().map_err(|e| CosError::Config(format!("brain persist: {e}")))?;
        Ok(Recorded { id, path: self.path.clone() })
    }

    /// The `k` nearest entries to `text`, as `(id, score, metadata)`.
    pub fn search(&self, text: &str, k: usize) -> Vec<(String, f32, Value)> {
        let q = self.embedder.compute_embedding(text);
        let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
        store.query(&q, k).into_iter().map(|r| (r.id, r.score, r.metadata)).collect()
    }

    /// One stored entry's metadata.
    pub fn get(&self, id: &str) -> Option<Value> {
        self.store.lock().unwrap_or_else(|e| e.into_inner()).get(id).map(|e| e.metadata.clone())
    }
}

fn bad(why: impl std::fmt::Display) -> CosError {
    CosError::Config(format!("brain metadata: {why}"))
}

/// Check `m` against `weftos.brain-metadata/1`: required fields present, no
/// unknown field, enums, lengths, the package digest shape, no absolute
/// machine path in `where`.
pub fn validate(m: &Value) -> Result<(), CosError> {
    let obj = m.as_object().ok_or_else(|| bad("not an object"))?;
    for k in REQUIRED {
        if !obj.contains_key(*k) {
            return Err(bad(format!("missing `{k}`")));
        }
    }
    if let Some(k) = obj.keys().find(|k| !ALLOWED.contains(&k.as_str())) {
        return Err(bad(format!("unknown field `{k}`")));
    }
    if m["schema"] != SCHEMA_ID {
        return Err(bad(format!("schema must be {SCHEMA_ID}")));
    }
    let kind = m["kind"].as_str().unwrap_or("");
    if !KINDS.contains(&kind) {
        return Err(bad(format!("kind {kind:?} is not one of {KINDS:?}")));
    }
    let text = |k: &str, max: usize| -> Result<(), CosError> {
        let s = m[k].as_str().unwrap_or("");
        if s.trim().is_empty() || s.chars().count() > max {
            return Err(bad(format!("`{k}` must be 1..={max} characters")));
        }
        Ok(())
    };
    text("summary", 500)?;
    text("why", 1000)?;
    text("installation", usize::MAX)?;
    text("author", usize::MAX)?;
    let wh = m["where"].as_array().ok_or_else(|| bad("`where` must be a list"))?;
    if wh.is_empty() {
        return Err(bad("`where` needs at least one place"));
    }
    for w in wh {
        let w = w.as_str().unwrap_or("");
        if w.is_empty() || w.chars().count() > 512 {
            return Err(bad("each `where` entry must be 1..=512 characters"));
        }
        if w.starts_with('/') || w.starts_with("~/") || w.chars().nth(1) == Some(':') {
            return Err(bad("`where` must not be an absolute machine path"));
        }
    }
    if !is_package_digest(m["package"].as_str().unwrap_or("")) {
        return Err(bad("`package` must be sha256:<64 hex>"));
    }
    if m["recorded_at"].as_u64().is_none() {
        return Err(bad("`recorded_at` must be unix seconds"));
    }
    if m["scope"] != "installation" && m["scope"] != "promoted" {
        return Err(bad("`scope` must be installation or promoted"));
    }
    if let Some(c) = obj.get("confidence")
        && !c.as_str().is_some_and(|c| CONFIDENCE.contains(&c))
    {
        return Err(bad("`confidence` must be low, medium or high"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:537fbd002578137017a15183931acc31d8e8800500ec4c6fcb72d472bb5942d6";

    fn brain(dir: &Path) -> Brain {
        let cfg = BrainConfig { path: Some(dir.join("b.rvf")), installation: Some("weftos://local/installations/test".into()), package: Some(DIGEST.into()), catalog: None };
        Brain::open(&cfg, dir).unwrap()
    }

    fn entry() -> NewEntry {
        NewEntry { kind: "decision".into(), summary: "Move the agent to PG after C4".into(), why: "PG holds the primary store".into(), where_: vec!["WEFT-901".into()], ..Default::default() }
    }

    #[test]
    fn required_matches_the_schema_file() {
        let schema: Value = serde_json::from_str(include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../agents/schemas/brain-metadata.schema.json"))).unwrap();
        let req: Vec<&str> = schema["required"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(req, REQUIRED);
        let mut props: Vec<&str> = schema["properties"].as_object().unwrap().keys().map(String::as_str).collect();
        props.sort_unstable();
        let mut allowed = ALLOWED.to_vec();
        allowed.sort_unstable();
        assert_eq!(props, allowed);
        let kinds: Vec<&str> = schema["properties"]["kind"]["enum"].as_array().unwrap().iter().map(|v| v.as_str().unwrap()).collect();
        assert_eq!(kinds, KINDS);
    }

    #[test]
    fn a_note_round_trips_with_full_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let b = brain(dir.path());
        let rec = b.record(&entry()).unwrap();
        let meta = b.get(&rec.id).unwrap();
        validate(&meta).unwrap();
        assert_eq!(meta["scope"], "installation");
        assert_eq!(meta["author"], "chief-of-staff");
        assert_eq!(meta["package"], DIGEST);
        let hits = b.search("agent to PG", 1);
        assert_eq!(hits[0].0, rec.id);
    }

    #[test]
    fn the_brain_persists_across_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let id = brain(dir.path()).record(&entry()).unwrap().id;
        let again = brain(dir.path());
        assert_eq!(again.len(), 1);
        assert!(again.get(&id).is_some());
    }

    #[test]
    fn a_bad_entry_is_refused_and_nothing_is_written() {
        let dir = tempfile::tempdir().unwrap();
        let b = brain(dir.path());
        for bad in [
            NewEntry { kind: "gossip".into(), ..entry() },
            NewEntry { why: "".into(), ..entry() },
            NewEntry { where_: vec![], ..entry() },
            NewEntry { where_: vec!["/Users/someone/repo/file.rs".into()], ..entry() },
            NewEntry { confidence: Some("certain".into()), ..entry() },
        ] {
            assert!(b.record(&bad).is_err(), "{bad:?}");
        }
        assert!(b.is_empty());
    }

    #[test]
    fn open_refuses_without_a_package_digest() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = BrainConfig { path: Some(dir.path().join("b.rvf")), package: Some("sha256:nope".into()), ..Default::default() };
        assert!(Brain::open(&cfg, dir.path()).is_err());
        let none = BrainConfig { path: Some(dir.path().join("b.rvf")), ..Default::default() };
        assert!(Brain::open(&none, dir.path()).is_err());
    }

    #[test]
    fn the_digest_comes_from_the_catalog() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("agents")).unwrap();
        std::fs::write(dir.path().join("agents/catalog.json"), json!({"agents": [{"id": "chief-of-staff", "digest": DIGEST}]}).to_string()).unwrap();
        let cfg = BrainConfig { path: Some(dir.path().join("b.rvf")), ..Default::default() };
        let b = Brain::open(&cfg, &dir.path().join("sub")).unwrap();
        assert!(b.metadata(&entry(), 1).unwrap()["package"] == DIGEST);
    }
}
