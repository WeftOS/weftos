//! Project-scoped apply of a team or an agent package (ADR-112 TM2): the one
//! code path behind `weftos team apply|status|uninstall` and the daemon's
//! `agent.install` action. It is `weftos init`'s own plan and apply, run with
//! a pinned digest, a collision check that refuses before anything is written,
//! a birth report, and an uninstall that removes exactly what the lock lists.
//!
//! Writes stay inside the project root; nothing in `$HOME` is touched. Inactive
//! team members are never applied. Drifted files and files WeftOS did not write
//! (including another package's) are refused, never overwritten.

use std::collections::BTreeSet;
use std::fmt;
use std::path::PathBuf;

use serde::Serialize;

use super::apply::{apply, read_text};
use super::birth::{Birth, birth, detect};
use super::digest::{Subject, bundle_digest};
use super::lock::{LOCK_PATH, Lock, sha256_hex};
use super::package::Package;
use super::plan::{Action, AgentInitOptions, Plan, plan};
use super::render::{Host, INDEX_OWNER, Merge};
use super::source::AgentSource;

/// Most paths named in a refusal message.
const MAX_LISTED: usize = 10;

#[derive(Clone, Debug)]
pub struct ApplyRequest {
    pub root: PathBuf,
    pub subject: Subject,
    pub name: String,
    pub hosts: Vec<Host>,
    /// An `agents/` directory on disk instead of the embedded tree.
    pub from: Option<PathBuf>,
    /// Pinned bundle digest (hex, no prefix); a mismatch writes nothing.
    pub pin: Option<String>,
    /// Plan and report; write nothing.
    pub dry_run: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Disposition {
    Copied,
    SkippedIdentical,
    SkippedDivergent,
}

impl Disposition {
    pub fn label(self) -> &'static str {
        match self {
            Disposition::Copied => "copied",
            Disposition::SkippedIdentical => "skipped-identical",
            Disposition::SkippedDivergent => "skipped-divergent",
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Collision {
    pub path: String,
    pub owner: String,
    pub disposition: Disposition,
}

/// A request that was refused on purpose, or an operational failure.
#[derive(Debug)]
pub struct TeamError {
    pub refused: bool,
    pub message: String,
    /// Per-path report; non-empty when files in the project are why.
    pub collisions: Vec<Collision>,
}

impl fmt::Display for TeamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TeamError {}

fn refuse(m: impl Into<String>) -> TeamError {
    TeamError { refused: true, message: m.into(), collisions: Vec::new() }
}

fn fail(m: impl Into<String>) -> TeamError {
    TeamError { refused: false, message: m.into(), collisions: Vec::new() }
}

fn load(from: &Option<PathBuf>) -> Result<AgentSource, TeamError> {
    match from {
        Some(d) => AgentSource::from_dir(d).map_err(|e| fail(format!("cannot read the agent packages: {e}"))),
        None => Ok(AgentSource::embedded()),
    }
}

/// The per-path disposition of a plan (the context file is not a collision subject).
pub fn collisions(plan: &Plan) -> Vec<Collision> {
    plan.items
        .iter()
        .filter(|i| i.host.is_some())
        .filter_map(|i| {
            let disposition = match i.action {
                Action::Create | Action::Update | Action::Overwrite => Disposition::Copied,
                Action::Unchanged => Disposition::SkippedIdentical,
                Action::Drifted | Action::Unmanaged => Disposition::SkippedDivergent,
                _ => return None,
            };
            Some(Collision { path: i.path.clone(), owner: i.owner.clone(), disposition })
        })
        .collect()
}

#[derive(Debug)]
pub struct Applied {
    pub plan: Plan,
    /// Paths written or deleted (0 on a dry run).
    pub touched: usize,
    /// True when the project already matched: nothing needed writing.
    pub already: bool,
    pub digest: String,
    pub collisions: Vec<Collision>,
    pub birth: Birth,
}

/// Verify the pin, refuse collisions, write, then verify by re-planning.
pub fn apply_request(req: &ApplyRequest) -> Result<Applied, TeamError> {
    if req.hosts.is_empty() {
        return Err(refuse("name at least one host: claude, grok or codex"));
    }
    if !req.root.is_dir() {
        return Err(refuse("the project directory does not exist"));
    }
    let src = load(&req.from)?;
    let digest = bundle_digest(&src, req.subject, &req.name)
        .map_err(|e| refuse(format!("this node cannot install {}: {e}", req.name)))?;
    if let Some(pin) = &req.pin
        && *pin != digest
    {
        return Err(refuse(format!(
            "digest mismatch: this node has {} at {} but the request pins {}; nothing was written",
            req.name,
            &digest[..12],
            &pin[..pin.len().min(12)]
        )));
    }

    let mut opts = AgentInitOptions {
        root: req.root.clone(),
        hosts: req.hosts.clone(),
        from: req.from.clone(),
        ..Default::default()
    };
    match req.subject {
        Subject::Agent => opts.agents = vec![req.name.clone()],
        Subject::Team => opts.team = Some(req.name.clone()),
    }
    let first = plan(&opts).map_err(|e| fail(format!("planning the install failed: {e}")))?;
    let report = collisions(&first);
    let held: Vec<&str> = report
        .iter()
        .filter(|c| c.disposition == Disposition::SkippedDivergent)
        .map(|c| c.path.as_str())
        .collect();
    if !held.is_empty() {
        let shown: Vec<&str> = held.iter().take(MAX_LISTED).copied().collect();
        let mut e = refuse(format!(
            "{} file(s) in the project were changed locally or are not WeftOS's; nothing was written: {}",
            held.len(),
            shown.join(", ")
        ));
        e.collisions = report;
        return Err(e);
    }
    let already = first.is_noop();
    let touched = if already || req.dry_run {
        0
    } else {
        apply(&first).map_err(|e| fail(format!("writing the install failed: {e}")))?
    };
    if !req.dry_run {
        let after = plan(&opts).map_err(|e| fail(format!("verifying the install failed: {e}")))?;
        if !after.is_noop() || after.items.iter().any(|i| matches!(i.action, Action::Drifted | Action::Unmanaged)) {
            return Err(fail("the installed files do not match the render; the install is not trusted"));
        }
    }
    let packages = first
        .selected
        .iter()
        .map(|id| Package::load(&src, id))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| fail(format!("reading the packages failed: {e}")))?;
    let hosts: Vec<&str> = req.hosts.iter().map(|h| h.as_str()).collect();
    let born = birth(&first, &packages, &hosts, &detect(&req.root));
    Ok(Applied { plan: first, touched, already, digest, collisions: report, birth: born })
}

#[derive(Debug)]
pub struct Uninstalled {
    pub plan: Plan,
    pub touched: usize,
    /// Package ids taken out.
    pub removed: Vec<String>,
    /// Paths left in place because they changed since apply (released from the lock).
    pub kept: Vec<String>,
}

fn read_lock(root: &std::path::Path) -> Result<Lock, TeamError> {
    Lock::read(root)
        .map_err(|e| fail(format!("cannot read {LOCK_PATH}: {e}")))?
        .ok_or_else(|| refuse(format!("nothing applied here: {LOCK_PATH} does not exist")))
}

fn lock_hosts(lock: &Lock) -> Vec<Host> {
    let set: BTreeSet<Host> = lock.agents.iter().flat_map(|a| a.files.iter().map(|f| f.host)).collect();
    set.into_iter().collect()
}

/// Remove one team (packages no other applied team or standalone install
/// needs), or everything the lock lists when `team` is `None`. Files changed
/// since apply stay and are reported; the project context is removed only while
/// it is exactly what apply seeded.
pub fn uninstall(
    root: &std::path::Path,
    team: Option<&str>,
    from: &Option<PathBuf>,
    dry_run: bool,
) -> Result<Uninstalled, TeamError> {
    let lock = read_lock(root)?;
    let in_lock: BTreeSet<String> = lock.agents.iter().map(|a| a.id.clone()).collect();
    let active = |t: &super::lock::LockTeam| -> BTreeSet<String> {
        t.slots.iter().filter(|s| s.active).map(|s| s.agent.clone()).collect()
    };
    let (remove, drop_teams): (BTreeSet<String>, Vec<String>) = match team {
        None => (in_lock.clone(), lock.teams.iter().map(|t| t.name.clone()).collect()),
        Some(name) => {
            let t = lock
                .teams
                .iter()
                .find(|t| t.name == name)
                .ok_or_else(|| refuse(format!("team '{name}' is not applied here")))?;
            let others: BTreeSet<String> =
                lock.teams.iter().filter(|o| o.name != name).flat_map(&active).collect();
            let rm = active(t).into_iter().filter(|a| in_lock.contains(a) && !others.contains(a)).collect();
            (rm, vec![name.to_string()])
        }
    };
    let src = load(from)?;
    let known = src.package_ids();
    if let Some(gone) = in_lock.iter().find(|a| !remove.contains(*a) && a.as_str() != INDEX_OWNER && !known.contains(a)) {
        return Err(refuse(format!("'{gone}' stays installed but is not in the source; pass --from the tree it came from")));
    }
    let opts = AgentInitOptions {
        root: root.to_path_buf(),
        hosts: lock_hosts(&lock),
        from: from.clone(),
        remove: remove.iter().cloned().collect(),
        drop_teams,
        ..Default::default()
    };
    let p = plan(&opts).map_err(|e| fail(format!("planning the uninstall failed: {e}")))?;
    if let Some(i) = p.items.iter().find(|i| {
        matches!(i.action, Action::Create | Action::Update | Action::Overwrite) && i.merge == Merge::File
    }) {
        return Err(refuse(format!(
            "{} would change for a package that stays; re-apply that package first",
            i.path
        )));
    }
    let kept: Vec<String> = p.items.iter().filter(|i| i.action == Action::Release).map(|i| i.path.clone()).collect();
    let touched = if dry_run {
        0
    } else {
        apply(&p).map_err(|e| fail(format!("writing the uninstall failed: {e}")))?
    };
    Ok(Uninstalled { plan: p, touched, removed: remove.into_iter().filter(|a| a != INDEX_OWNER).collect(), kept })
}

#[derive(Debug, Serialize)]
pub struct AgentStatus {
    pub id: String,
    pub version: String,
    pub digest: String,
    pub files: usize,
    pub modified: Vec<String>,
    pub missing: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct TeamStatus {
    pub name: String,
    pub version: String,
    pub digest: String,
    pub hosts: Vec<String>,
    pub slots: Vec<String>,
    /// Digest of the source's current bundle, when the source has this team.
    pub source_digest: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Status {
    pub teams: Vec<TeamStatus>,
    pub agents: Vec<AgentStatus>,
    pub context: Option<String>,
}

/// What the lock says is applied and whether the disk still agrees.
pub fn status(root: &std::path::Path, from: &Option<PathBuf>) -> Result<Status, TeamError> {
    let lock = read_lock(root)?;
    let src = load(from).ok();
    let teams = lock
        .teams
        .iter()
        .map(|t| TeamStatus {
            name: t.name.clone(),
            version: t.version.clone(),
            digest: t.digest.clone(),
            hosts: t.hosts.iter().map(|h| h.as_str().to_string()).collect(),
            slots: t
                .slots
                .iter()
                .map(|s| format!("{}{}", s.agent, if s.active { "" } else { " (inactive)" }))
                .collect(),
            source_digest: src.as_ref().and_then(|s| bundle_digest(s, Subject::Team, &t.name).ok()),
        })
        .collect();
    let mut agents = Vec::new();
    for a in &lock.agents {
        let (mut modified, mut missing) = (Vec::new(), Vec::new());
        for f in &a.files {
            let text = match f.merge {
                Merge::File => std::fs::read(root.join(&f.path)).ok(),
                Merge::Block => read_text(&root.join(&f.path))
                    .ok()
                    .flatten()
                    .and_then(|t| super::apply::extract_block(&t).map(|b| b.as_bytes().to_vec())),
            };
            match text {
                None => missing.push(f.path.clone()),
                Some(b) if sha256_hex(&b) != f.sha256 => modified.push(f.path.clone()),
                Some(_) => {}
            }
        }
        agents.push(AgentStatus {
            id: a.id.clone(),
            version: a.version.clone(),
            digest: a.digest.clone(),
            files: a.files.len(),
            modified,
            missing,
        });
    }
    Ok(Status { teams, agents, context: lock.context.map(|c| c.path) })
}

pub fn format_status(s: &Status) -> String {
    let mut out = String::new();
    for t in &s.teams {
        let moved = match &t.source_digest {
            Some(d) if *d != t.digest => "  (source has moved on; re-apply to update)",
            _ => "",
        };
        out.push_str(&format!(
            "team {} {} sha256:{}{moved}\n  hosts: {}\n  slots: {}\n",
            t.name, t.version, t.digest, t.hosts.join(", "), t.slots.join(", ")
        ));
    }
    for a in &s.agents {
        let state = if a.modified.is_empty() && a.missing.is_empty() {
            "clean".to_string()
        } else {
            format!("{} modified, {} missing", a.modified.len(), a.missing.len())
        };
        out.push_str(&format!("  {:<18} {} {} file(s) {state}\n", a.id, a.version, a.files));
        for p in a.modified.iter().chain(&a.missing) {
            out.push_str(&format!("    ! {p}\n"));
        }
    }
    if let Some(c) = &s.context {
        out.push_str(&format!("project context: {c} (seeded; yours to edit)\n"));
    }
    out
}

pub fn format_collisions(c: &[Collision]) -> String {
    let mut s = String::new();
    for x in c {
        s.push_str(&format!("  {:<18} {}  [{}]\n", x.disposition.label(), x.path, x.owner));
    }
    s
}

#[cfg(test)]
#[path = "team_tests.rs"]
mod tests;
