//! Owner extension to ADR-126 §4 (board card f40828fb): share the worktree *tree* itself, not only
//! its gitignored layer, so unmerged docs and planning stay visible and a different agent, on a
//! different machine, can merge later.
//!
//! Git still lands code. The store carries (a) the working-tree files git can see (tracked plus
//! untracked-and-not-ignored) under `.wt/tree/`, (b) a `git bundle` of the unmerged commits under
//! `.wt/git/branch.bundle`, and (c) a manifest and handoff record. Everything is under the
//! reserved `.wt/` prefix, so it never appears in overlay reads and is never promoted as an asset.
//!
//! This code touches only the filesystem and the backend. Enumerating files and building the
//! bundle is done by `scripts/worktree-store.sh` (git), keeping subprocess use out of Rust.

use crate::overlay::{ObjectBackend, Result, S2Error, validate_key};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub const TREE_PREFIX: &str = ".wt/tree/";
pub const MANIFEST_KEY: &str = ".wt/tree.manifest.json";
pub const BUNDLE_KEY: &str = ".wt/git/branch.bundle";
pub const HANDOFF_KEY: &str = ".wt/handoff.json";
pub const SCHEMA: u32 = 1;
/// A merge claim older than this is considered abandoned.
pub const CLAIM_TTL_MS: u64 = 30 * 60 * 1000;

/// Who wrote or claimed something. Advisory identity, not authority (grants are in RustFS IAM).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub agent: String,
    pub host: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FileEntry {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

/// One path this worktree changed relative to its base. A rename is a delete plus an add.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PathChange {
    pub path: String,
    pub kind: ChangeKind,
}

/// Parse `git diff --name-status -z --no-renames` output (`M\0path\0D\0path\0...`), optionally
/// followed by `A\0path\0` records for untracked files. The result is sorted, one entry per path
/// (a later record for the same path wins). Rename/copy records (`R`, `C`) are refused: the caller
/// must pass `--no-renames`, so a rename can never hide a delete.
pub fn parse_name_status_z(raw: &[u8]) -> Result<Vec<PathChange>> {
    let mut toks = raw.split(|b| *b == 0).filter(|t| !t.is_empty());
    let mut out: BTreeMap<String, ChangeKind> = BTreeMap::new();
    while let Some(st) = toks.next() {
        let kind = match st {
            b"A" => ChangeKind::Added,
            b"M" | b"T" => ChangeKind::Modified,
            b"D" => ChangeKind::Deleted,
            other => {
                return Err(S2Error::Invalid(format!(
                    "unsupported change status {:?} (use --no-renames)",
                    String::from_utf8_lossy(other)
                )));
            }
        };
        let path = toks
            .next()
            .ok_or_else(|| S2Error::Invalid("change record without a path".into()))?;
        let path = std::str::from_utf8(path)
            .map_err(|_| S2Error::Invalid("non-UTF-8 path in change list".into()))?;
        validate_key(path)?;
        out.insert(path.to_string(), kind);
    }
    Ok(out
        .into_iter()
        .map(|(path, kind)| PathChange { path, kind })
        .collect())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DocEntry {
    pub path: String,
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TreeManifest {
    pub schema: u32,
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    pub files: Vec<FileEntry>,
    /// Paths whose working-tree content is not in `head_commit` (the bundle does not carry them).
    pub uncommitted: Vec<String>,
    /// Paths changed relative to `base_commit`, including deletions. Empty for a manifest written
    /// before this field existed, in which case `overlaps` can only compare file content.
    #[serde(default)]
    pub changes: Vec<PathChange>,
    pub docs: Vec<DocEntry>,
    pub has_bundle: bool,
    pub writer: Actor,
    pub published_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Open,
    /// The writer says it is ready for someone to merge.
    Handoff,
    Claimed,
    Merged,
    Archived,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handoff {
    pub state: State,
    pub note: String,
    pub writer: Actor,
    pub merger: Option<Actor>,
    pub updated_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_file: u64,
    pub max_files: usize,
    pub max_total: u64,
    pub max_bundle: u64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file: 16 << 20,
            max_files: 20_000,
            max_total: 512 << 20,
            max_bundle: 256 << 20,
        }
    }
}

/// What the shell hook hands over. `paths` is `git ls-files -co --exclude-standard`.
pub struct TreeSource<'a> {
    pub root: &'a Path,
    pub paths: &'a [String],
    pub uncommitted: &'a [String],
    /// Changes relative to the base (see [`parse_name_status_z`]); may be empty.
    pub changes: &'a [PathChange],
    pub branch: &'a str,
    pub base_commit: &'a str,
    pub head_commit: &'a str,
    pub bundle: Option<&'a Path>,
    pub writer: Actor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    Secret,
    Symlink,
    NotAFile,
    TooLarge,
    BadPath,
    Missing,
    OverBudget,
}

#[derive(Debug, Default)]
pub struct PublishReport {
    pub uploaded: usize,
    pub unchanged: usize,
    pub removed: usize,
    pub skipped: Vec<(String, Skip)>,
    pub bundle_uploaded: bool,
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Files that must never be shared even when git can see them.
pub fn is_secret_path(path: &str) -> bool {
    let name = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let env_template = [".example", ".sample", ".template"]
        .iter()
        .any(|t| name.ends_with(t));
    (!env_template && (name == ".env" || name.starts_with(".env.")))
        || name.ends_with(".env")
        || name == ".envrc"
        || name == ".git-credentials"
        || name == ".pgpass"
        || name == ".htpasswd"
        || name.starts_with("id_ecdsa")
        || name.starts_with("id_dsa")
        || name.ends_with(".keystore")
        || name.ends_with(".jks")
        || name.ends_with(".tfstate")
        || name.ends_with(".tfstate.backup")
        || name.ends_with(".pem")
        || name.ends_with(".key")
        || name.ends_with(".p12")
        || name.ends_with(".pfx")
        || name.starts_with("id_rsa")
        || name.starts_with("id_ed25519")
        || name.starts_with("credentials")
        || name == ".npmrc"
        || name == ".netrc"
        || (name.contains("secret") && !name.ends_with(".md"))
        || name == "board-token"
}

/// PEM/OpenSSH private key material, whatever the file is called.
fn looks_like_private_key(data: &[u8]) -> bool {
    const NEEDLE: &[u8] = b"PRIVATE KEY-----";
    data.len() >= NEEDLE.len() && data.windows(NEEDLE.len()).any(|w| w == NEEDLE)
}

fn is_doc_path(path: &str) -> bool {
    path.starts_with("docs/")
        || path.starts_with(".planning/")
        || path.starts_with("planning/")
        || path.to_ascii_lowercase().ends_with(".md")
}

fn md_title(path: &str, data: &[u8]) -> Option<String> {
    if !path.to_ascii_lowercase().ends_with(".md") {
        return None;
    }
    let head = &data[..data.len().min(8192)];
    String::from_utf8_lossy(head).lines().find_map(|l| {
        l.strip_prefix("# ")
            .map(|t| t.trim().chars().take(160).collect())
    })
}

pub fn read_manifest<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<TreeManifest>> {
    match b.get(bucket, MANIFEST_KEY)? {
        Some(d) => Ok(Some(serde_json::from_slice(&d)?)),
        None => Ok(None),
    }
}

/// Publish (or refresh) the worktree tree into its bucket. Re-runs upload only changed files and
/// remove objects that left the tree. The manifest is written last, so a reader that sees it sees
/// a complete tree.
pub fn publish_tree<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    src: &TreeSource<'_>,
    limits: &Limits,
    now_ms: u64,
) -> Result<PublishReport> {
    if let Some(h) = read_handoff(b, bucket)? {
        if matches!(h.state, State::Merged | State::Archived) {
            return Err(S2Error::Invalid(format!(
                "worktree is already {:?}; refusing to republish",
                h.state
            )));
        }
        guard_live_claim(&h, &src.writer, now_ms)?;
    }
    if let Some(bp) = src.bundle {
        let len = std::fs::metadata(bp)?.len();
        if len > limits.max_bundle {
            return Err(S2Error::Invalid(format!(
                "bundle is {len} bytes, over the {} limit",
                limits.max_bundle
            )));
        }
    }
    let prev = read_manifest(b, bucket)?;
    let prev_sha: std::collections::HashMap<&str, &str> = prev
        .as_ref()
        .map(|m| {
            m.files
                .iter()
                .map(|f| (f.path.as_str(), f.sha256.as_str()))
                .collect()
        })
        .unwrap_or_default();
    let mut rep = PublishReport::default();
    let (mut files, mut docs, mut total) = (Vec::new(), Vec::new(), 0u64);
    let mut seen = std::collections::BTreeSet::new();

    for p in src.paths {
        if validate_key(p).is_err() || p.starts_with(".wt/") || !seen.insert(p.clone()) {
            rep.skipped.push((p.clone(), Skip::BadPath));
            continue;
        }
        if is_secret_path(p) {
            rep.skipped.push((p.clone(), Skip::Secret));
            continue;
        }
        if is_dangerous_path(p) {
            rep.skipped.push((p.clone(), Skip::BadPath));
            continue;
        }
        let full = match safe_join(src.root, p) {
            Ok(f) => f,
            Err(_) => {
                rep.skipped.push((p.clone(), Skip::Symlink));
                continue;
            }
        };
        let md = match std::fs::symlink_metadata(&full) {
            Ok(m) => m,
            Err(_) => {
                rep.skipped.push((p.clone(), Skip::Missing));
                continue;
            }
        };
        if md.file_type().is_symlink() {
            rep.skipped.push((p.clone(), Skip::Symlink));
            continue;
        }
        if !md.is_file() {
            rep.skipped.push((p.clone(), Skip::NotAFile));
            continue;
        }
        if md.len() > limits.max_file {
            rep.skipped.push((p.clone(), Skip::TooLarge));
            continue;
        }
        if files.len() >= limits.max_files || total + md.len() > limits.max_total {
            rep.skipped.push((p.clone(), Skip::OverBudget));
            continue;
        }
        let data = std::fs::read(&full)?;
        if looks_like_private_key(&data) {
            rep.skipped.push((p.clone(), Skip::Secret));
            continue;
        }
        let sha = sha256_hex(&data);
        let key = format!("{TREE_PREFIX}{p}");
        if prev_sha.get(p.as_str()) == Some(&sha.as_str()) && b.head(bucket, &key)?.is_some() {
            rep.unchanged += 1;
        } else {
            b.put(bucket, &key, &data)?;
            rep.uploaded += 1;
        }
        total += data.len() as u64;
        if is_doc_path(p) {
            docs.push(DocEntry {
                path: p.clone(),
                title: md_title(p, &data),
            });
        }
        files.push(FileEntry {
            path: p.clone(),
            size: data.len() as u64,
            sha256: sha,
        });
    }

    let mut has_bundle = prev
        .as_ref()
        .map(|m| m.has_bundle && m.head_commit == src.head_commit)
        .unwrap_or(false);
    if let Some(bp) = src.bundle {
        if !has_bundle {
            let md = std::fs::metadata(bp)?;
            if md.len() > limits.max_bundle {
                return Err(S2Error::Invalid(format!(
                    "bundle is {} bytes, over the {} limit",
                    md.len(),
                    limits.max_bundle
                )));
            }
            b.put(bucket, BUNDLE_KEY, &std::fs::read(bp)?)?;
            rep.bundle_uploaded = true;
            has_bundle = true;
        }
    } else if !has_bundle {
        b.delete(bucket, BUNDLE_KEY)?;
    }

    let uncommitted = src
        .uncommitted
        .iter()
        .filter(|u| files.iter().any(|f| &f.path == *u))
        .cloned()
        .collect();
    let manifest = TreeManifest {
        schema: SCHEMA,
        branch: src.branch.into(),
        base_commit: src.base_commit.into(),
        head_commit: src.head_commit.into(),
        files,
        uncommitted,
        changes: src.changes.to_vec(),
        docs,
        has_bundle,
        writer: src.writer.clone(),
        published_ms: now_ms,
    };
    b.put(bucket, MANIFEST_KEY, &serde_json::to_vec_pretty(&manifest)?)?;
    // Stale objects go after the manifest no longer lists them, so a reader never sees a listed file missing.
    if let Some(pm) = &prev {
        let kept: std::collections::HashSet<&str> =
            manifest.files.iter().map(|f| f.path.as_str()).collect();
        for f in &pm.files {
            if !kept.contains(f.path.as_str()) {
                b.delete(bucket, &format!("{TREE_PREFIX}{}", f.path))?;
                rep.removed += 1;
            }
        }
    }

    if read_handoff(b, bucket)?.is_none() {
        write_handoff(
            b,
            bucket,
            &Handoff {
                state: State::Open,
                note: String::new(),
                writer: src.writer.clone(),
                merger: None,
                updated_ms: now_ms,
            },
        )?;
    }
    Ok(rep)
}

pub fn read_handoff<B: ObjectBackend>(b: &B, bucket: &str) -> Result<Option<Handoff>> {
    match b.get(bucket, HANDOFF_KEY)? {
        Some(d) => Ok(Some(serde_json::from_slice(&d)?)),
        None => Ok(None),
    }
}

fn write_handoff<B: ObjectBackend>(b: &B, bucket: &str, h: &Handoff) -> Result<()> {
    b.put(bucket, HANDOFF_KEY, &serde_json::to_vec_pretty(h)?)?;
    Ok(())
}

/// Refuse changes by anyone but the merger while a live claim exists.
fn guard_live_claim(h: &Handoff, who: &Actor, now_ms: u64) -> Result<()> {
    if h.state == State::Claimed
        && let Some(m) = h.merger.as_ref().filter(|m| *m != who)
        && now_ms.saturating_sub(h.updated_ms) < CLAIM_TTL_MS
    {
        return Err(S2Error::Claimed(format!("{}@{}", m.agent, m.host)));
    }
    Ok(())
}

/// The writer says the tree is ready for another agent to merge.
pub fn mark_handoff<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    writer: &Actor,
    note: &str,
    now_ms: u64,
) -> Result<()> {
    if read_manifest(b, bucket)?.is_none() {
        return Err(S2Error::Invalid(
            "nothing published to hand off; publish the tree first".into(),
        ));
    }
    let cur = read_handoff(b, bucket)?;
    if let Some(h) = &cur {
        if matches!(h.state, State::Merged | State::Archived) {
            return Err(S2Error::Invalid(format!(
                "worktree is already {:?}",
                h.state
            )));
        }
        guard_live_claim(h, writer, now_ms)?;
    }
    write_handoff(
        b,
        bucket,
        &Handoff {
            state: State::Handoff,
            note: note.into(),
            writer: writer.clone(),
            merger: None,
            updated_ms: now_ms,
        },
    )
}

/// Take the merge. Advisory (the store has no cross-client locking, ADR-126 §4): write, re-read,
/// and lose if someone else's claim is the one that stuck. A claim older than [`CLAIM_TTL_MS`]
/// can be taken over.
pub fn claim_merge<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    now_ms: u64,
) -> Result<Handoff> {
    let cur = read_handoff(b, bucket)?
        .ok_or_else(|| S2Error::Invalid("worktree has no handoff record".into()))?;
    match cur.state {
        State::Merged | State::Archived => {
            return Err(S2Error::Invalid(format!(
                "worktree is already {:?}",
                cur.state
            )));
        }
        State::Claimed => {
            let other = cur.merger.as_ref().filter(|m| *m != me);
            if let Some(m) = other
                && now_ms.saturating_sub(cur.updated_ms) < CLAIM_TTL_MS
            {
                return Err(S2Error::Claimed(format!("{}@{}", m.agent, m.host)));
            }
        }
        State::Open | State::Handoff => {}
    }
    let next = Handoff {
        state: State::Claimed,
        merger: Some(me.clone()),
        updated_ms: now_ms,
        ..cur
    };
    write_handoff(b, bucket, &next)?;
    let back = read_handoff(b, bucket)?
        .ok_or_else(|| S2Error::Backend("handoff vanished after write".into()))?;
    match &back.merger {
        Some(m) if m == me => Ok(back),
        Some(m) => Err(S2Error::Claimed(format!("{}@{}", m.agent, m.host))),
        None => Err(S2Error::Backend("claim not recorded".into())),
    }
}

/// The merger finished: git has landed the branch. Promotion and bucket cleanup follow (§3, §4).
pub fn mark_merged<B: ObjectBackend>(
    b: &B,
    bucket: &str,
    me: &Actor,
    note: &str,
    now_ms: u64,
) -> Result<()> {
    let cur = read_handoff(b, bucket)?
        .ok_or_else(|| S2Error::Invalid("worktree has no handoff record".into()))?;
    if cur.state != State::Claimed {
        return Err(S2Error::Invalid(format!(
            "claim the merge before marking it merged (state is {:?})",
            cur.state
        )));
    }
    if cur.merger.as_ref() != Some(me) {
        return Err(S2Error::Claimed(
            cur.merger
                .map(|m| format!("{}@{}", m.agent, m.host))
                .unwrap_or_default(),
        ));
    }
    write_handoff(
        b,
        bucket,
        &Handoff {
            state: State::Merged,
            note: note.into(),
            merger: Some(me.clone()),
            updated_ms: now_ms,
            ..cur
        },
    )
}

/// Path segments that must never be written by a fetch: VCS control dirs and files that make a
/// later git/shell command in the destination run attacker-chosen code.
pub fn is_dangerous_path(rel: &str) -> bool {
    rel.split('/').any(|seg| {
        let s = seg.to_ascii_lowercase();
        matches!(
            s.as_str(),
            ".git" | ".gitattributes" | ".gitmodules" | ".hg" | ".svn" | ".envrc"
        ) || seg.contains(':')
            || seg.len() > 200
            || is_windows_hostile(seg)
    })
}

/// Names a Windows filesystem treats specially, refused on every platform because a tree fetched
/// here may later be read there (and `.git` aliases like `git~1` would bypass the `.git` check).
fn is_windows_hostile(seg: &str) -> bool {
    if seg.ends_with('.') || seg.ends_with(' ') || seg.contains(['<', '>', '"', '|', '?', '*']) {
        return true;
    }
    if seg.bytes().any(|b| b < 0x20) {
        return true;
    }
    let stem = seg.split('.').next().unwrap_or(seg).to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (stem.len() == 4
        && (stem.starts_with("COM") || stem.starts_with("LPT"))
        && stem.as_bytes()[3].is_ascii_digit()
        && stem.as_bytes()[3] != b'0');
    // 8.3 short-name aliases of the dangerous names (GIT~1, GITATT~1, GITMOD~1).
    let lower = seg.to_ascii_lowercase();
    reserved
        || lower.starts_with("git~")
        || lower.starts_with("gitatt~")
        || lower.starts_with("gitmod~")
}

/// Where a path lands under `dest`, refusing traversal and symlinked parents.
pub fn safe_join(dest: &Path, rel: &str) -> Result<PathBuf> {
    validate_key(rel)?;
    if is_dangerous_path(rel) {
        return Err(S2Error::BadKey(rel.to_string()));
    }
    let mut cur = dest.to_path_buf();
    for seg in rel.split('/') {
        cur.push(seg);
        if let Ok(md) = std::fs::symlink_metadata(&cur)
            && md.file_type().is_symlink()
        {
            return Err(S2Error::BadKey(rel.to_string()));
        }
    }
    Ok(cur)
}
