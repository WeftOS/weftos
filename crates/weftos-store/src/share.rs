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

#[path = "share_handoff.rs"]
mod handoff;
#[path = "share_publish.rs"]
mod publish;
#[path = "share_intent.rs"]
mod intent;
#[path = "share_schema.rs"]
mod schema;
#[path = "share_gc.rs"]
mod gc;
#[path = "share_write.rs"]
mod write;

pub use handoff::{
    claim_merge, claim_merge_with, fence_claim, mark_handoff, mark_landed, mark_landed_at, mark_merged, mark_merged_at, read_handoff,
};
use handoff::{HANDOFF_RETRIES, contended, guard_live_claim, read_handoff_versioned, write_handoff_if};
use intent::{Settled, announce, refuse_pending, restore_base, settle, withdraw};
pub(crate) use publish::{is_doc_path, looks_like_private_key, parse_manifest, read_manifest_bytes, read_manifest_versioned, read_regular};
use publish::{check_manifest_len, md_title};
pub use gc::{DEFAULT_BUNDLE_GRACE_MS, DEFAULT_GRACE_MS, KEPT_BUNDLES, PublishOptions};
pub use write::{publish_tree, publish_tree_listed, publish_tree_opts};
pub use schema::{LAYOUT, OBJ_PREFIX, READ_SCHEMA_MAX, Retiring, SCHEMA_V1, SCHEMA_V2, is_sha_hex, obj_key, parse_obj_key, tree_digest};
pub use publish::{
    MAX_MANIFEST_BYTES, Published, max_retiring, parse_manifest_with, read_manifest, read_manifest_with, summarize_with,
};
#[cfg(test)]
use publish::read_open_regular;

pub const TREE_PREFIX: &str = ".wt/tree/";
pub const MANIFEST_KEY: &str = ".wt/tree.manifest.json";
pub const BUNDLE_KEY: &str = ".wt/git/branch.bundle";
pub const HANDOFF_KEY: &str = ".wt/handoff.json";
/// The schema this build writes by default (a writer can still be asked for schema 1, see
/// `share_publish.rs`).
pub const SCHEMA: u32 = SCHEMA_V2;
/// A merge claim older than this is considered abandoned.
///
/// The age is `now - updated_ms`, where `now` is the clock of the daemon being asked and
/// `updated_ms` was written by the daemon that took the claim: two machines' clocks, not one.
pub const CLAIM_TTL_MS: u64 = 30 * 60 * 1000;
/// A claim stamped further ahead of this daemon's clock than this is not trusted as live (a
/// claimer with a fast clock would otherwise hold the merge far past the TTL), so it can be taken.
pub const CLAIM_SKEW_MS: u64 = 10 * 60 * 1000;

/// Is this claim still live at `now_ms`: younger than the TTL, and not stamped from the future.
fn claim_is_live(h: &Handoff, now_ms: u64) -> bool {
    stamp_is_live(h.updated_ms, now_ms)
}

/// The claim rule for any stamp another daemon wrote: younger than [`CLAIM_TTL_MS`] and not more
/// than [`CLAIM_SKEW_MS`] ahead of this clock. A publish announcement is judged by it too, so a bad
/// clock holds nothing for longer than the TTL plus the skew.
fn stamp_is_live(stamp_ms: u64, now_ms: u64) -> bool {
    stamp_ms <= now_ms.saturating_add(CLAIM_SKEW_MS) && now_ms.saturating_sub(stamp_ms) < CLAIM_TTL_MS
}

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
    /// Schema 2: the generation of the pool object holding these bytes ([`obj_key`]). `None` in a
    /// schema-1 manifest, where the object is `.wt/tree/<path>`.
    #[serde(default, rename = "gen", skip_serializing_if = "Option::is_none")]
    pub generation: Option<u64>,
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

/// A published tree, either schema. Stored through [`schema::Wire`]: schema 1 lists `files` and keys
/// objects by path, schema 2 lists `entries` and keys them by content (see `share_schema.rs`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(try_from = "schema::Wire", into = "schema::Wire")]
pub struct TreeManifest {
    pub schema: u32,
    /// Schema 2: previous generation + 1 (0 when absent, as in a schema-1 manifest from rc.1).
    pub generation: u64,
    pub branch: String,
    pub base_commit: String,
    pub head_commit: String,
    /// The tree's files (`files` on the schema-1 wire, `entries` on schema 2).
    pub files: Vec<FileEntry>,
    /// Paths whose working-tree content is not in `head_commit` (the bundle does not carry them).
    pub uncommitted: Vec<String>,
    /// Paths changed relative to `base_commit`, including deletions.
    pub changes: Vec<PathChange>,
    /// The writer computed `changes`, so an empty list means "changed nothing", not "unknown".
    pub changes_recorded: bool,
    pub docs: Vec<DocEntry>,
    pub has_bundle: bool,
    /// SHA-256 of the bundle object, so a fetch can tell a swapped bundle from the published one.
    pub bundle_sha256: Option<String>,
    /// Schema 2: the bundle object's generation.
    pub bundle_gen: Option<u64>,
    /// Schema 2 pool objects no longer referenced, kept until their grace has passed.
    pub retiring: Vec<Retiring>,
    /// When a schema-2 publish first replaced a schema-1 tree in this bucket: the `.wt/tree/`
    /// objects (and the old bundle key) are swept a grace period after it.
    pub v1_retired_ms: Option<u64>,
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
    /// Registry only, written by the daemon (`orphan::mark_orphan`): the worktree's local path has
    /// been gone past the grace period and it never landed. Never written to `handoff.json`; the
    /// next publish re-derives the entry from the worktree's own bucket, which clears it.
    Orphan,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Handoff {
    pub state: State,
    pub note: String,
    pub writer: Actor,
    pub merger: Option<Actor>,
    pub updated_ms: u64,
    /// Counts claims. A merger that sees a higher epoch than the one it claimed under knows its
    /// claim was taken over (fencing), whatever the clocks say.
    #[serde(default)]
    pub epoch: u64,
    /// The published head (`manifest.head_commit`) when the current claim was taken. A claim is a
    /// claim on that head: a merge is refused once the published head differs (WO-069 6a-1).
    /// `None` outside a claim, and on a record written before the field existed: such a claim cannot be
    /// fenced, so a fenced merge refuses it and the holder claims again (WO-069 lane Y, item 3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_head: Option<String>,
    /// The published tree ([`tree_digest`] of the manifest) when the current claim was taken: a merge
    /// is refused once it differs, so a republish of the same head with other uncommitted content
    /// voids the claim too (WO-069 lane Y, item 2). `None` outside a claim, and on a claim recorded
    /// before the field existed (fenced on head only).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub claimed_tree: Option<String>,
    /// The last publish announced, written with a conditional write just before its manifest PUT
    /// (WO-069 lane Y, A4). A fenced merge or land refuses while it is pending (the manifest is not
    /// yet the announced bytes) and live, so a manifest cannot change between a merger's fence and
    /// its write. `None` on a record from before the field and outside any publish.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub publishing: Option<PublishIntent>,
}

/// One publish's announcement: the manifest bytes it is about to PUT, by digest.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PublishIntent {
    /// sha256 of the exact manifest bytes the publish writes: equal to the stored manifest's once
    /// its PUT happened, which is how every reader tells a finished announcement from a pending one.
    pub manifest_sha256: String,
    pub head: String,
    /// [`tree_digest`] of that manifest.
    pub tree: String,
    /// The publisher's clock when it announced (judged by [`CLAIM_TTL_MS`] / [`CLAIM_SKEW_MS`]).
    pub ms: u64,
    pub writer: Actor,
    /// The stamp of an earlier announcement still pending when this one replaced it: its PUT may yet
    /// land, so a fenced merge waits until that stamp is no longer live either. A publish is never
    /// blocked by it, only merges, and only for the TTL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held_ms: Option<u64>,
    /// Set by the publisher's post-check once its conditional manifest write succeeded (WO-069 lane
    /// D3): never pending again. The write that sets it is what makes a merger that read the record
    /// earlier fail its own conditional write, and it can never bring the record back to bytes a
    /// merger read before the announcement (the digest makes it unique).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub done: bool,
}

/// The size ceilings of one shared worktree tree. This is the ONE place the numbers live: the
/// writer (`publish_tree`) takes a `Limits`, and every reader that applies a file-count cap
/// (`read_manifest_with`, `parse_manifest_with`, `fetch_tree_with`) takes the same `Limits`, so a
/// tree published under some limits can be read back under those limits. The plain readers
/// (`read_manifest`, `fetch_tree`) use `Limits::default()`, which is what the daemon publishes with.
///
/// Owner decision 2026-11-15 (ADR-126 section 4a): 100 000 files and 4 GiB per worktree. The
/// 16 MiB per-file and 256 MiB bundle caps are unchanged. The dependent caps are derived from these
/// and pinned by tests: `MAX_MANIFEST_BYTES` (`share_publish.rs`) and
/// `retention::DEFAULT_WORKTREE_QUOTA`.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_file: u64,
    pub max_files: usize,
    pub max_total: u64,
    pub max_bundle: u64,
}
impl Limits {
    /// Largest single file a publish takes.
    pub const DEFAULT_MAX_FILE: u64 = 16 << 20;
    /// Most files a tree may list (also the most a reader accepts in a manifest).
    pub const DEFAULT_MAX_FILES: usize = 100_000;
    /// Most bytes of file content in one tree.
    pub const DEFAULT_MAX_TOTAL: u64 = 4 << 30;
    /// Largest git bundle a publish takes.
    pub const DEFAULT_MAX_BUNDLE: u64 = 256 << 20;
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file: Self::DEFAULT_MAX_FILE,
            max_files: Self::DEFAULT_MAX_FILES,
            max_total: Self::DEFAULT_MAX_TOTAL,
            max_bundle: Self::DEFAULT_MAX_BUNDLE,
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
    /// The caller computed `changes`, so an empty list means "changed nothing" (finding 6f). A
    /// non-empty list counts as recorded either way.
    pub changes_recorded: bool,
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
    /// The path changed between the checks and the read (swapped for a symlink or another file).
    Changed,
}

#[derive(Debug, Default)]
pub struct PublishReport {
    pub uploaded: usize,
    pub unchanged: usize,
    /// Objects this publish deleted (schema 2: past their grace; schema 1: stale tree objects too).
    pub removed: usize,
    /// Pool objects this publish found unreferenced for the first time (now in `retiring`).
    pub retired: usize,
    /// Pool objects still inside their grace after this publish (left for a later sweep).
    pub deferred: usize,
    pub skipped: Vec<(String, Skip)>,
    pub bundle_uploaded: bool,
}

pub fn sha256_hex(data: &[u8]) -> String {
    let d = Sha256::digest(data);
    d.iter().map(|b| format!("{b:02x}")).collect()
}

/// Files that must never be shared even when git can see them.
pub fn is_secret_path(path: &str) -> bool {
    let lower = path.to_ascii_lowercase();
    let mut segs: Vec<&str> = lower.split('/').collect();
    let name = segs.pop().unwrap_or("").to_string();
    // Credential directories, wherever they sit in the tree: a kube config and its caches, the
    // docker client config, and the daemon's own per-store keys (`store-secrets/wk-*`).
    if segs.iter().any(|s| *s == ".kube" || *s == "store-secrets")
        || (name == "config.json" && segs.last() == Some(&".docker"))
    {
        return true;
    }
    let name = name.as_str();
    let env_template = [".example", ".sample", ".template"]
        .iter()
        .any(|t| name.ends_with(t));
    (!env_template && (name == ".env" || name.starts_with(".env.")))
        || name.ends_with(".env")
        // backups and copies of env files (`prod.env.bak`); the template suffixes stay shareable
        || (!env_template && name.contains(".env."))
        || name.ends_with(".tfvars")
        || name == ".pypirc"
        || name.ends_with(".token")
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

/// Path segments that must never be written by a fetch: VCS control dirs and files that make a
/// later git/shell command in the destination run attacker-chosen code.
pub fn is_dangerous_path(rel: &str) -> bool {
    rel.split('/').any(|seg| {
        let s = seg.to_ascii_lowercase();
        matches!(
            s.as_str(),
            ".git" | ".gitattributes" | ".gitmodules" | ".hg" | ".svn" | ".envrc" | ".wt"
        ) || seg.starts_with(".wt-") // the follower's own control files (`.wt-index.json`, temp files)
            || seg.contains(':')
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

#[cfg(test)]
#[path = "share_read_tests.rs"]
mod read_tests;
