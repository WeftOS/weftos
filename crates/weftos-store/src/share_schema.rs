//! Manifest schema 2 and the content-addressed object pool (WO-069 lane D3, board card 6b58589c;
//! design `reports/wo069-content-addressed-keys-proposal-20261115.md`). A child of `share`.
//!
//! Schema 1 stores each file at `.wt/tree/<path>`, the key the previous manifest names, so a publish
//! overwrites bytes a reader of the previous manifest still needs (drill 3a). Schema 2 stores each
//! distinct content once, under an immutable key named by its SHA-256 and a generation:
//!
//! ```text
//! .wt/obj/sha256/<hex[0..2]>/<hex>.g<gen>
//! ```
//!
//! The manifest stays the only map from paths to content. Nothing a manifest names is ever
//! overwritten, and an object leaves the pool only after it has gone unreferenced for a grace
//! period (`share_gc.rs`). The generation is the one deviation from the proposal: a publish PUTs
//! only keys of a generation newer than every manifest it can see, so a PUT can never re-create a
//! key some other publish's sweep is about to delete (see `share_gc.rs` for the argument).
//!
//! On the wire the schema-2 file list is `entries`, not `files`, on purpose: a schema-1 reader
//! (rc.1) then fails with `missing field files` instead of reading `.wt/tree/<path>` objects that
//! are not there. Both shapes decode into the one in-memory [`TreeManifest`].

use super::*;

/// Where schema-2 objects live.
pub const OBJ_PREFIX: &str = ".wt/obj/sha256/";
/// The schema-2 layout id: how a sha and a generation become a key. A reader refuses any other.
pub const LAYOUT: &str = "sha256-fan2-gen";
/// Path-named objects (`.wt/tree/<path>`, `.wt/git/branch.bundle`).
pub const SCHEMA_V1: u32 = 1;
/// Content-addressed objects under [`OBJ_PREFIX`].
pub const SCHEMA_V2: u32 = 2;
/// The newest schema this build reads; a newer one is refused with an upgrade message.
pub const READ_SCHEMA_MAX: u32 = SCHEMA_V2;

/// The pool key of the object with this content and generation. `sha` must be 64 lowercase hex
/// (the parser checks every sha a manifest names before any key is built from it).
pub fn obj_key(sha: &str, g: u64) -> String {
    format!("{OBJ_PREFIX}{}/{sha}.g{g}", &sha[..2])
}

/// `(sha, gen)` of a pool key, `None` for anything not exactly of [`obj_key`]'s form.
pub fn parse_obj_key(key: &str) -> Option<(String, u64)> {
    let rest = key.strip_prefix(OBJ_PREFIX)?;
    let (fan, name) = rest.split_once('/')?;
    let (sha, digits) = name.split_once(".g")?;
    let g: u64 = digits.parse().ok().filter(|g| *g >= 1)?;
    (is_sha_hex(sha) && fan == &sha[..2] && g.to_string() == digits).then(|| (sha.to_string(), g))
}

/// 64 lowercase hex characters.
pub fn is_sha_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One pool object no current manifest names, kept until it has been unreferenced for its grace.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Retiring {
    pub sha256: String,
    #[serde(rename = "gen")]
    pub generation: u64,
    pub size: u64,
    /// When a publish first saw it unreferenced (writer clock). Carried forward unchanged.
    pub since_ms: u64,
    /// A git bundle (shorter grace beyond the newest two, see `share_gc.rs`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub bundle: bool,
}

impl Retiring {
    pub fn key(&self) -> String {
        obj_key(&self.sha256, self.generation)
    }
}

impl TreeManifest {
    /// Schema 2 or later: objects are in the pool.
    pub fn is_pooled(&self) -> bool {
        self.schema >= SCHEMA_V2
    }

    /// The object holding `f`'s bytes under this manifest's schema.
    pub fn object_key(&self, f: &FileEntry) -> String {
        match (self.is_pooled(), f.generation) {
            (true, Some(g)) => obj_key(&f.sha256, g),
            _ => format!("{TREE_PREFIX}{}", f.path),
        }
    }

    /// The object holding the bundle, when the manifest has one.
    pub fn bundle_object(&self) -> Option<String> {
        if !self.has_bundle {
            return None;
        }
        match (self.is_pooled(), self.bundle_sha256.as_deref(), self.bundle_gen) {
            (true, Some(s), Some(g)) => Some(obj_key(s, g)),
            (true, _, _) => None,
            (false, _, _) => Some(BUNDLE_KEY.to_string()),
        }
    }
}

/// What a claim is a claim on, as one digest of the manifest's content: base and head commits, every
/// file (path, size, sha256), the uncommitted list and the bundle. Not `published_ms`, `writer`,
/// `docs` or `changes` (derived, or different on a republish of the same tree), and not the schema,
/// generations or retiring list (where the bytes are stored, not what they are), so the same tree
/// has the same digest stored as schema 1 or 2, and any change to what a merger would fetch changes
/// it. Versioned (`t1:`): a reader that does not know a recorded version refuses the merge.
pub fn tree_digest(m: &TreeManifest) -> String {
    fn field(h: &mut Sha256, s: &[u8]) {
        h.update((s.len() as u64).to_le_bytes());
        h.update(s);
    }
    let mut h = Sha256::new();
    field(&mut h, b"t1");
    field(&mut h, m.base_commit.as_bytes());
    field(&mut h, m.head_commit.as_bytes());
    let mut files: Vec<&FileEntry> = m.files.iter().collect();
    files.sort_by(|a, b| a.path.cmp(&b.path));
    field(&mut h, &(files.len() as u64).to_le_bytes());
    for f in files {
        field(&mut h, f.path.as_bytes());
        field(&mut h, &f.size.to_le_bytes());
        field(&mut h, f.sha256.as_bytes());
    }
    let mut unc: Vec<&str> = m.uncommitted.iter().map(String::as_str).collect();
    unc.sort_unstable();
    field(&mut h, &(unc.len() as u64).to_le_bytes());
    for u in unc {
        field(&mut h, u.as_bytes());
    }
    field(&mut h, &[u8::from(m.has_bundle)]);
    field(&mut h, m.bundle_sha256.as_deref().unwrap_or("").as_bytes());
    let d = h.finalize();
    format!("t1:{}", d.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// The manifest as stored: both schemas' fields, each optional, checked by `TryFrom`.
#[derive(Serialize, Deserialize)]
pub struct Wire {
    schema: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    objects: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    generation: Option<u64>,
    branch: String,
    base_commit: String,
    head_commit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    files: Option<Vec<FileEntry>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    entries: Option<Vec<FileEntry>>,
    uncommitted: Vec<String>,
    #[serde(default)]
    changes: Vec<PathChange>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    changes_recorded: Option<bool>,
    docs: Vec<DocEntry>,
    has_bundle: bool,
    #[serde(default)]
    bundle_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    bundle_gen: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    retiring: Vec<Retiring>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    v1_retired_ms: Option<u64>,
    writer: Actor,
    published_ms: u64,
}

fn bad(m: String) -> String {
    format!("{m}; the manifest is malformed or from a build this one does not understand")
}

impl TryFrom<Wire> for TreeManifest {
    type Error = String;
    fn try_from(w: Wire) -> std::result::Result<Self, String> {
        let pooled = w.schema >= SCHEMA_V2;
        if w.schema > READ_SCHEMA_MAX {
            return Err(format!("manifest is schema {}, newer than the schema {READ_SCHEMA_MAX} this build reads; upgrade to read it", w.schema));
        }
        let generation = w.generation.unwrap_or(0);
        let files = if pooled {
            if w.objects.as_deref() != Some(LAYOUT) {
                return Err(format!(
                    "manifest schema 2 uses object layout {:?}, which this build does not know (it reads {LAYOUT:?}); upgrade to read it",
                    w.objects
                ));
            }
            if w.files.is_some() || generation == 0 {
                return Err(bad("schema 2 manifest with a `files` list or no generation".into()));
            }
            let entries = w.entries.ok_or_else(|| bad("schema 2 manifest without `entries`".into()))?;
            let in_range = |g: Option<u64>| g.is_some_and(|g| (1..=generation).contains(&g));
            if let Some(f) = entries.iter().find(|f| !is_sha_hex(&f.sha256) || !in_range(f.generation)) {
                return Err(bad(format!("entry {} has no valid object (sha256 and generation)", f.path)));
            }
            if w.has_bundle && !(w.bundle_sha256.as_deref().is_some_and(is_sha_hex) && in_range(w.bundle_gen)) {
                return Err(bad("schema 2 manifest says bundle but names no valid bundle object".into()));
            }
            entries
        } else {
            if w.entries.is_some() {
                return Err(bad("schema 1 manifest with an `entries` list".into()));
            }
            w.files.ok_or_else(|| "missing field `files`".to_string())?
        };
        if let Some(r) = w.retiring.iter().find(|r| !is_sha_hex(&r.sha256) || r.generation == 0) {
            return Err(bad(format!("retiring object {:?} is not a pool object", r.sha256)));
        }
        // A schema-1 manifest from before the flag: a non-empty change list was recorded.
        let changes_recorded = w.changes_recorded.unwrap_or(!w.changes.is_empty());
        Ok(TreeManifest {
            schema: w.schema,
            generation,
            branch: w.branch,
            base_commit: w.base_commit,
            head_commit: w.head_commit,
            files,
            uncommitted: w.uncommitted,
            changes: w.changes,
            changes_recorded,
            docs: w.docs,
            has_bundle: w.has_bundle,
            bundle_sha256: w.bundle_sha256,
            bundle_gen: if pooled { w.bundle_gen } else { None },
            retiring: w.retiring,
            v1_retired_ms: w.v1_retired_ms,
            writer: w.writer,
            published_ms: w.published_ms,
        })
    }
}

impl From<TreeManifest> for Wire {
    fn from(m: TreeManifest) -> Self {
        let pooled = m.is_pooled();
        let (files, entries) = if pooled { (None, Some(m.files)) } else { (Some(m.files), None) };
        Wire {
            schema: m.schema,
            objects: pooled.then(|| LAYOUT.to_string()),
            generation: (m.generation > 0).then_some(m.generation),
            branch: m.branch,
            base_commit: m.base_commit,
            head_commit: m.head_commit,
            files,
            entries,
            uncommitted: m.uncommitted,
            changes: m.changes,
            // Schema 2 always says; schema 1 (rc.1 ignores unknown fields) says only when recorded.
            changes_recorded: (pooled || m.changes_recorded).then_some(m.changes_recorded),
            docs: m.docs,
            has_bundle: m.has_bundle,
            bundle_sha256: m.bundle_sha256,
            bundle_gen: if pooled { m.bundle_gen } else { None },
            retiring: m.retiring,
            v1_retired_ms: if pooled { m.v1_retired_ms } else { None },
            writer: m.writer,
            published_ms: m.published_ms,
        }
    }
}

#[cfg(test)]
#[path = "share_schema_tests.rs"]
mod tests;
