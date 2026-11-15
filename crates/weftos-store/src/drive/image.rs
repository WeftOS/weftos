//! The ADR-131 §4 attribute profile for a **root-image layer** (a Linux rootfs a sandboxed
//! "computer" boots from). The project-tree profile (`index.rs`, `scan.rs`) is untouched; this is a
//! separate profile with its own rules:
//!
//! | Attribute | Image rule |
//! |---|---|
//! | Symlinks | Carried as link text. Absolute links are kept (they mean "inside the image root" once the sandbox pivots into it); a relative link that would climb above the root is rewritten to its clamped form. Nothing is ever resolved on the host's own root. |
//! | Mode | Permission bits kept (`0o777`, plus sticky). setuid/setgid are **stripped by default** ([`SetidRule::Strip`]); [`SetidRule::Keep`] is the explicit opt-in. |
//! | Hard links | Preserved as links (the second path is a [`ImageKind::HardLink`] to the first). |
//! | Owner/group | Never carried or applied: every file belongs to the materialising (sandbox) user. No chown, so no privilege and no `libc`. |
//! | Devices, FIFOs, sockets | Skipped with a reason; `bwrap --dev` supplies `/dev`. |
//!
//! Std only (`std::os::unix::fs`). Content is copied from a source root; hashing into the CAS is a
//! follow-up (the card's full-rootfs round trip).

use std::collections::HashMap;
use std::io;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

/// What to do with setuid (04000) and setgid (02000) bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum SetidRule {
    /// Clear both (the default).
    #[default]
    Strip,
    /// Keep them, an explicit choice per image.
    Keep,
}

/// The image profile's knobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct ImageProfile {
    pub setid: SetidRule,
}

/// One entry of a captured image layer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageKind {
    Dir,
    File { size: u64 },
    Symlink { target: String },
    /// A second name for the file at `to` (an earlier path in the same layer).
    HardLink { to: String },
    /// A device node, FIFO or socket: not carried.
    Skipped { reason: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageEntry {
    /// Path relative to the image root, `/`-separated, no leading slash.
    pub path: String,
    /// Permission bits after the profile's setid rule.
    pub mode: u32,
    pub kind: ImageKind,
}

/// The mode bits the image profile keeps.
pub fn image_mode(mode: u32, rule: SetidRule) -> u32 {
    let keep = if rule == SetidRule::Keep { 0o7777 } else { 0o1777 };
    mode & keep
}

/// Resolve `target` (a symlink at `link_rel`, relative to the image root) as the sandbox would,
/// with the image root as `/`: an absolute target is root-relative, `..` at the root stays at the
/// root. Returns the root-relative, `/`-separated result (empty string is the root itself).
pub fn confine_link(link_rel: &str, target: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if !target.starts_with('/') {
        parts.extend(link_rel.split('/').filter(|s| !s.is_empty()));
        parts.pop(); // the link's own directory
    }
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// The text a symlink gets on materialise: absolute stays as is; a relative target that climbs
/// above the root is replaced by its clamped, absolute (in-image) form.
fn link_text(link_rel: &str, target: &str) -> String {
    if target.starts_with('/') {
        return target.to_owned();
    }
    let depth = link_rel.split('/').filter(|s| !s.is_empty()).count().saturating_sub(1);
    let mut level = depth as i64;
    for seg in target.split('/') {
        match seg {
            "" | "." => {}
            ".." => level -= 1,
            _ => level += 1,
        }
        if level < 0 {
            return format!("/{}", confine_link(link_rel, target));
        }
    }
    target.to_owned()
}

/// Walk `root` (never following symlinks) and describe it under the image profile.
pub fn capture(root: &Path, profile: &ImageProfile) -> io::Result<Vec<ImageEntry>> {
    let mut out = Vec::new();
    let mut seen: HashMap<(u64, u64), String> = HashMap::new();
    walk(root, "", profile, &mut seen, &mut out)?;
    Ok(out)
}

fn walk(dir: &Path, rel: &str, p: &ImageProfile, seen: &mut HashMap<(u64, u64), String>, out: &mut Vec<ImageEntry>) -> io::Result<()> {
    let mut names: Vec<_> = std::fs::read_dir(dir)?.collect::<io::Result<Vec<_>>>()?;
    names.sort_by_key(|e| e.file_name());
    for e in names {
        let name = e.file_name().to_string_lossy().into_owned();
        let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        let md = std::fs::symlink_metadata(e.path())?;
        let ft = md.file_type();
        let mode = image_mode(md.mode(), p.setid);
        if ft.is_symlink() {
            let target = std::fs::read_link(e.path())?.to_string_lossy().into_owned();
            out.push(ImageEntry { path, mode: 0o777, kind: ImageKind::Symlink { target } });
        } else if ft.is_dir() {
            out.push(ImageEntry { path: path.clone(), mode, kind: ImageKind::Dir });
            walk(&e.path(), &path, p, seen, out)?;
        } else if ft.is_file() {
            let key = (md.dev(), md.ino());
            if md.nlink() > 1 {
                if let Some(first) = seen.get(&key) {
                    out.push(ImageEntry { path, mode, kind: ImageKind::HardLink { to: first.clone() } });
                    continue;
                }
                seen.insert(key, path.clone());
            }
            out.push(ImageEntry { path, mode, kind: ImageKind::File { size: md.len() } });
        } else {
            let reason = "device node, FIFO or socket (the sandbox supplies /dev)".to_owned();
            out.push(ImageEntry { path, mode: 0, kind: ImageKind::Skipped { reason } });
        }
    }
    Ok(())
}

/// What `materialise` did.
#[derive(Debug, Default)]
pub struct ImageReport {
    pub placed: usize,
    pub skipped: Vec<(String, String)>,
}

fn bad(path: &str, why: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, format!("{path}: {why}"))
}

/// A manifest path must be relative with no `.`/`..`/root components.
fn check_rel(rel: &str) -> io::Result<PathBuf> {
    let p = Path::new(rel);
    if rel.is_empty() || !p.components().all(|c| matches!(c, Component::Normal(_))) {
        return Err(bad(rel, "not a plain relative path"));
    }
    Ok(p.to_path_buf())
}

/// Refuse to write through a symlink or non-directory among `rel`'s parents inside `dest`.
fn check_parents(dest: &Path, rel: &Path) -> io::Result<()> {
    let mut cur = dest.to_path_buf();
    if let Some(parent) = rel.parent() {
        for c in parent.components() {
            cur.push(c);
            let md = std::fs::symlink_metadata(&cur)?;
            if !md.file_type().is_dir() {
                return Err(bad(&rel.to_string_lossy(), "a parent is a symlink or not a directory"));
            }
        }
    }
    Ok(())
}

/// Lay `entries` (from [`capture`] of `src_root`) down under `dest`.
pub fn materialise(src_root: &Path, entries: &[ImageEntry], dest: &Path, profile: &ImageProfile) -> io::Result<ImageReport> {
    let mut rep = ImageReport::default();
    std::fs::create_dir_all(dest)?;
    for e in entries {
        let rel = check_rel(&e.path)?;
        let target = dest.join(&rel);
        match &e.kind {
            ImageKind::Skipped { reason } => {
                rep.skipped.push((e.path.clone(), reason.clone()));
                continue;
            }
            ImageKind::Dir => {
                check_parents(dest, &rel)?;
                if !target.exists() {
                    std::fs::create_dir(&target)?;
                }
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(image_mode(e.mode, profile.setid) | 0o700))?;
            }
            ImageKind::File { .. } => {
                check_parents(dest, &rel)?;
                std::fs::copy(src_root.join(&rel), &target)?;
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(image_mode(e.mode, profile.setid)))?;
            }
            ImageKind::Symlink { target: t } => {
                check_parents(dest, &rel)?;
                symlink(link_text(&e.path, t), &target)?;
            }
            ImageKind::HardLink { to } => {
                check_parents(dest, &rel)?;
                let to = check_rel(to)?;
                std::fs::hard_link(dest.join(to), &target)?;
            }
        }
        rep.placed += 1;
    }
    Ok(rep)
}
